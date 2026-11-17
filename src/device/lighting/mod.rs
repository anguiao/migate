// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Project CHIP Authors
// Licensed under Apache-2.0: https://www.apache.org/licenses/LICENSE-2.0
//
// Device-owned trajectories, clocks, action tokens and raw progress. The caller
// supplies coordinates, unit conversion and frame power intent in one plan.
#[cfg(test)]
mod tests;

pub(crate) mod clock;
pub(crate) mod color;
pub(crate) mod hs;
pub(crate) mod level;

use super::{ActionToken, CommandError, Function, LightAction, LightDimension, LightTarget, Rgb};
use clock::Clock;
use futures_util::FutureExt;
use std::{cell::RefCell, future::Future, pin::Pin, rc::Rc};

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
}

/// Maps sampled coordinates to the logical device's physical units.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Units {
    pub(crate) brightness: fn(u8) -> f64,
    pub(crate) temperature: fn(u16) -> f64,
    pub(crate) xy: fn(u16, u16) -> Rgb,
    pub(crate) hs: fn(hs::Hs) -> Rgb,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct FramePower {
    pub(crate) during: Option<bool>,
    pub(crate) final_frame: Option<bool>,
}
impl FramePower {
    fn at(self, final_frame: bool) -> Option<bool> {
        if final_frame {
            self.final_frame
        } else {
            self.during
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ActionPlan {
    pub(crate) task: Task,
    pub(crate) units: Units,
    pub(crate) power: FramePower,
    /// A single-axis command retains the other axis of the current HS action.
    pub(crate) retain_hs: Option<hs::AxisKind>,
}
impl ActionPlan {
    fn target(self, elapsed: u64) -> Result<(LightTarget, bool), CommandError> {
        let (target, end) = match self.task {
            Task::Level(task) => {
                let (level, end) = task.frame(elapsed);
                (
                    LightTarget::brightness(None, (self.units.brightness)(level))?,
                    end,
                )
            }
            Task::Color(task) => {
                let (target, end) = task.frame(elapsed);
                let target = match target {
                    color::Target::Xy(x, y) => LightTarget::rgb(None, (self.units.xy)(x, y)),
                    color::Target::Hs(value) => LightTarget::rgb(None, (self.units.hs)(value)),
                    color::Target::Temperature(value) => {
                        LightTarget::temperature(None, (self.units.temperature)(value))?
                    }
                };
                (target, end)
            }
        };
        Ok((target.with_power(self.power.at(end)), end))
    }
}

struct Active {
    plan: ActionPlan,
    token: ActionToken,
    action: Rc<LightAction>,
    start: u64,
    last: LightTarget,
    final_submitted: bool,
}

/// Raw action facts; protocol encoding and reporting cursors stay with Matter.
#[derive(Clone, Copy)]
pub(crate) struct Observation {
    pub(crate) token: ActionToken,
    pub(crate) task: Task,
    pub(crate) accepted_while_current: bool,
    pub(crate) final_submitted: bool,
    pub(crate) current: bool,
    pub(crate) ended_ok: bool,
    pub(crate) elapsed: u64,
    pub(crate) duration: u64,
    pub(crate) remaining_ms: u64,
}

fn index(dimension: LightDimension) -> usize {
    match dimension {
        LightDimension::Brightness => 0,
        LightDimension::Color => 1,
    }
}

pub(crate) struct Scheduler {
    function: Function,
    clock: Rc<dyn Clock>,
    active: RefCell<[Option<Active>; 2]>,
}
impl Scheduler {
    pub(crate) fn new(function: Function, clock: Rc<dyn Clock>) -> Self {
        Self {
            function,
            clock,
            active: RefCell::new([None, None]),
        }
    }
    pub(crate) fn begin(&self, mut plan: ActionPlan) -> Result<Rc<LightAction>, CommandError> {
        let now = self.clock.now();
        if let (Task::Color(color::Task::Hs(task)), Some(axis)) = (&mut plan.task, plan.retain_hs)
            && let Some(active) = &self.active.borrow()[index(LightDimension::Color)]
            && self.function.light_action_current(active.token)
            && let Task::Color(color::Task::Hs(previous)) = active.plan.task
        {
            task.retain(previous, now.saturating_sub(active.start), axis);
        }
        let (target, final_frame) = plan.target(0)?;
        let dimension = plan.task.dimension();
        let allow_unchanged_startup =
            matches!(plan.task, Task::Color(color::Task::Hs(task)) if task.is_achromatic());
        let action = Rc::new(self.function.begin_light_action_with_unchanged_startup(
            target,
            final_frame,
            allow_unchanged_startup,
        )?);
        self.active.borrow_mut()[index(dimension)] = Some(Active {
            plan,
            token: action.token(),
            action: action.clone(),
            start: now,
            last: target,
            final_submitted: final_frame,
        });
        Ok(action)
    }
    pub(crate) fn advance(&self) {
        let now = self.clock.now();
        for index in 0..2 {
            let mut actions = self.active.borrow_mut();
            let Some(active) = actions[index].as_mut() else {
                continue;
            };
            if !self.function.light_action_current(active.token) {
                actions[index] = None;
                continue;
            }
            if active.final_submitted {
                continue;
            }
            let elapsed = now.saturating_sub(active.start);
            let (target, final_frame) = active
                .plan
                .target(elapsed)
                .expect("validated controller plan");
            if final_frame || target != active.last {
                match self
                    .function
                    .submit_light_frame(active.token, target, final_frame)
                {
                    Ok(()) => {
                        active.last = target;
                        active.final_submitted = final_frame;
                    }
                    Err(error) => {
                        log::warn!("lighting action ended: {error}");
                        actions[index] = None;
                    }
                }
            }
        }
    }
    pub(crate) fn stop(&self, dimension: LightDimension) -> Result<(), CommandError> {
        self.function.stop_light(dimension)?;
        self.active.borrow_mut()[index(dimension)] = None;
        Ok(())
    }
    pub(crate) fn observe(&self, dimension: LightDimension) -> Option<Observation> {
        let actions = self.active.borrow();
        let active = actions[index(dimension)].as_ref()?;
        let ended = active.action.ended().now_or_never();
        let current = self.function.light_action_current(active.token);
        let elapsed = self.clock.now().saturating_sub(active.start);
        let duration = active.plan.task.duration();
        Some(Observation {
            token: active.token,
            task: active.plan.task,
            accepted_while_current: active.action.accepted_while_current(),
            final_submitted: active.final_submitted,
            current,
            ended_ok: matches!(ended, Some(Ok(_))),
            elapsed,
            duration,
            remaining_ms: if current {
                duration.saturating_sub(elapsed)
            } else {
                0
            },
        })
    }
    pub(crate) fn is_ticking(&self) -> bool {
        self.active.borrow().iter().any(Option::is_some)
    }
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
            .map(|active| active.start.saturating_add(active.plan.task.duration()))
            .fold(now.saturating_add(20), u64::min)
    }
    pub(crate) fn wait_until(&self, deadline: u64) -> Pin<Box<dyn Future<Output = ()> + '_>> {
        self.clock.wait_until(deadline)
    }
}
