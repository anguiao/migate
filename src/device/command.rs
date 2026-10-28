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

mod requests;

pub(super) use requests::Completion;
pub use requests::{CommandReceiver, CommandRequest, DeviceRequest, LightRequest};
