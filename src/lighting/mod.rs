// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Project CHIP Authors
// Licensed under Apache-2.0: https://www.apache.org/licenses/LICENSE-2.0
//
// Shared lighting scheduler extracted from the Matter lighting layer. It owns
// per-dimension trajectories, action tokens, elapsed-deadline frame
// progression, final-frame handling, cancellation observation, the clock seam
// and raw action progress for ordinary Function/LightTarget/LightAction
// targets. Protocol decoding, unit conversion and cluster representation stay
// in Matter.
#[cfg(test)]
mod tests;

pub(crate) mod clock;
pub(crate) mod color;
pub(crate) mod hs;
pub(crate) mod level;

use crate::device::{
    ActionToken, CommandError, Function, LightAction, LightDimension, LightTarget,
};
use clock::Clock;
use futures_util::FutureExt;
use std::{
    cell::{Cell, RefCell},
    future::Future,
    pin::Pin,
    rc::Rc,
};

#[derive(Clone, Copy, Debug)]
pub(crate) enum Task {
    Level(level::Task),
    Color(color::Task),
}
impl Task {
    pub(crate) fn dimension(self) -> LightDimension {
        match self {
            Self::Level(_) => LightDimension::Brightness,
            Self::Color(_) => LightDimension::Color,
        }
    }
    pub(crate) fn duration(self) -> u64 {
        match self {
            Self::Level(task) => task.duration(),
            Self::Color(task) => task.duration(),
        }
    }
    /// Convert one sampled frame into a validated device target. The unit
    /// functions are supplied by the protocol boundary so encoding stays out
    /// of the trajectory; interpolation and rounding are unchanged.
    fn target(
        self,
        elapsed: u64,
        brightness: fn(u8) -> Result<f64, CommandError>,
        temperature: fn(u16) -> f64,
    ) -> Result<(LightTarget, bool), CommandError> {
        match self {
            Self::Level(task) => {
                let (level, end, power) = task.frame(elapsed);
                Ok((LightTarget::brightness(power, brightness(level)?)?, end))
            }
            Self::Color(task) => {
                let (target, end) = task.frame(elapsed);
                let target = match target {
                    color::Target::Xy(x, y) => LightTarget::rgb(None, color::xy_to_rgb(x, y)),
                    color::Target::Hs(value) => LightTarget::rgb(None, hs::hs_to_rgb(value)),
                    color::Target::Temperature(mireds) => {
                        LightTarget::temperature(None, temperature(mireds))?
                    }
                };
                Ok((target, end))
            }
        }
    }
}

struct Active {
    task: Task,
    token: ActionToken,
    action: Rc<LightAction>,
    start: u64,
    last: LightTarget,
    final_submitted: bool,
}

/// Raw trajectory observation. Report encoding, version counters and
/// notification cursors belong to the Matter report owner.
#[derive(Clone, Copy)]
pub(crate) struct Observation {
    pub(crate) token: ActionToken,
    pub(crate) task: Task,
    /// The startup receipt was committed while this action still owned its dimension.
    pub(crate) accepted_while_current: bool,
    pub(crate) final_submitted: bool,
    pub(crate) current: bool,
    pub(crate) ended_ok: bool,
    pub(crate) elapsed: u64,
    pub(crate) duration: u64,
    /// Raw remaining transition time in milliseconds, before protocol encoding.
    pub(crate) remaining_ms: u64,
}

fn index(dimension: LightDimension) -> usize {
    match dimension {
        LightDimension::Brightness => 0,
        LightDimension::Color => 1,
    }
}

/// Per-dimension trajectory scheduler. It works with ordinary Function targets
/// and action tokens and knows nothing about Matter, units or native groups.
pub(crate) struct Scheduler {
    function: Function,
    clock: Rc<dyn Clock>,
    brightness: fn(u8) -> Result<f64, CommandError>,
    temperature: fn(u16) -> f64,
    active: RefCell<[Option<Active>; 2]>,
    remaining: Cell<[u64; 2]>,
}
impl Scheduler {
    pub(crate) fn new(
        function: Function,
        clock: Rc<dyn Clock>,
        brightness: fn(u8) -> Result<f64, CommandError>,
        temperature: fn(u16) -> f64,
    ) -> Self {
        Self {
            function,
            clock,
            brightness,
            temperature,
            active: RefCell::new([None, None]),
            remaining: Cell::new([0, 0]),
        }
    }
    pub(crate) fn begin(
        &self,
        task: Task,
        forced_power: Option<bool>,
    ) -> Result<Rc<LightAction>, CommandError> {
        let (mut target, final_frame) = task.target(0, self.brightness, self.temperature)?;
        if forced_power.is_some() {
            target = target.with_power(forced_power);
        }
        let dimension = task.dimension();
        let allow_unchanged_startup =
            matches!(task, Task::Color(color::Task::Hs(task)) if task.is_achromatic());
        let action = Rc::new(self.function.begin_light_action_with_unchanged_startup(
            target,
            final_frame,
            allow_unchanged_startup,
        )?);
        self.active.borrow_mut()[index(dimension)] = Some(Active {
            task,
            token: action.token(),
            action: action.clone(),
            start: self.clock.now(),
            last: target,
            final_submitted: final_frame,
        });
        self.set_remaining(dimension, task.duration());
        Ok(action)
    }
    pub(crate) fn advance(&self) {
        let now = self.clock.now();
        for index in 0..2 {
            let mut actions = self.active.borrow_mut();
            let Some(active) = actions[index].as_mut() else {
                continue;
            };
            let dimension = active.task.dimension();
            if !self.function.light_action_current(active.token) {
                actions[index] = None;
                drop(actions);
                self.set_remaining(dimension, 0);
                continue;
            }
            if active.final_submitted {
                continue;
            }
            let elapsed = now.saturating_sub(active.start);
            let (target, final_frame) = active
                .task
                .target(elapsed, self.brightness, self.temperature)
                .expect("validated controller task");
            let mut remaining = active.task.duration().saturating_sub(elapsed);
            if final_frame || target != active.last {
                let submitted = self
                    .function
                    .submit_light_frame(active.token, target, final_frame);
                match submitted {
                    Ok(()) => {
                        active.last = target;
                        active.final_submitted = final_frame;
                    }
                    Err(error) => {
                        log::warn!("lighting action ended: {error}");
                        actions[index] = None;
                        remaining = 0;
                    }
                }
            }
            drop(actions);
            self.set_remaining(dimension, remaining);
        }
    }
    pub(crate) fn stop(&self, dimension: LightDimension) -> Result<(), CommandError> {
        self.function.stop_light(dimension)?;
        self.active.borrow_mut()[index(dimension)] = None;
        self.set_remaining(dimension, 0);
        Ok(())
    }
    pub(crate) fn observe(&self, dimension: LightDimension) -> Option<Observation> {
        let actions = self.active.borrow();
        let active = actions[index(dimension)].as_ref()?;
        let ended = active.action.ended().now_or_never();
        Some(Observation {
            token: active.token,
            task: active.task,
            accepted_while_current: active.action.accepted_while_current(),
            final_submitted: active.final_submitted,
            current: self.function.light_action_current(active.token),
            ended_ok: matches!(ended, Some(Ok(_))),
            elapsed: self.clock.now().saturating_sub(active.start),
            duration: active.task.duration(),
            remaining_ms: self.remaining.get()[index(dimension)],
        })
    }
    fn set_remaining(&self, dimension: LightDimension, millis: u64) {
        let mut remaining = self.remaining.get();
        remaining[index(dimension)] = millis;
        self.remaining.set(remaining);
    }
    pub(crate) fn is_ticking(&self) -> bool {
        self.active.borrow().iter().any(Option::is_some)
    }
    /// Current time on the scheduler clock seam, for report pacing.
    pub(crate) fn now(&self) -> u64 {
        self.clock.now()
    }
    pub(crate) fn deadline(&self) -> u64 {
        let now = self.clock.now();
        self.active
            .borrow()
            .iter()
            .flatten()
            .filter(|active| !active.final_submitted)
            .map(|active| active.start.saturating_add(active.task.duration()))
            .fold(now.saturating_add(20), u64::min)
    }
    pub(crate) fn wait_until(&self, deadline: u64) -> Pin<Box<dyn Future<Output = ()> + '_>> {
        self.clock.wait_until(deadline)
    }
}
