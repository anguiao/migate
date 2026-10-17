// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Project CHIP Authors
// Licensed under Apache-2.0: https://www.apache.org/licenses/LICENSE-2.0
//
// Shared lighting scheduler extracted from the Matter lighting layer. It owns
// per-dimension trajectories, action tokens, elapsed-deadline frame
// progression, final-frame handling, cancellation observation, the clock seam
// and remaining-time data for ordinary Function/LightTarget/LightAction
// targets. Protocol decoding and cluster representation stay in Matter.
#[cfg(test)]
mod tests;

pub(crate) mod clock;
pub(crate) mod color;
pub(crate) mod hs;
pub(crate) mod level;

use crate::device::{
    ActionToken, CommandError, Function, LightAction, LightColorTarget, LightDimension,
    LightTarget, NumericRange, brightness_from_matter_level,
};
use clock::Clock;
use futures_util::FutureExt;
use std::{
    cell::{Cell, RefCell},
    future::Future,
    pin::Pin,
    rc::Rc,
};

/// Quiet-report state adapted from rs-matter 0.4.1 Level/Color RemainingTime.
#[derive(Default)]
struct RemainingTime {
    value: u16,
    short_transition: bool,
}
impl RemainingTime {
    // Adapted from LevelControlState::write_remaining_time_quietly and
    // ColorControlState::write_remaining_time_quietly; saturation avoids wrap.
    fn write(&mut self, millis: u64, start: bool, maximum: u16) -> bool {
        // Matter 1.6 Level §1.6.6.3 / Color §3.2.7.4 suppress RemainingTime
        // reports for an entire transition shorter than one second. A longer
        // transition keeps its end/Stop report even when the remainder is short.
        if start {
            self.short_transition = millis > 0 && millis < 1000;
        }
        let new = millis.div_ceil(100).min(maximum as u64) as u16;
        let old = self.value;
        self.value = new;
        !self.short_transition
            && ((new == 0 && old != 0)
                || (old == 0 && new > 10)
                || (start && new.abs_diff(old) > 10))
    }
}

#[derive(Clone, Copy)]
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
    fn target(
        self,
        elapsed: u64,
        range: Option<NumericRange>,
    ) -> Result<(LightTarget, bool), CommandError> {
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
            Self::Color(task) => {
                let (target, end) = task.frame(elapsed);
                let color = match target {
                    color::Target::Xy(x, y) => LightColorTarget::Rgb(color::xy_to_rgb(x, y)),
                    color::Target::Hs(value) => LightColorTarget::Rgb(hs::hs_to_rgb(value)),
                    color::Target::Temperature(mireds) => LightColorTarget::Temperature(
                        range
                            .ok_or(CommandError::UnknownState)?
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
}

struct Active {
    task: Task,
    token: ActionToken,
    action: Rc<LightAction>,
    start: u64,
    last: LightTarget,
    final_submitted: bool,
    accepted: bool,
}

/// Trajectory state exposed for Matter representation and reporting.
#[derive(Clone, Copy)]
pub(crate) struct Observation {
    pub(crate) task: Task,
    /// The startup receipt was committed while this action still owned its dimension.
    pub(crate) accepted: bool,
    pub(crate) final_submitted: bool,
    pub(crate) current: bool,
    pub(crate) ended_ok: bool,
    pub(crate) elapsed: u64,
}

/// The transition where an accepted startup receipt becomes representation metadata.
#[derive(Clone, Copy)]
pub(crate) struct Acceptance {
    pub(crate) task: Task,
    pub(crate) ended_ok: bool,
}

fn index(dimension: LightDimension) -> usize {
    match dimension {
        LightDimension::Brightness => 0,
        LightDimension::Color => 1,
    }
}

/// Per-dimension trajectory scheduler. It works with ordinary Function targets
/// and action tokens and knows nothing about Matter or native groups.
pub(crate) struct Scheduler {
    function: Function,
    range: Option<NumericRange>,
    clock: Rc<dyn Clock>,
    active: RefCell<[Option<Active>; 2]>,
    remaining: RefCell<[RemainingTime; 2]>,
    remaining_revision: Cell<[u64; 2]>,
    remaining_report: Cell<[bool; 2]>,
}
impl Scheduler {
    pub(crate) fn new(
        function: Function,
        range: Option<NumericRange>,
        clock: Rc<dyn Clock>,
    ) -> Self {
        Self {
            function,
            range,
            clock,
            active: RefCell::new([None, None]),
            remaining: RefCell::new([RemainingTime::default(), RemainingTime::default()]),
            remaining_revision: Cell::new([0, 0]),
            remaining_report: Cell::new([false, false]),
        }
    }
    pub(crate) fn begin(
        &self,
        task: Task,
        forced_power: Option<bool>,
    ) -> Result<Rc<LightAction>, CommandError> {
        let (mut target, final_frame) = task.target(0, self.range)?;
        if forced_power.is_some() {
            target.power = forced_power;
        }
        let dimension = task.dimension();
        let allow_unchanged_startup =
            matches!(task, Task::Color(color::Task::Hs(task)) if task.is_achromatic());
        let action = Rc::new(self.function.begin_light_action_with_unchanged_startup(
            dimension,
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
            accepted: false,
        });
        self.update_remaining(dimension, task.duration(), true);
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
                self.update_remaining(dimension, 0, false);
                continue;
            }
            if active.final_submitted {
                continue;
            }
            let elapsed = now.saturating_sub(active.start);
            let (target, final_frame) = active
                .task
                .target(elapsed, self.range)
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
            self.update_remaining(dimension, remaining, false);
        }
    }
    pub(crate) fn stop(&self, dimension: LightDimension) -> Result<(), CommandError> {
        self.function.stop_light(dimension)?;
        self.active.borrow_mut()[index(dimension)] = None;
        self.update_remaining(dimension, 0, false);
        Ok(())
    }
    pub(crate) fn observe(&self, dimension: LightDimension) -> Option<Observation> {
        let actions = self.active.borrow();
        let active = actions[index(dimension)].as_ref()?;
        let ended = active.action.ended().now_or_never();
        Some(Observation {
            task: active.task,
            accepted: active.accepted,
            final_submitted: active.final_submitted,
            current: self.function.light_action_current(active.token),
            ended_ok: matches!(ended, Some(Ok(_))),
            elapsed: self.clock.now().saturating_sub(active.start),
        })
    }
    /// Commit representation metadata ownership for an action whose startup
    /// receipt arrived while it still owned the color dimension.
    pub(crate) fn accept_color(&self) -> Option<Acceptance> {
        let mut actions = self.active.borrow_mut();
        let active = actions[1].as_mut()?;
        let ended = active.action.ended().now_or_never();
        if active.accepted || !active.action.accepted_while_current() {
            return None;
        }
        active.accepted = true;
        Some(Acceptance {
            task: active.task,
            ended_ok: matches!(ended, Some(Ok(_))),
        })
    }
    /// Resume an HS trajectory while retaining the other axis's timeline.
    pub(crate) fn continue_hs(&self, start: hs::Hs) -> hs::HsTask {
        let actions = self.active.borrow();
        match &actions[1] {
            Some(active) if self.function.light_action_current(active.token) => {
                if let Task::Color(color::Task::Hs(task)) = active.task {
                    task.resume(self.clock.now().saturating_sub(active.start))
                } else {
                    hs::HsTask::stationary(start)
                }
            }
            _ => hs::HsTask::stationary(start),
        }
    }
    pub(crate) fn remaining_time(&self, dimension: LightDimension) -> u16 {
        self.remaining.borrow()[index(dimension)].value
    }
    pub(crate) fn remaining_revision(&self, dimension: LightDimension) -> u64 {
        self.remaining_revision.get()[index(dimension)]
    }
    pub(crate) fn take_remaining_report(&self, dimension: LightDimension) -> bool {
        let index = index(dimension);
        let mut dirty = self.remaining_report.get();
        let report = dirty[index];
        dirty[index] = false;
        self.remaining_report.set(dirty);
        report
    }
    fn update_remaining(&self, dimension: LightDimension, millis: u64, start: bool) {
        let index = index(dimension);
        let (old, new, report) = {
            let mut remaining = self.remaining.borrow_mut();
            let remaining = &mut remaining[index];
            let old = remaining.value;
            let maximum = if index == 0 { u16::MAX } else { 0xfffe };
            let report = remaining.write(millis, start, maximum);
            (old, remaining.value, report)
        };
        if old != new {
            let mut revisions = self.remaining_revision.get();
            revisions[index] += 1;
            self.remaining_revision.set(revisions);
        }
        if report {
            let mut dirty = self.remaining_report.get();
            dirty[index] = true;
            self.remaining_report.set(dirty);
        }
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
