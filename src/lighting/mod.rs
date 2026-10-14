//! Lighting action scheduling shared by single lights and group members.
//!
//! Each active plan owns the parameters needed to advance its physical
//! function. Group commands submit to the same control chain and keep only
//! temporary acceptance waiters. A Matter instance reset cancels pending
//! schedules when applied configuration changes; normal state updates do not.

pub(crate) mod clock;
pub(crate) mod controllers;
pub(crate) mod planning;

#[cfg(test)]
mod tests;

use crate::device::{
    ActionToken, Command, CommandError, Function, FunctionKey, FunctionSnapshot, LightAction,
    LightColorTarget, LightDimension, LightMode, LightSnapshot, LightTarget, NumericRange, Rgb,
    brightness_from_matter_level,
};
use clock::Clock;
use controllers::{color, hs, level};
use futures_util::FutureExt;
use planning::ColorState;
use std::{
    cell::RefCell,
    collections::HashMap,
    future::{Future, poll_fn},
    pin::Pin,
    rc::Rc,
    task::{Context, Poll},
};

/// A fixed trajectory submitted for one physical function dimension.
///
/// The plan carries everything later ticks need until completion, takeover,
/// or the Matter instance reset that applies a new configuration.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Plan {
    Level(level::Task),
    Color {
        task: color::Task,
        temperature: Option<NumericRange>,
    },
}
impl Plan {
    pub(crate) fn dimension(self) -> LightDimension {
        match self {
            Self::Level(_) => LightDimension::Brightness,
            Self::Color { .. } => LightDimension::Color,
        }
    }
    pub(crate) fn duration(self) -> u64 {
        match self {
            Self::Level(task) => task.duration(),
            Self::Color { task, .. } => task.duration(),
        }
    }
    pub(crate) fn frame(self, elapsed: u64) -> Result<(LightTarget, bool), CommandError> {
        match self {
            Self::Level(task) => {
                let (level, end, power) = task.frame(elapsed);
                Ok((
                    LightTarget {
                        power,
                        brightness_percent: Some(brightness_from_matter_level(level)?),
                        color: None,
                    },
                    end,
                ))
            }
            Self::Color { task, temperature } => {
                let (target, end) = task.frame(elapsed);
                let color = match target {
                    color::Target::Xy(x, y) => LightColorTarget::Rgb(color::xy_to_rgb(x, y)),
                    color::Target::Hs(value) => LightColorTarget::Rgb(hs::hs_to_rgb(value)),
                    color::Target::Temperature(mireds) => LightColorTarget::Temperature(
                        temperature
                            .expect("temperature plan requires its fixed range")
                            .kelvin_from_mireds(mireds)?,
                    ),
                };
                Ok((
                    LightTarget {
                        color: Some(color),
                        ..Default::default()
                    },
                    end,
                ))
            }
        }
    }
    fn hs_task(self) -> Option<hs::HsTask> {
        match self {
            Self::Color {
                task: color::Task::Hs(task),
                ..
            } => Some(task),
            _ => None,
        }
    }
    fn is_achromatic_hs(self) -> bool {
        self.hs_task().is_some_and(hs::HsTask::is_achromatic)
    }
}

fn index(dimension: LightDimension) -> usize {
    match dimension {
        LightDimension::Brightness => 0,
        LightDimension::Color => 1,
    }
}

#[derive(Default)]
struct Actions {
    slots: [Option<Active>; 2],
    hint: ColorHint,
}

#[derive(Default, Clone, Copy)]
struct ColorHint {
    /// 0 = HS, 1 = XY; temperature uses the fixed Matter value 2.
    rgb_mode: u8,
    hint: f64,
}

struct Active {
    function: Function,
    plan: Plan,
    token: ActionToken,
    action: Rc<LightAction>,
    start: u64,
    last: LightTarget,
    final_submitted: bool,
    accepted: bool,
}

/// Timing of a current action, without retaining its plan or completion state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Transition {
    pub start: u64,
    pub duration: u64,
}
impl Transition {
    pub(crate) fn remaining_ms(self, now: u64) -> u64 {
        self.duration.saturating_sub(now.saturating_sub(self.start))
    }
}

struct PendingPower {
    future: Pin<Box<dyn Future<Output = ()>>>,
    receipt: Rc<PowerReceipt>,
}

#[derive(Default)]
struct PowerReceipt {
    result: RefCell<Option<Result<FunctionSnapshot, CommandError>>>,
    changed: event_listener::Event,
}
impl PowerReceipt {
    fn finish(&self, result: Result<FunctionSnapshot, CommandError>) {
        let mut current = self.result.borrow_mut();
        if current.is_none() {
            *current = Some(result);
            self.changed.notify(usize::MAX);
        }
    }
    fn result(&self) -> Option<Result<FunctionSnapshot, CommandError>> {
        self.result.borrow().clone()
    }
}

/// A droppable waiter for one executor-owned power dispatch. Dropping it never
/// withdraws the underlying `Function::execute` future.
#[derive(Clone)]
pub(crate) struct PowerWaiter {
    executor: Rc<LightingExecutor>,
    receipt: Rc<PowerReceipt>,
}
impl PowerWaiter {
    pub(crate) async fn accepted(&self) -> Result<FunctionSnapshot, CommandError> {
        loop {
            let mut listener = self.receipt.changed.listen();
            // Poll the executor-owned dispatch with this waiter's task waker so
            // a physical accept/reject wakes the caller directly. The future
            // still belongs to the executor: dropping this waiter withdraws
            // nothing. The receipt event keeps every other waiter woken, and
            // the runtime runner can always drive the dispatch on its own.
            let result = poll_fn(|cx| {
                self.executor.drive_power_with(cx);
                if let Some(result) = self.result() {
                    return Poll::Ready(Some(result));
                }
                match Pin::new(&mut listener).poll(cx) {
                    Poll::Ready(()) => Poll::Ready(None),
                    Poll::Pending => Poll::Pending,
                }
            })
            .await;
            if let Some(result) = result {
                return result;
            }
        }
    }
    pub(crate) fn result(&self) -> Option<Result<FunctionSnapshot, CommandError>> {
        self.receipt.result()
    }
}

/// Capture accepted representation metadata from a startup receipt.
///
/// The scheduler and Matter reads capture the current mode and achromatic
/// hue before an ended action releases its slot.
fn capture_color_hint(hint: &mut ColorHint, active: &mut Active) {
    let ended_ok = matches!(active.action.ended().now_or_never(), Some(Ok(_)));
    let newly_accepted = !active.accepted && active.action.accepted_while_current();
    if !newly_accepted {
        // A completed HS action retains its final hue for achromatic state.
        if active.accepted
            && ended_ok
            && let Plan::Color {
                task: color::Task::Hs(task),
                ..
            } = active.plan
        {
            hint.rgb_mode = 0;
            hint.hint = task.frame(task.duration()).0.hue;
        }
        return;
    }
    active.accepted = true;
    match active.plan {
        Plan::Color {
            task: color::Task::Hs(task),
            ..
        } => {
            hint.rgb_mode = 0;
            hint.hint = task.frame(if ended_ok { task.duration() } else { 0 }).0.hue;
        }
        Plan::Color {
            task: color::Task::XyMoveTo { .. } | color::Task::XyMove { .. },
            ..
        } => {
            hint.rgb_mode = 1;
        }
        _ => {}
    }
}

/// The process-local scheduler for current lighting actions.
pub(crate) struct LightingExecutor {
    clock: Rc<dyn Clock>,
    actions: RefCell<HashMap<FunctionKey, Actions>>,
    power: RefCell<Vec<PendingPower>>,
    changed: event_listener::Event,
}

impl LightingExecutor {
    pub(crate) fn new(clock: Rc<dyn Clock>) -> Rc<Self> {
        Rc::new(Self {
            clock,
            actions: RefCell::new(HashMap::new()),
            power: RefCell::new(Vec::new()),
            changed: event_listener::Event::new(),
        })
    }

    pub(crate) fn clock(&self) -> &Rc<dyn Clock> {
        &self.clock
    }

    /// Commit one prepared plan for one physical function. The first frame and
    /// the final flag are fixed now; the device queue owns the takeover of the
    /// function's dimension and other functions are untouched.
    pub(crate) fn begin(
        &self,
        function: &Function,
        plan: Plan,
        forced_power: Option<bool>,
    ) -> Result<Rc<LightAction>, CommandError> {
        self.begin_at(function, plan, forced_power, self.clock.now())
    }

    /// Commit one prepared plan at a caller-fixed origin so every member of a
    /// fanout shares one trigger instant.
    fn begin_at(
        &self,
        function: &Function,
        plan: Plan,
        forced_power: Option<bool>,
        start: u64,
    ) -> Result<Rc<LightAction>, CommandError> {
        let (mut target, final_frame) = plan.frame(0)?;
        if forced_power.is_some() {
            target.power = forced_power;
        }
        let dimension = plan.dimension();
        let action = Rc::new(function.begin_light_action(dimension, target, final_frame)?);
        let mut actions = self.actions.borrow_mut();
        let entry = actions.entry(function.function_key().clone()).or_default();
        entry.slots[index(dimension)] = Some(Active {
            function: function.clone(),
            plan,
            token: action.token(),
            action: action.clone(),
            start,
            last: target,
            final_submitted: final_frame,
            accepted: false,
        });
        drop(actions);
        self.changed.notify(usize::MAX);
        if plan.duration() > 0 && plan.is_achromatic_hs() {
            function.accept_light_state_only_startup(action.token())?;
        }
        Ok(action)
    }

    /// Cancel one committed dimension and its actual device work.
    pub(crate) fn stop(
        &self,
        function: &Function,
        dimension: LightDimension,
    ) -> Result<(), CommandError> {
        function.stop_light(dimension)?;
        if let Some(entry) = self.actions.borrow_mut().get_mut(function.function_key()) {
            entry.slots[index(dimension)] = None;
        }
        self.changed.notify(usize::MAX);
        Ok(())
    }

    /// Stop only when the color dimension currently holds an HS trajectory.
    /// Returns whether an action was actually stopped.
    pub(crate) fn stop_hs(&self, function: &Function) -> Result<bool, CommandError> {
        let is_hs = self
            .actions
            .borrow()
            .get(function.function_key())
            .and_then(|entry| entry.slots[1].as_ref())
            .is_some_and(|active| active.plan.hs_task().is_some());
        if is_hs {
            self.stop(function, LightDimension::Color)?;
        }
        Ok(is_hs)
    }

    /// Start a definite power target owned by the executor. The returned
    /// waiter may be dropped while the command is still unsent; resetting the
    /// executor withdraws pending submissions.
    pub(crate) fn dispatch_power(
        self: &Rc<Self>,
        function: &Function,
        command: Command,
    ) -> PowerWaiter {
        let receipt = Rc::new(PowerReceipt::default());
        let function = function.clone();
        let pending_receipt = receipt.clone();
        let future: Pin<Box<dyn Future<Output = ()>>> = Box::pin(async move {
            let result = function.execute(command).await;
            pending_receipt.finish(result);
        });
        self.power.borrow_mut().push(PendingPower {
            future,
            receipt: receipt.clone(),
        });
        // Wake an idle runner so it starts draining the new dispatch even when
        // every waiter is dropped and no trajectory is active.
        self.changed.notify(usize::MAX);
        self.drive_power();
        PowerWaiter {
            executor: self.clone(),
            receipt,
        }
    }

    /// Cancel current schedules when the Matter instance ends. New instances
    /// start without replaying unfinished commands; representation hints stay
    /// available for the current physical state.
    pub(crate) fn reset(&self) {
        for entry in self.actions.borrow_mut().values_mut() {
            for slot in &mut entry.slots {
                if let Some(active) = slot.take() {
                    active.function.cancel_light_action(active.token);
                }
            }
        }
        for pending in self.power.borrow_mut().drain(..) {
            pending.receipt.finish(Err(CommandError::Cancelled));
        }
        self.changed.notify(usize::MAX);
    }

    /// Timing of the longest current member action. Original duration lets
    /// protocol reporting distinguish a short action from a long action's tail.
    pub(crate) fn transition<'a>(
        &self,
        functions: impl IntoIterator<Item = &'a FunctionKey>,
        dimension: LightDimension,
    ) -> Option<Transition> {
        let now = self.clock.now();
        let actions = self.actions.borrow();
        functions
            .into_iter()
            .filter_map(|key| {
                let active = actions.get(key)?.slots[index(dimension)].as_ref()?;
                active
                    .function
                    .light_action_current(active.token)
                    .then_some(Transition {
                        start: active.start,
                        duration: active.plan.duration(),
                    })
            })
            .max_by_key(|transition| (transition.remaining_ms(now), transition.duration))
    }

    /// Whether the active dimension has submitted its final frame and the
    /// device queue already released it.
    pub(crate) fn ending(&self, key: &FunctionKey, dimension: LightDimension) -> bool {
        self.actions
            .borrow()
            .get(key)
            .and_then(|entry| entry.slots[index(dimension)].as_ref())
            .is_none_or(|active| {
                active.final_submitted && !active.function.light_action_current(active.token)
            })
    }

    pub(crate) fn has_active(&self, key: &FunctionKey) -> bool {
        self.actions
            .borrow()
            .get(key)
            .is_some_and(|entry| entry.slots.iter().any(Option::is_some))
    }

    /// The next unfinished action deadline across every function, or now +
    /// 20ms when only final frames wait for their receipts or nothing is live.
    fn next_deadline_all(&self) -> u64 {
        let now = self.clock.now();
        self.actions
            .borrow()
            .values()
            .flat_map(|entry| entry.slots.iter())
            .flatten()
            .filter(|active| !active.final_submitted)
            .map(|active| active.start.saturating_add(active.plan.duration()))
            .fold(now.saturating_add(20), u64::min)
    }

    fn has_any_active(&self) -> bool {
        self.actions
            .borrow()
            .values()
            .any(|entry| entry.slots.iter().any(Option::is_some))
    }

    fn has_pending_power(&self) -> bool {
        !self.power.borrow().is_empty()
    }

    /// The resumed HS trajectory of an active color action, used by the
    /// Matter entry for single-axis commands.
    pub(crate) fn resumed_hs(&self, key: &FunctionKey) -> Option<hs::HsTask> {
        self.resumed_hs_at(key, self.clock.now())
    }

    /// Resumed HS trajectory at a caller-fixed origin, so a fanout resolves
    /// every member against the same trigger instant.
    fn resumed_hs_at(&self, key: &FunctionKey, now: u64) -> Option<hs::HsTask> {
        let actions = self.actions.borrow();
        let active = actions.get(key)?.slots[1].as_ref()?;
        if !active.function.light_action_current(active.token) {
            return None;
        }
        active
            .plan
            .hs_task()
            .map(|task| task.resume(now.saturating_sub(active.start)))
    }

    /// Current Matter color-mode representation metadata for accepted HS/XY.
    pub(crate) fn rgb_mode(&self, key: &FunctionKey) -> u8 {
        self.actions
            .borrow()
            .get(key)
            .map_or(0, |entry| entry.hint.rgb_mode)
    }

    /// Commit representation metadata once the startup receipt was accepted
    /// while the action still owned its dimension. Matter reads use this as a
    /// fast path; the executor tick captures the same state independently.
    pub(crate) fn accept_color_metadata(&self, key: &FunctionKey) {
        let mut actions = self.actions.borrow_mut();
        let Some(entry) = actions.get_mut(key) else {
            return;
        };
        let Some(active) = entry.slots[1].as_mut() else {
            return;
        };
        capture_color_hint(&mut entry.hint, active);
    }

    /// Project the caller-selected observations into normalized color values.
    /// Freshness, published capabilities and any continuing trajectory remain
    /// the caller's responsibility.
    pub(crate) fn observed_color(
        &self,
        key: Option<&FunctionKey>,
        lighting: Option<LightSnapshot>,
        temperature: Option<NumericRange>,
    ) -> ColorState {
        let temperature_mireds = temperature.and_then(|range| {
            lighting
                .and_then(|state| state.temperature_kelvin)
                .map(|value| {
                    range
                        .mireds_from_kelvin(value)
                        .expect("validated snapshot temperature")
                })
        });
        let (xy, hs) = match lighting.and_then(|state| state.mode) {
            Some(LightMode::Temperature) => {
                // Active CT must not overwrite the accepted achromatic hue hint.
                let xy = temperature_mireds.map(color::temperature_to_xy);
                (xy, xy.map(|(x, y)| hs::rgb_to_hs(color::xy_to_rgb(x, y))))
            }
            Some(LightMode::Rgb) => {
                let rgb = lighting.and_then(|state| state.color);
                (
                    rgb.and_then(color::rgb_to_xy),
                    rgb.map(|rgb| {
                        key.map_or_else(|| hs::rgb_to_hs(rgb), |key| self.hs_for_rgb(key, rgb))
                    }),
                )
            }
            None => (None, None),
        };
        ColorState {
            temperature_mireds,
            xy,
            hs,
            continuing_hs: None,
        }
    }

    /// Matter HS projection with hue retention through achromatic frames.
    pub(crate) fn hs_for_rgb(&self, key: &FunctionKey, rgb: Rgb) -> hs::Hs {
        let mut value = hs::rgb_to_hs(rgb);
        let mut actions = self.actions.borrow_mut();
        if let Some(entry) = actions.get_mut(key) {
            if let Some(active) = entry.slots[1].as_ref()
                && active.accepted
                && let Plan::Color {
                    task: color::Task::Hs(task),
                    ..
                } = active.plan
            {
                let finished = active.final_submitted
                    && matches!(active.action.ended().now_or_never(), Some(Ok(_)));
                let elapsed = if finished {
                    task.duration()
                } else {
                    self.clock.now().saturating_sub(active.start)
                };
                let requested = task.frame(elapsed).0;
                if (finished
                    || (value.saturation == 0.0
                        && active.function.light_action_current(active.token)))
                    && hs::hs_to_rgb(requested) == rgb
                {
                    entry.hint.hint = requested.hue;
                }
            }
            if value.saturation == 0.0 || (value.hue == 0.0 && entry.hint.hint == 254.0) {
                value.hue = entry.hint.hint;
            } else {
                entry.hint.hint = value.hue;
            }
        }
        value
    }

    /// Drive every committed action one step and poll pending power dispatch.
    pub(crate) fn advance(&self) {
        self.drive_power();
        let now = self.clock.now();
        let mut actions = self.actions.borrow_mut();
        for entry in actions.values_mut() {
            for slot in entry.slots.iter_mut() {
                let Some(active) = slot else {
                    continue;
                };
                // Capture representation metadata before clearing an ended slot.
                capture_color_hint(&mut entry.hint, active);
                if !active.function.light_action_current(active.token) {
                    // A takeover, revoke or close already ended the device
                    // action; stop replaying it without sending another frame.
                    *slot = None;
                    continue;
                }
                if active.final_submitted {
                    continue;
                }
                let elapsed = now.saturating_sub(active.start);
                let (target, final_frame) =
                    active.plan.frame(elapsed).expect("validated lighting plan");
                let local_startup = !active.accepted && active.plan.is_achromatic_hs();
                if final_frame || target != active.last || local_startup {
                    let submitted = active
                        .function
                        .submit_light_frame(active.token, target, final_frame)
                        .and_then(|()| {
                            if local_startup {
                                // A preceding write may have prevented local
                                // acceptance at startup; recheck after its gate
                                // settles, even when later RGB frames match.
                                active
                                    .function
                                    .accept_light_state_only_startup(active.token)
                                    .map(|_| ())
                            } else {
                                Ok(())
                            }
                        });
                    match submitted {
                        Ok(()) => {
                            active.last = target;
                            active.final_submitted = final_frame;
                        }
                        Err(error) => {
                            log::warn!("shared lighting action ended: {error}");
                            *slot = None;
                            break;
                        }
                    }
                }
            }
        }
        drop(actions);
        self.changed.notify(usize::MAX);
    }

    /// Runtime runner. It advances frames and drains power dispatches on the
    /// shared clock in parallel with the source and Matter services.
    pub(crate) async fn run(self: Rc<Self>) {
        loop {
            self.advance();
            let listener = self.changed.listen();
            if self.has_any_active() || self.has_pending_power() {
                let now = self.clock.now();
                // Always wait at least 1ms forward: a deadline that lapsed
                // between advance() and the wait must not spin the runner.
                let deadline = self
                    .next_deadline_all()
                    .min(now.saturating_add(20))
                    .max(now.saturating_add(1));
                futures_lite::future::or(listener, self.clock.wait_until(deadline)).await;
            } else {
                listener.await;
            }
        }
    }

    fn drive_power(&self) {
        self.drive_power_with(&mut Context::from_waker(std::task::Waker::noop()));
    }

    /// Drive pending power dispatches with a real task context so a waiter can
    /// be woken by the physical accept/reject. The executor keeps ownership of
    /// every future; this only polls them.
    fn drive_power_with(&self, cx: &mut Context<'_>) {
        let mut pending = self.power.borrow_mut();
        let mut index = 0;
        while index < pending.len() {
            match pending[index].future.as_mut().poll(cx) {
                Poll::Ready(()) => {
                    pending.swap_remove(index);
                }
                Poll::Pending => {
                    index += 1;
                }
            }
        }
    }
}

/// Runtime end terminates actual execution. Nothing here persists or replays;
/// a later process starts with an empty executor.
impl Drop for LightingExecutor {
    fn drop(&mut self) {
        self.reset();
    }
}
