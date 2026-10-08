use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

use super::{
    Capability, Category, CommandError, CommandReceiver, CommandRequest, Function,
    FunctionSnapshot, Rgb, command::Completion, state::RuntimeState,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LightColorTarget {
    Temperature(u32),
    Rgb(Rgb),
}

/// Power is explicit: a level change alone never implies on/off.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LightTarget {
    pub power: Option<bool>,
    pub brightness_percent: Option<f64>,
    pub color: Option<LightColorTarget>,
}

struct Action {
    token: ActionToken,
    accepted: Arc<Completion>,
    ended: Arc<Completion>,
    startup_sent: AtomicBool,
    final_submitted: AtomicBool,
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

struct Frame {
    action: Arc<Action>,
    target: LightTarget,
    final_frame: bool,
    order: u64,
}
struct Flight {
    action: Arc<Action>,
    gate: Arc<Completion>,
    sent: AtomicBool,
}

pub(super) struct LightCoordinator {
    owner: u64,
    serial: u64,
    order: u64,
    current: [Option<Arc<Action>>; 2],
    pending: [Option<Frame>; 2],
    flight: Option<Arc<Flight>>,
    last: Option<LightActionResult>,
    revision: u64,
}
impl LightCoordinator {
    pub(super) fn new() -> Self {
        static NEXT_OWNER: AtomicU64 = AtomicU64::new(1);
        Self {
            owner: NEXT_OWNER.fetch_add(1, Ordering::Relaxed),
            serial: 0,
            order: 0,
            current: [None, None],
            pending: [None, None],
            flight: None,
            last: None,
            revision: 0,
        }
    }
    pub(super) fn next_order(&mut self) -> u64 {
        self.order += 1;
        self.order
    }
    fn current(&self, token: ActionToken) -> Option<&Arc<Action>> {
        self.current[token.dimension.index()]
            .as_ref()
            .filter(|action| action.token == token)
    }
    fn diagnostics(&self) -> LightActionDiagnostics {
        LightActionDiagnostics {
            brightness: self.current[0].as_ref().map(|action| action.token),
            color: self.current[1].as_ref().map(|action| action.token),
            awaiting_startup: self.flight.as_ref().and_then(|flight| {
                (flight.sent.load(Ordering::Relaxed) && !flight.action.accepted.is_finished())
                    .then_some(flight.action.token)
            }),
            last: self.last.clone(),
            revision: self.revision,
        }
    }
    fn record(&mut self, token: ActionToken, result: Result<(), CommandError>) {
        self.last = Some(LightActionResult { token, result });
        self.revision += 1;
    }
    pub(super) fn cancel(&mut self, dimension: LightDimension, error: CommandError) {
        let index = dimension.index();
        self.pending[index] = None;
        if let Some(action) = self.current[index].take() {
            action.ended.finish(Err(error.clone()));
            if !action.startup_sent.load(Ordering::Relaxed) {
                action.accepted.finish(Err(error.clone()));
            }
            if let Some(flight) = &self.flight
                && flight.action.token == action.token
                && !flight.sent.load(Ordering::Relaxed)
            {
                flight.gate.finish(Err(error.clone()));
            }
            self.record(action.token, Err(error));
        }
    }
    pub(super) fn cancel_all(&mut self, error: CommandError) {
        self.cancel(LightDimension::Brightness, error.clone());
        self.cancel(LightDimension::Color, error);
    }
    fn enqueue(&mut self, action: Arc<Action>, mut target: LightTarget, final_frame: bool) {
        let index = action.token.dimension.index();
        if let Some(pending) = &self.pending[index] {
            target.power = target.power.or(pending.target.power);
        }
        // Repeated ticks retain their place relative to the other dimension.
        let order = self.pending[index]
            .as_ref()
            .map(|pending| pending.order)
            .unwrap_or_else(|| self.next_order());
        self.pending[index] = Some(Frame {
            action,
            target,
            final_frame,
            order,
        });
    }
    fn first_pending(&self) -> Option<(usize, u64)> {
        self.pending
            .iter()
            .enumerate()
            .filter_map(|(index, frame)| frame.as_ref().map(|frame| (index, frame.order)))
            .min_by_key(|(_, order)| *order)
    }
}

impl Function {
    fn validate_light_target(
        &self,
        dimension: LightDimension,
        target: LightTarget,
    ) -> Result<(), CommandError> {
        let description = self.description();
        if description.category != Category::Light {
            return Err(CommandError::Unsupported);
        }
        match dimension {
            LightDimension::Brightness => {
                if !description.capabilities.contains(&Capability::Brightness) {
                    return Err(CommandError::Unsupported);
                }
                if target.color.is_some() || target.brightness_percent.is_none() {
                    return Err(CommandError::Rejected(
                        "brightness action requires only a level target".into(),
                    ));
                }
                super::model::validate_brightness(target.brightness_percent.unwrap())?;
            }
            LightDimension::Color => {
                if target.brightness_percent.is_some() || target.color.is_none() {
                    return Err(CommandError::Rejected(
                        "color action requires only a color target".into(),
                    ));
                }
                match target.color.unwrap() {
                    LightColorTarget::Temperature(value) => {
                        let Some(range) = description.temperature_range() else {
                            return Err(CommandError::Unsupported);
                        };
                        if !(range.min..=range.max).contains(&value) {
                            return Err(CommandError::Rejected(
                                "color temperature is outside its physical range".into(),
                            ));
                        }
                    }
                    LightColorTarget::Rgb(_)
                        if !description.capabilities.contains(&Capability::ColorRgb) =>
                    {
                        return Err(CommandError::Unsupported);
                    }
                    LightColorTarget::Rgb(_) => {}
                }
            }
        }
        if target.power.is_some() && !description.capabilities.contains(&Capability::OnOff) {
            return Err(CommandError::Unsupported);
        }
        Ok(())
    }

    /// Validate before taking over the dimension; the first frame shares the
    /// action's acceptance waiter with every subsequent controller tick.
    pub fn begin_light_action(
        &self,
        dimension: LightDimension,
        target: LightTarget,
        final_frame: bool,
    ) -> Result<LightAction, CommandError> {
        self.validate_light_target(dimension, target)?;
        let mut state = self.0.state.lock().unwrap();
        self.validate_light_available(&state)?;
        let coordinator = state.light_commands.as_mut().unwrap();
        if target.power == Some(false) {
            coordinator.cancel_all(CommandError::Superseded);
        } else {
            coordinator.cancel(dimension, CommandError::Superseded);
        }
        coordinator.serial += 1;
        let action = Arc::new(Action {
            token: ActionToken {
                owner: coordinator.owner,
                serial: coordinator.serial,
                dimension,
            },
            accepted: Arc::new(Completion::default()),
            ended: Arc::new(Completion::default()),
            startup_sent: AtomicBool::new(false),
            final_submitted: AtomicBool::new(final_frame),
        });
        coordinator.current[dimension.index()] = Some(action.clone());
        coordinator.enqueue(action.clone(), target, final_frame);
        coordinator.revision += 1;
        if target.power == Some(false) {
            state.queue.invalidate_light_on();
        }
        self.notify_light_actions();
        Ok(LightAction(action))
    }

    pub fn submit_light_frame(
        &self,
        token: ActionToken,
        target: LightTarget,
        final_frame: bool,
    ) -> Result<(), CommandError> {
        self.validate_light_target(token.dimension, target)?;
        let mut state = self.0.state.lock().unwrap();
        self.validate_light_available(&state)?;
        let coordinator = state.light_commands.as_mut().unwrap();
        let action = coordinator
            .current(token)
            .cloned()
            .ok_or(CommandError::Superseded)?;
        if action.final_submitted.load(Ordering::Relaxed) {
            return Err(CommandError::Rejected(
                "action already has its final frame".into(),
            ));
        }
        action.final_submitted.store(final_frame, Ordering::Relaxed);
        if target.power == Some(false) {
            let other = match token.dimension {
                LightDimension::Brightness => LightDimension::Color,
                LightDimension::Color => LightDimension::Brightness,
            };
            coordinator.cancel(other, CommandError::Superseded);
        }
        coordinator.enqueue(action, target, final_frame);
        if target.power == Some(false) {
            state.queue.invalidate_light_on();
            self.notify_light_actions();
        }
        self.0.submitted.notify(usize::MAX);
        Ok(())
    }
    fn validate_light_available(&self, state: &RuntimeState) -> Result<(), CommandError> {
        if state.queue.closed {
            return Err(CommandError::Cancelled);
        }
        self.snapshot_locked(state).availability.command_result()
    }
    pub fn light_action_current(&self, token: ActionToken) -> bool {
        self.0
            .state
            .lock()
            .unwrap()
            .light_commands
            .as_ref()
            .is_some_and(|coordinator| coordinator.current(token).is_some())
    }
    pub fn stop_light(&self, dimension: LightDimension) -> Result<FunctionSnapshot, CommandError> {
        self.cancel_light_dimension(dimension, CommandError::Stopped)
    }
    /// The adapter invokes takeover before applying a validated external report.
    pub fn take_over_light(
        &self,
        dimension: LightDimension,
    ) -> Result<FunctionSnapshot, CommandError> {
        self.cancel_light_dimension(dimension, CommandError::Superseded)
    }
    fn cancel_light_dimension(
        &self,
        dimension: LightDimension,
        error: CommandError,
    ) -> Result<FunctionSnapshot, CommandError> {
        let mut state = self.0.state.lock().unwrap();
        let Some(coordinator) = state.light_commands.as_mut() else {
            return Err(CommandError::Unsupported);
        };
        coordinator.cancel(dimension, error);
        self.notify_light_actions();
        Ok(self.snapshot_locked(&state))
    }
    /// External off invalidates both dimensions and queued power operations.
    pub fn take_over_light_all(&self) -> Result<FunctionSnapshot, CommandError> {
        let mut state = self.0.state.lock().unwrap();
        let Some(coordinator) = state.light_commands.as_mut() else {
            return Err(CommandError::Unsupported);
        };
        coordinator.cancel_all(CommandError::Superseded);
        state.queue.invalidate_light_power(CommandError::Superseded);
        for completion in state.queue.drain_pending() {
            completion.finish(Err(CommandError::Superseded));
        }
        self.notify_light_actions();
        Ok(self.snapshot_locked(&state))
    }
    pub fn light_action_diagnostics(&self) -> Option<LightActionDiagnostics> {
        self.0
            .state
            .lock()
            .unwrap()
            .light_commands
            .as_ref()
            .map(LightCoordinator::diagnostics)
    }
    pub fn subscribe_light_actions(&self) -> Option<LightActionChanges> {
        self.light_action_diagnostics()
            .map(|snapshot| LightActionChanges {
                function: self.clone(),
                revision: snapshot.revision,
            })
    }
    pub(super) fn notify_light_actions(&self) {
        self.0.light_changed.notify(usize::MAX);
        self.0.submitted.notify(usize::MAX);
    }
}

pub struct LightActionChanges {
    function: Function,
    revision: u64,
}
impl LightActionChanges {
    pub async fn changed(&mut self) -> LightActionDiagnostics {
        loop {
            let listener = self.function.0.light_changed.listen();
            let snapshot = self.function.light_action_diagnostics().unwrap();
            if snapshot.revision != self.revision {
                self.revision = snapshot.revision;
                return snapshot;
            }
            listener.await;
        }
    }
}

pub enum DeviceRequest {
    Power(CommandRequest),
    Light(LightRequest),
}
impl CommandReceiver {
    /// One gate serializes power and both lighting dimensions for this function.
    pub async fn next_operation(&mut self) -> Option<DeviceRequest> {
        self.wait_active().await;
        loop {
            let listener = self.function.0.submitted.listen();
            {
                let mut state = self.function.0.state.lock().unwrap();
                let light = state
                    .light_commands
                    .as_ref()
                    .and_then(LightCoordinator::first_pending);
                let power = state.queue.pending.front().map(|pending| pending.order);
                if let Some((index, order)) = light
                    && power.is_none_or(|power| order < power)
                {
                    let coordinator = state.light_commands.as_mut().unwrap();
                    let frame = coordinator.pending[index].take().unwrap();
                    let flight = Arc::new(Flight {
                        action: frame.action,
                        gate: Arc::new(Completion::default()),
                        sent: AtomicBool::new(false),
                    });
                    coordinator.flight = Some(flight.clone());
                    state.queue.active = Some(flight.gate.clone());
                    return Some(DeviceRequest::Light(LightRequest {
                        token: flight.action.token,
                        target: frame.target,
                        final_frame: frame.final_frame,
                        function: self.function.clone(),
                        flight,
                    }));
                }
                if let Some(request) = self.take_power(&mut state) {
                    return Some(DeviceRequest::Power(request));
                }
                if state.queue.closed {
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
    function: Function,
    flight: Arc<Flight>,
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
        let coordinator = state.light_commands.as_mut().unwrap();
        if self.flight.sent.load(Ordering::Relaxed) {
            return Ok(());
        }
        if coordinator.current(self.token).is_none() || self.flight.gate.is_finished() {
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
        coordinator.revision += 1;
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
    /// Quantized nonfinal duplicates release the gate without accepting startup.
    /// A final no-write operation may complete locally using the known snapshot.
    pub fn skip_unchanged(self) -> Result<FunctionSnapshot, CommandError> {
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
        self.finish(Ok(snapshot.clone()), self.final_frame);
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
        let coordinator = state.light_commands.as_mut().unwrap();
        if accept_startup {
            self.flight.action.accepted.finish(result.clone());
        }
        if coordinator.current(self.token).is_some() {
            match &result {
                Err(error) => coordinator.cancel(self.token.dimension, error.clone()),
                Ok(_) if self.final_frame => {
                    coordinator.current[self.token.dimension.index()] = None;
                    coordinator.pending[self.token.dimension.index()] = None;
                    self.flight.action.ended.finish(result.clone());
                    coordinator.record(self.token, Ok(()));
                }
                Ok(_) if accept_startup => coordinator.record(self.token, Ok(())),
                Ok(_) => {
                    coordinator.revision += 1;
                }
            }
        } else {
            coordinator.revision += 1;
        }
        if coordinator
            .flight
            .as_ref()
            .is_some_and(|flight| Arc::ptr_eq(flight, &self.flight))
        {
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
