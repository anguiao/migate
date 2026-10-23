use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Instant,
};

use event_listener::Event;

use super::{
    LightTarget,
    model::{Command, CommandError, FunctionSnapshot},
    state::{Function, RuntimeState},
};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum LightDimension {
    Brightness,
    Color,
}
impl LightDimension {
    fn index(self) -> usize {
        match self {
            Self::Brightness => 0,
            Self::Color => 1,
        }
    }
}

/// An action belongs to one function instance, including across identity reuse.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ActionToken {
    owner: u64,
    serial: u64,
    dimension: LightDimension,
}
impl ActionToken {
    pub fn dimension(self) -> LightDimension {
        self.dimension
    }
}

struct Action {
    token: ActionToken,
    accepted: Arc<Completion>,
    ended: Arc<Completion>,
    accepted_while_current: AtomicBool,
    startup_sent: AtomicBool,
    final_submitted: AtomicBool,
    allow_unchanged_startup: bool,
}

/// Startup acceptance and action termination have independent lifetimes.
pub struct LightAction(Arc<Action>);
impl LightAction {
    pub fn token(&self) -> ActionToken {
        self.0.token
    }
    pub async fn accepted(&self) -> Result<FunctionSnapshot, CommandError> {
        self.0.accepted.wait().await
    }
    pub async fn ended(&self) -> Result<FunctionSnapshot, CommandError> {
        self.0.ended.wait().await
    }
    /// Whether startup succeeded while this action still owned its dimension.
    pub(crate) fn accepted_while_current(&self) -> bool {
        self.0.accepted_while_current.load(Ordering::Relaxed)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LightActionResult {
    pub token: ActionToken,
    pub result: Result<(), CommandError>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LightActionDiagnostics {
    pub brightness: Option<ActionToken>,
    pub color: Option<ActionToken>,
    /// A cancelled action can still own a sent startup receipt.
    pub awaiting_startup: Option<ActionToken>,
    pub last: Option<LightActionResult>,
    pub revision: u64,
}

struct PowerCommand {
    command: Command,
    completion: Arc<Completion>,
    order: u64,
    queued_at: Instant,
}
struct LightFrame {
    action: Arc<Action>,
    target: LightTarget,
    final_frame: bool,
    order: u64,
    queued_at: Instant,
}
struct PowerFlight {
    target: bool,
    completion: Arc<Completion>,
    sent: AtomicBool,
    invalid: Mutex<Option<CommandError>>,
}
struct LightFlight {
    action: Arc<Action>,
    power: Option<bool>,
    gate: Arc<Completion>,
    sent: AtomicBool,
}
/// One function owns at most one transport operation; the original receipt
/// keeps that place until it settles, an unsent frame may be retired early.
enum Flight {
    Power(Arc<PowerFlight>),
    Light(Arc<LightFlight>),
}
impl Flight {
    fn gate(&self) -> Arc<Completion> {
        match self {
            Self::Power(flight) => flight.completion.clone(),
            Self::Light(flight) => flight.gate.clone(),
        }
    }
}

/// The single command owner inside a function: power FIFO, one merged pending
/// frame per light dimension, the overall order and the in-flight operation.
pub(super) struct CommandCoordinator {
    owner: u64,
    serial: u64,
    order: u64,
    power: VecDeque<PowerCommand>,
    light_current: [Option<Arc<Action>>; 2],
    // Final acceptance releases the action but not its remaining owned writes.
    light_write_serials: [u64; 2],
    light_pending: [Option<LightFrame>; 2],
    flight: Option<Arc<Flight>>,
    last_light: Option<LightActionResult>,
    light_revision: u64,
    closed: bool,
}
impl CommandCoordinator {
    pub(super) fn new() -> Self {
        static NEXT_OWNER: AtomicU64 = AtomicU64::new(1);
        Self {
            owner: NEXT_OWNER.fetch_add(1, Ordering::Relaxed),
            serial: 0,
            order: 0,
            power: VecDeque::new(),
            light_current: [None, None],
            light_write_serials: [0, 0],
            light_pending: [None, None],
            flight: None,
            last_light: None,
            light_revision: 0,
            closed: false,
        }
    }
    pub(super) fn is_closed(&self) -> bool {
        self.closed
    }
    /// Function shutdown: closing and cancellation are one lifecycle operation
    /// so callers cannot forget the light or outlet half.
    pub(super) fn close(&mut self, light: bool, error: CommandError) {
        self.closed = true;
        if light {
            self.cancel_pending_light(error);
        } else {
            self.drain_all_power(error);
        }
    }
    /// Losing availability ends queued work and unsent flights; an original
    /// sent receipt keeps its gate until it settles.
    pub(super) fn unavailable(&mut self, light: bool, error: CommandError) {
        if light {
            self.cancel_pending_light(error);
        } else {
            self.drain_pending_power(error);
        }
    }
    /// One power submission owns FIFO placement, Off takeover and order.
    fn submit_power(&mut self, command: Command, light: bool, completion: Arc<Completion>) {
        if light && command == Command::Off {
            self.cancel_all(CommandError::Superseded);
            self.invalidate_light_on();
        }
        let order = if light { self.next_order() } else { 0 };
        self.power.push_back(PowerCommand {
            command,
            completion,
            order,
            queued_at: Instant::now(),
        });
    }
    fn next_order(&mut self) -> u64 {
        self.order += 1;
        self.order
    }
    fn light_action(&self, token: ActionToken) -> Option<&Arc<Action>> {
        self.light_current[token.dimension.index()]
            .as_ref()
            .filter(|action| action.token == token)
    }
    pub(super) fn light_action_current(&self, token: ActionToken) -> bool {
        self.light_action(token).is_some()
    }
    /// A completed final frame can finish its remaining writes until takeover or Stop.
    pub(super) fn light_action_owns_writes(&self, token: ActionToken) -> bool {
        self.owner == token.owner
            && self.light_write_serials[token.dimension.index()] == token.serial
    }
    pub(super) fn light_diagnostics(&self) -> LightActionDiagnostics {
        LightActionDiagnostics {
            brightness: self.light_current[0].as_ref().map(|action| action.token),
            color: self.light_current[1].as_ref().map(|action| action.token),
            awaiting_startup: self.flight.as_ref().and_then(|flight| match &**flight {
                Flight::Light(flight)
                    if flight.sent.load(Ordering::Relaxed)
                        && !flight.action.accepted.is_finished() =>
                {
                    Some(flight.action.token)
                }
                _ => None,
            }),
            last: self.last_light.clone(),
            revision: self.light_revision,
        }
    }
    fn record_light(&mut self, token: ActionToken, result: Result<(), CommandError>) {
        self.last_light = Some(LightActionResult { token, result });
        self.light_revision += 1;
    }
    /// One logical action begins here: takeover, token, ownership and the
    /// first merged frame are coordinated in a single operation.
    pub(super) fn begin_light(
        &mut self,
        target: LightTarget,
        final_frame: bool,
        allow_unchanged_startup: bool,
    ) -> LightAction {
        let dimension = target.dimension();
        let power = target.power();
        if power == Some(false) {
            self.cancel_all(CommandError::Superseded);
        } else {
            self.cancel_light(dimension, CommandError::Superseded);
        }
        self.serial += 1;
        let action = Arc::new(Action {
            token: ActionToken {
                owner: self.owner,
                serial: self.serial,
                dimension,
            },
            accepted: Arc::new(Completion::default()),
            ended: Arc::new(Completion::default()),
            accepted_while_current: AtomicBool::new(false),
            startup_sent: AtomicBool::new(false),
            final_submitted: AtomicBool::new(final_frame),
            allow_unchanged_startup,
        });
        self.light_write_serials[dimension.index()] = action.token.serial;
        self.light_current[dimension.index()] = Some(action.clone());
        self.enqueue_light(action.clone(), target, final_frame);
        self.light_revision += 1;
        if power == Some(false) {
            self.invalidate_light_on();
        }
        LightAction(action)
    }
    /// Apply one validated frame to an existing action.
    pub(super) fn submit_light_frame(
        &mut self,
        token: ActionToken,
        target: LightTarget,
        final_frame: bool,
    ) -> Result<(), CommandError> {
        let action = self
            .light_action(token)
            .cloned()
            .ok_or(CommandError::Superseded)?;
        if action.final_submitted.load(Ordering::Relaxed) {
            return Err(CommandError::Rejected(
                "action already has its final frame".into(),
            ));
        }
        action.final_submitted.store(final_frame, Ordering::Relaxed);
        let cancels_power = target.power() == Some(false);
        if cancels_power {
            let other = match token.dimension {
                LightDimension::Brightness => LightDimension::Color,
                LightDimension::Color => LightDimension::Brightness,
            };
            self.cancel_light(other, CommandError::Superseded);
        }
        self.enqueue_light(action, target, final_frame);
        if cancels_power {
            self.invalidate_light_on();
        }
        Ok(())
    }
    pub(super) fn cancel_light(&mut self, dimension: LightDimension, error: CommandError) {
        let index = dimension.index();
        self.light_write_serials[index] = 0;
        self.light_pending[index] = None;
        if let Some(action) = self.light_current[index].take() {
            action.ended.finish(Err(error.clone()));
            if !action.startup_sent.load(Ordering::Relaxed) {
                action.accepted.finish(Err(error.clone()));
            }
            if let Some(flight) = &self.flight
                && let Flight::Light(flight) = &**flight
                && flight.action.token == action.token
                && !flight.sent.load(Ordering::Relaxed)
            {
                flight.gate.finish(Err(error.clone()));
            }
            self.record_light(action.token, Err(error));
        }
    }
    fn cancel_all(&mut self, error: CommandError) {
        self.cancel_light(LightDimension::Brightness, error.clone());
        self.cancel_light(LightDimension::Color, error);
    }
    /// Cancel light actions, revoke every unsent power wait and drop queued
    /// power commands. An original sent receipt keeps its gate.
    pub(super) fn cancel_pending_light(&mut self, error: CommandError) {
        self.cancel_all(error.clone());
        self.invalidate_active_power(error.clone());
        self.drain_pending_power(error);
    }
    fn invalidate_light_on(&mut self) {
        self.power.retain(|pending| {
            if pending.command == Command::On {
                pending.completion.finish(Err(CommandError::Superseded));
                false
            } else {
                true
            }
        });
        if let Some(flight) = &self.flight
            && let Flight::Power(flight) = &**flight
            && flight.target
            && !flight.sent.load(Ordering::Relaxed)
        {
            *flight.invalid.lock().unwrap() = Some(CommandError::Superseded);
            flight.completion.finish(Err(CommandError::Superseded));
        }
    }
    fn invalidate_active_power(&mut self, error: CommandError) {
        if let Some(flight) = &self.flight
            && let Flight::Power(flight) = &**flight
            && !flight.sent.load(Ordering::Relaxed)
        {
            *flight.invalid.lock().unwrap() = Some(error.clone());
            flight.completion.finish(Err(error));
        }
    }
    /// End queued power commands without touching the active request.
    fn drain_pending_power(&mut self, error: CommandError) {
        for pending in self.power.drain(..) {
            pending.completion.finish(Err(error.clone()));
        }
    }
    /// End every queued and active power command; used when a function closes.
    fn drain_all_power(&mut self, error: CommandError) {
        self.drain_pending_power(error.clone());
        if let Some(flight) = self.flight.take()
            && let Flight::Power(flight) = &*flight
        {
            flight.completion.finish(Err(error));
        }
    }
    fn enqueue_light(&mut self, action: Arc<Action>, mut target: LightTarget, final_frame: bool) {
        let index = action.token.dimension.index();
        if let Some(pending) = &self.light_pending[index] {
            target = target.inherit_power(pending.target.power());
        }
        if let Some(flight) = &self.flight
            && let Flight::Light(flight) = &**flight
            && flight.action.token == action.token
            && !flight.sent.load(Ordering::Relaxed)
        {
            target = target.inherit_power(flight.power);
            // Replacing an unsent tick releases only its frame, never the logical action.
            flight.gate.finish(Err(CommandError::Superseded));
        }
        // Repeated ticks retain their place relative to the other dimension.
        let order = self.light_pending[index]
            .as_ref()
            .map(|pending| pending.order)
            .unwrap_or_else(|| self.next_order());
        self.light_pending[index] = Some(LightFrame {
            action,
            target,
            final_frame,
            order,
            queued_at: Instant::now(),
        });
    }
    fn first_light_pending(&self) -> Option<(usize, u64)> {
        self.light_pending
            .iter()
            .enumerate()
            .filter_map(|(index, frame)| frame.as_ref().map(|frame| (index, frame.order)))
            .min_by_key(|(_, order)| *order)
    }
}

impl Function {
    /// Completion means the downstream adapter accepted the command, not that
    /// the physical action has been independently confirmed.
    /// Dropping the future withdraws a queued command. An adapter that already
    /// received it still owns its result and may update the shared snapshot.
    pub async fn execute(&self, command: Command) -> Result<FunctionSnapshot, CommandError> {
        let completion = Arc::new(Completion::default());
        let light = self.description().kind.is_light();
        let resolved = {
            let mut state = self.0.state.lock().unwrap();
            let Some(commands) = state.commands() else {
                return Err(CommandError::Unsupported);
            };
            if commands.is_closed() {
                return Err(CommandError::Cancelled);
            }
            self.snapshot_locked(&state).availability.command_result()?;
            let resolved = if light {
                match command {
                    Command::On => Command::On,
                    Command::Off => Command::Off,
                    Command::Toggle => {
                        let value = state
                            .power()
                            .and_then(super::Power::value)
                            .ok_or(CommandError::UnknownState)?;
                        if value { Command::Off } else { Command::On }
                    }
                }
            } else {
                command
            };
            state
                .commands_mut()
                .expect("checked command queue exists")
                .submit_power(resolved, light, completion.clone());
            resolved
        };
        if light && resolved == Command::Off {
            self.notify_light_actions();
        }
        let _submission = Submission {
            function: self.clone(),
            completion: completion.clone(),
        };
        self.0.submitted.notify(usize::MAX);
        completion.wait().await
    }
}

#[derive(Default)]
struct Completion {
    result: Mutex<Option<Result<FunctionSnapshot, CommandError>>>,
    changed: Event,
}

impl Completion {
    fn finish(&self, result: Result<FunctionSnapshot, CommandError>) {
        let mut current = self.result.lock().unwrap();
        if current.is_none() {
            *current = Some(result);
            self.changed.notify(usize::MAX);
        }
    }

    fn result(&self) -> Option<Result<FunctionSnapshot, CommandError>> {
        self.result.lock().unwrap().clone()
    }
    fn is_finished(&self) -> bool {
        self.result.lock().unwrap().is_some()
    }
    async fn wait(&self) -> Result<FunctionSnapshot, CommandError> {
        loop {
            let listener = self.changed.listen();
            if let Some(result) = self.result.lock().unwrap().clone() {
                return result;
            }
            listener.await;
        }
    }
}

/// A waiting caller owns its place in the queue, until the adapter takes over.
struct Submission {
    function: Function,
    completion: Arc<Completion>,
}

impl Drop for Submission {
    fn drop(&mut self) {
        let mut state = self.function.0.state.lock().unwrap();
        if let Some(commands) = state.commands_mut() {
            commands
                .power
                .retain(|pending| !Arc::ptr_eq(&pending.completion, &self.completion));
        }
    }
}

/// The single command consumer preserves receive order across adapter sessions.
pub struct CommandReceiver {
    pub(super) function: Function,
}

impl CommandReceiver {
    async fn wait_active(&self) {
        let flight = self
            .function
            .0
            .state
            .lock()
            .unwrap()
            .commands()
            .expect("read-only functions have no command receiver")
            .flight
            .clone();
        if let Some(flight) = flight {
            let _ = flight.gate().wait().await;
            let mut state = self.function.0.state.lock().unwrap();
            let commands = state
                .commands_mut()
                .expect("read-only functions have no command receiver");
            if commands
                .flight
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &flight))
            {
                commands.flight = None;
            }
        }
    }
    /// One function consumes power in FIFO order and resolves Toggle at dequeue.
    fn take_power(&self, state: &mut RuntimeState) -> Option<CommandRequest> {
        loop {
            let pending = state
                .commands_mut()
                .expect("read-only functions have no command receiver")
                .power
                .pop_front()?;
            let snapshot = self.function.snapshot_locked(state);
            let target =
                snapshot
                    .availability
                    .command_result()
                    .and_then(|()| match pending.command {
                        Command::On => Ok(true),
                        Command::Off => Ok(false),
                        Command::Toggle => snapshot
                            .power()
                            .and_then(super::Power::value)
                            .map(|value| !value)
                            .ok_or(CommandError::UnknownState),
                    });
            let target = match target {
                Ok(target) => target,
                Err(error) => {
                    pending.completion.finish(Err(error));
                    continue;
                }
            };
            let flight = Arc::new(PowerFlight {
                target,
                completion: pending.completion.clone(),
                sent: AtomicBool::new(false),
                invalid: Mutex::new(None),
            });
            state
                .commands_mut()
                .expect("read-only functions have no command receiver")
                .flight = Some(Arc::new(Flight::Power(flight.clone())));
            return Some(CommandRequest {
                target,
                queued_at: pending.queued_at,
                function: self.function.clone(),
                flight,
            });
        }
    }
}

impl Drop for CommandReceiver {
    fn drop(&mut self) {
        self.function.close();
    }
}

/// A protocol-independent absolute write owned by the adapter until completion.
pub struct CommandRequest {
    pub target: bool,
    queued_at: Instant,
    function: Function,
    flight: Arc<PowerFlight>,
}

impl CommandRequest {
    pub(crate) fn queued_at(&self) -> Instant {
        self.queued_at
    }

    /// Inspect a lighting power cancellation without marking it sent.
    pub fn current_result(&self) -> Result<(), CommandError> {
        if let Some(error) = self.flight.invalid.lock().unwrap().as_ref() {
            return Err(error.clone());
        }
        if self.flight.completion.is_finished() {
            return Err(self
                .flight
                .completion
                .result()
                .and_then(Result::err)
                .unwrap_or(CommandError::Superseded));
        }
        Ok(())
    }
    pub fn mark_sent(&self) -> Result<(), CommandError> {
        let _state = self.function.0.state.lock().unwrap();
        if let Some(error) = self.flight.invalid.lock().unwrap().as_ref() {
            return Err(error.clone());
        }
        if self.flight.completion.is_finished() {
            return Err(self
                .flight
                .completion
                .result()
                .and_then(Result::err)
                .unwrap_or(CommandError::Cancelled));
        }
        self.flight.sent.store(true, Ordering::Relaxed);
        Ok(())
    }
    /// Settle this request against the current snapshot. The coordinator never
    /// infers physical state from acceptance; source observations own that.
    pub fn accept(self) -> Result<FunctionSnapshot, CommandError> {
        let state = self.function.0.state.lock().unwrap();
        if let Some(completed) = self.flight.completion.result() {
            return completed;
        }
        let accepted = Ok(self.function.snapshot_locked(&state));
        self.flight.completion.finish(accepted.clone());
        accepted
    }

    pub fn reject(self, error: CommandError) {
        self.flight.completion.finish(Err(error));
    }
}

impl Drop for CommandRequest {
    fn drop(&mut self) {
        self.flight.completion.finish(Err(CommandError::Cancelled));
    }
}

pub enum DeviceRequest {
    Power(CommandRequest),
    Light(LightRequest),
}
impl DeviceRequest {
    pub(crate) fn queued_at(&self) -> Instant {
        match self {
            Self::Power(request) => request.queued_at(),
            Self::Light(request) => request.queued_at,
        }
    }
}
impl CommandReceiver {
    /// One gate serializes power and both lighting dimensions for this function.
    pub async fn next_operation(&mut self) -> Option<DeviceRequest> {
        self.wait_active().await;
        loop {
            let listener = self.function.0.submitted.listen();
            {
                let mut state = self.function.0.state.lock().unwrap();
                let (light, power) = {
                    let commands = state
                        .commands()
                        .expect("read-only functions have no command receiver");
                    (
                        commands.first_light_pending(),
                        commands.power.front().map(|pending| pending.order),
                    )
                };
                if let Some((index, order)) = light
                    && power.is_none_or(|power| order < power)
                {
                    let commands = state
                        .commands_mut()
                        .expect("read-only functions have no command receiver");
                    let frame = commands.light_pending[index].take().unwrap();
                    let token = frame.action.token;
                    let flight = Arc::new(LightFlight {
                        action: frame.action,
                        power: frame.target.power(),
                        gate: Arc::new(Completion::default()),
                        sent: AtomicBool::new(false),
                    });
                    commands.flight = Some(Arc::new(Flight::Light(flight.clone())));
                    return Some(DeviceRequest::Light(LightRequest {
                        token,
                        target: frame.target,
                        final_frame: frame.final_frame,
                        queued_at: frame.queued_at,
                        function: self.function.clone(),
                        flight,
                    }));
                }
                if let Some(request) = self.take_power(&mut state) {
                    return Some(DeviceRequest::Power(request));
                }
                if state
                    .commands()
                    .expect("read-only functions have no command receiver")
                    .is_closed()
                {
                    return None;
                }
            }
            listener.await;
        }
    }
}

/// The adapter owns this frame until its original receipt or timeout completes.
pub struct LightRequest {
    pub token: ActionToken,
    pub target: LightTarget,
    pub final_frame: bool,
    queued_at: Instant,
    function: Function,
    flight: Arc<LightFlight>,
}
impl LightRequest {
    pub fn is_current(&self) -> bool {
        self.current_result().is_ok()
    }
    /// Inspect cancellation without transferring actual-send ownership.
    pub fn current_result(&self) -> Result<(), CommandError> {
        if !self.flight.gate.is_finished() && self.function.light_action_current(self.token) {
            Ok(())
        } else {
            Err(self
                .flight
                .action
                .ended
                .result()
                .and_then(Result::err)
                .unwrap_or(CommandError::Superseded))
        }
    }
    /// Call at the actual transport submission boundary, never just on dequeue.
    pub fn mark_sent(&self) -> Result<(), CommandError> {
        let mut state = self.function.0.state.lock().unwrap();
        let coordinator = state
            .commands_mut()
            .expect("read-only functions have no command receiver");
        if self.flight.sent.load(Ordering::Relaxed) {
            return Ok(());
        }
        if coordinator.light_action(self.token).is_none() || self.flight.gate.is_finished() {
            return Err(self
                .flight
                .action
                .ended
                .result()
                .and_then(Result::err)
                .unwrap_or(CommandError::Superseded));
        }
        self.flight.sent.store(true, Ordering::Relaxed);
        self.flight
            .action
            .startup_sent
            .store(true, Ordering::Relaxed);
        // Diagnostics distinguish an invalidated action from its original receipt.
        coordinator.light_revision += 1;
        self.function.notify_light_actions();
        Ok(())
    }
    pub fn accept(self) -> Result<FunctionSnapshot, CommandError> {
        if !self.flight.sent.load(Ordering::Relaxed) {
            let error = CommandError::SendFailed("lighting frame was not submitted".into());
            self.finish(Err(error.clone()), true);
            return Err(error);
        }
        let snapshot = self.function.snapshot();
        self.finish(Ok(snapshot.clone()), true);
        Ok(snapshot)
    }
    /// Settle a source-confirmed target that needs no transport write.
    /// Transition startup still waits for a write unless its whole target stays constant.
    pub fn accept_unchanged(self) -> Result<FunctionSnapshot, CommandError> {
        if !self.is_current() {
            return Err(self
                .flight
                .action
                .ended
                .result()
                .and_then(Result::err)
                .unwrap_or(CommandError::Superseded));
        }
        if self.flight.sent.load(Ordering::Relaxed) {
            return Err(CommandError::SendFailed(
                "submitted frame requires its transport receipt".into(),
            ));
        }
        let snapshot = self.function.snapshot();
        self.finish(
            Ok(snapshot.clone()),
            self.final_frame || self.flight.action.allow_unchanged_startup,
        );
        Ok(snapshot)
    }
    pub fn reject(self, error: CommandError) {
        self.finish(Err(error), true);
    }
    fn finish(&self, result: Result<FunctionSnapshot, CommandError>, accept_startup: bool) {
        let mut state = self.function.0.state.lock().unwrap();
        if self.flight.gate.is_finished() {
            return;
        }
        let coordinator = state
            .commands_mut()
            .expect("read-only functions have no command receiver");
        if accept_startup {
            if result.is_ok()
                && !self.flight.action.accepted.is_finished()
                && coordinator.light_action(self.token).is_some()
            {
                self.flight
                    .action
                    .accepted_while_current
                    .store(true, Ordering::Relaxed);
            }
            self.flight.action.accepted.finish(result.clone());
        }
        if coordinator.light_action(self.token).is_some() {
            match &result {
                Err(error) => coordinator.cancel_light(self.token.dimension, error.clone()),
                Ok(_) if self.final_frame => {
                    coordinator.light_current[self.token.dimension.index()] = None;
                    coordinator.light_pending[self.token.dimension.index()] = None;
                    self.flight.action.ended.finish(result.clone());
                    coordinator.record_light(self.token, Ok(()));
                }
                Ok(_) if accept_startup => coordinator.record_light(self.token, Ok(())),
                Ok(_) => coordinator.light_revision += 1,
            }
        } else {
            coordinator.light_revision += 1;
        }
        if coordinator.flight.as_ref().is_some_and(
            |flight| matches!(&**flight, Flight::Light(light) if Arc::ptr_eq(light, &self.flight)),
        ) {
            coordinator.flight = None;
        }
        self.flight.gate.finish(result);
        self.function.notify_light_actions();
    }
}
impl Drop for LightRequest {
    fn drop(&mut self) {
        self.finish(Err(CommandError::Cancelled), true);
    }
}
