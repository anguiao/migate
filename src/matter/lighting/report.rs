//! Matter lighting report owner.
//!
//! All encoded report state lives here: the attribute projection, HS/XY
//! metadata including the hue hint, RemainingTime encoding and quiet rules,
//! the DataVersion set, and the separate comparison and notification cursors.
//! `Lighting` owns assembly only and calls these operations; the trajectory
//! layer supplies raw samples.

use crate::device::{ActionToken, Function, LightDimension, LightMode, LightProfile, Rgb};
use crate::lighting::{Scheduler, Task, color, hs};
use crate::matter::units::{level_from_brightness, mireds_from_kelvin};
use rs_matter::dm::Dataver;
use std::cell::{Cell, RefCell};

const LEVEL_REMAINING_MAX: u16 = u16::MAX;
const COLOR_REMAINING_MAX: u16 = 0xfffe;

#[derive(Clone, Copy, Default)]
pub(super) struct Projection {
    pub(super) power: Option<bool>,
    pub(super) level: Option<u8>,
    pub(super) xy: Option<(u16, u16)>,
    pub(super) temperature: Option<u16>,
    pub(super) mode: Option<LightMode>,
    pub(super) hs: Option<hs::Hs>,
    pub(super) color_mode: Option<u8>,
}
impl Projection {
    pub(super) fn encoded_hs(self) -> Option<(u8, u8)> {
        self.hs
            .map(|value| (value.hue.round() as u8, value.saturation.round() as u8))
    }
}

/// Quiet-report state adapted from rs-matter 0.4.1 Level/Color RemainingTime.
#[derive(Default)]
pub(super) struct RemainingTime {
    pub(super) value: u16,
    short_transition: bool,
}
impl RemainingTime {
    // Adapted from LevelControlState::write_remaining_time_quietly and
    // ColorControlState::write_remaining_time_quietly; saturation avoids wrap.
    pub(super) fn write(&mut self, millis: u64, start: bool, maximum: u16) -> bool {
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

/// The single owner of Matter report state. Comparison and notification
/// cursors stay separate but inside this owner.
pub(super) struct Report {
    pub(super) on_off_version: Dataver,
    pub(super) level_version: Dataver,
    pub(super) color_version: Dataver,
    versioned: Cell<Projection>,
    observed: Cell<Projection>,
    level_dirty: Cell<bool>,
    color_dirty: Cell<bool>,
    last_level_report: Cell<u64>,
    last_color_report: Cell<u64>,
    remaining: RefCell<[RemainingTime; 2]>,
    remaining_token: Cell<[Option<ActionToken>; 2]>,
    remaining_report: Cell<[bool; 2]>,
    rgb_mode: Cell<u8>,
    hue_hint: Cell<f64>,
    accepted_color: Cell<Option<ActionToken>>,
}
impl Report {
    pub(super) fn new(on_off: Dataver, level: Dataver, color: Dataver) -> Self {
        Self {
            on_off_version: on_off,
            level_version: level,
            color_version: color,
            versioned: Cell::new(Projection::default()),
            observed: Cell::new(Projection::default()),
            level_dirty: Cell::new(false),
            color_dirty: Cell::new(false),
            last_level_report: Cell::new(0),
            last_color_report: Cell::new(0),
            remaining: RefCell::new([RemainingTime::default(), RemainingTime::default()]),
            remaining_token: Cell::new([None, None]),
            remaining_report: Cell::new([false, false]),
            rgb_mode: Cell::new(0),
            hue_hint: Cell::new(0.0),
            accepted_color: Cell::new(None),
        }
    }
    /// Retained cluster DataVersion bump rule owned by the report layer.
    pub(super) fn bump_version(&self, cluster: Option<u32>, profile: LightProfile) {
        if cluster.is_none_or(|id| id == 6) {
            self.on_off_version.changed();
        }
        if profile.has_brightness() && cluster.is_none_or(|id| id == 8) {
            self.level_version.changed();
        }
        if profile.temperature_interval().is_some() && cluster.is_none_or(|id| id == 0x300) {
            self.color_version.changed();
        }
    }
    /// Whether a quiet report is waiting for its next flush decision.
    pub(super) fn is_pending(&self) -> bool {
        self.level_dirty.get() || self.color_dirty.get()
    }
    pub(super) fn reset(&self, projection: Projection) {
        self.versioned.set(projection);
        self.observed.set(projection);
    }
    pub(super) fn refresh(
        &self,
        function: &Function,
        scheduler: &Scheduler,
        profile: LightProfile,
    ) -> Projection {
        self.note_acceptance(scheduler);
        let snapshot = function.snapshot();
        let lighting = snapshot.lighting().expect("lighting Function snapshot");
        let interval = profile.temperature_interval();
        let temperature = interval.and_then(|interval| {
            lighting
                .temperature_kelvin
                .map(|value| mireds_from_kelvin(interval, value))
        });
        let (xy, hs) = if profile.has_rgb() {
            match lighting.mode {
                Some(LightMode::Temperature) => {
                    let xy = temperature.map(color::temperature_to_xy);
                    let hs = xy.map(|(x, y)| hs::rgb_to_hs(color::xy_to_rgb(x, y)));
                    (xy, hs)
                }
                Some(LightMode::Rgb) => (
                    lighting.color.and_then(color::rgb_to_xy),
                    lighting.color.map(|rgb| self.rgb_hs(scheduler, rgb)),
                ),
                None => (None, None),
            }
        } else {
            (None, None)
        };
        Projection {
            power: snapshot.power().and_then(crate::device::Power::value),
            // Observed zero is a known physical value; Matter's nonzero minimum
            // projects it to MinLevel without inferring the independent power state.
            level: lighting
                .brightness_percent
                .map(|value| level_from_brightness(value.max(1.0))),
            xy,
            temperature,
            mode: lighting.mode,
            hs,
            color_mode: lighting.mode.map(|mode| match mode {
                LightMode::Rgb => self.rgb_mode.get(),
                LightMode::Temperature => 2,
            }),
        }
    }
    fn rgb_hs(&self, scheduler: &Scheduler, rgb: Rgb) -> hs::Hs {
        let mut value = hs::rgb_to_hs(rgb);
        // RGB cannot encode a hue at zero saturation. Retain accepted hue
        // selections, including a hue-only trajectory through neutral white.
        if let Some(observation) = scheduler.observe(LightDimension::Color)
            && self.accepted_color.get() == Some(observation.token)
            && let Task::Color(color::Task::Hs(task)) = observation.task
        {
            let finished = observation.final_submitted && observation.ended_ok;
            let elapsed = if finished {
                task.duration()
            } else {
                observation.elapsed
            };
            let requested = task.frame(elapsed).0;
            if (finished || (value.saturation == 0.0 && observation.current))
                && hs::hs_to_rgb(requested) == rgb
            {
                self.hue_hint.set(requested.hue);
            }
        }
        if value.saturation == 0.0 || (value.hue == 0.0 && self.hue_hint.get() == 254.0) {
            value.hue = self.hue_hint.get();
        } else {
            self.hue_hint.set(value.hue);
        }
        value
    }
    /// Consume each accepted startup receipt once, keyed by action identity.
    /// Receipt ownership outlives the Invoke waiter: the runner and getters
    /// commit representation metadata even if that waiter has been dropped.
    /// This runs before the scheduler advances or stops, so late final and
    /// cancelled receipts are still observed while the action record exists.
    fn note_acceptance(&self, scheduler: &Scheduler) {
        let Some(observation) = scheduler.observe(LightDimension::Color) else {
            return;
        };
        if !observation.accepted_while_current
            || self.accepted_color.get() == Some(observation.token)
        {
            return;
        }
        self.accepted_color.set(Some(observation.token));
        match observation.task {
            Task::Color(color::Task::Hs(task)) => {
                self.rgb_mode.set(0);
                self.hue_hint.set(
                    task.frame(if observation.ended_ok {
                        task.duration()
                    } else {
                        0
                    })
                    .0
                    .hue,
                );
            }
            Task::Color(color::Task::XyMoveTo { .. } | color::Task::XyMove { .. }) => {
                self.rgb_mode.set(1);
            }
            _ => {}
        }
    }
    pub(super) fn sync_versions(
        &self,
        function: &Function,
        scheduler: &Scheduler,
        profile: LightProfile,
    ) -> Projection {
        self.sync_remaining(scheduler);
        let current = self.refresh(function, scheduler, profile);
        let old = self.versioned.replace(current);
        // The generated handler filters by DataVersion before calling a getter.
        // Version the encoded snapshot now; quiet reporting has its own cursor.
        if old.power != current.power {
            self.on_off_version.changed();
        }
        if old.level != current.level {
            self.level_version.changed();
        }
        if old.xy != current.xy
            || old.temperature != current.temperature
            || old.mode != current.mode
            || old.encoded_hs() != current.encoded_hs()
            || old.color_mode != current.color_mode
        {
            self.color_version.changed();
        }
        current
    }
    /// Encode raw trajectory progress into Matter RemainingTime, bumping the
    /// cluster DataVersion and report cursor on encoded changes.
    pub(super) fn sync_remaining(&self, scheduler: &Scheduler) {
        for (index, dimension) in [(0, LightDimension::Brightness), (1, LightDimension::Color)] {
            let observation = scheduler.observe(dimension);
            let start = observation
                .is_some_and(|value| self.remaining_token.get()[index] != Some(value.token));
            if let Some(value) = observation.filter(|_| start) {
                let mut tokens = self.remaining_token.get();
                tokens[index] = Some(value.token);
                self.remaining_token.set(tokens);
            }
            let maximum = if index == 0 {
                LEVEL_REMAINING_MAX
            } else {
                COLOR_REMAINING_MAX
            };
            let mut report = false;
            let changed = {
                let mut remaining = self.remaining.borrow_mut();
                let remaining = &mut remaining[index];
                let old = remaining.value;
                if start {
                    // Encode the whole transition first so the short-transition
                    // rule classifies the requested duration, matching the
                    // original begin/advance update order.
                    let duration = observation.map(|value| value.duration).unwrap_or(0);
                    let millis = observation.map(|value| value.remaining_ms).unwrap_or(0);
                    report |= remaining.write(duration, true, maximum);
                    if millis != duration {
                        report |= remaining.write(millis, false, maximum);
                    }
                } else {
                    let millis = observation.map(|value| value.remaining_ms).unwrap_or(0);
                    report |= remaining.write(millis, false, maximum);
                }
                old != remaining.value
            };
            if changed {
                if index == 0 {
                    self.level_version.changed();
                } else {
                    self.color_version.changed();
                }
            }
            if report {
                let mut dirty = self.remaining_report.get();
                dirty[index] = true;
                self.remaining_report.set(dirty);
            }
        }
    }
    pub(super) fn remaining_time(&self, dimension: LightDimension) -> u16 {
        let index = match dimension {
            LightDimension::Brightness => 0,
            LightDimension::Color => 1,
        };
        self.remaining.borrow()[index].value
    }
    fn take_remaining_report(&self, index: usize) -> bool {
        let mut dirty = self.remaining_report.get();
        let report = dirty[index];
        dirty[index] = false;
        self.remaining_report.set(dirty);
        report
    }
    /// Emit the compact `(cluster, attribute)` notification list for this
    /// report interval. The protocol context is wired by `Lighting::reports`.
    pub(super) fn reports(
        &self,
        function: &Function,
        scheduler: &Scheduler,
        profile: LightProfile,
        mut emit: impl FnMut(u32, u32),
    ) {
        let now = scheduler.now();
        let current = self.sync_versions(function, scheduler, profile);
        let old = self.observed.replace(current);
        if old.power != current.power {
            emit(6, 0);
        }
        if old.level != current.level {
            self.level_dirty.set(true);
            if old.level.is_none() || current.level.is_none() {
                self.last_level_report.set(now.saturating_sub(1000));
            }
        }
        let level_end = scheduler
            .observe(LightDimension::Brightness)
            .is_none_or(|observation| observation.final_submitted && !observation.current);
        if self.level_dirty.get()
            && (level_end
                || now.saturating_sub(self.last_level_report.get()) >= 1000
                || old.level.is_none()
                || current.level.is_none())
        {
            emit(8, 0);
            self.level_dirty.set(false);
            self.last_level_report.set(now);
        }
        let mode_changed = old.color_mode != current.color_mode;
        let value_changed = match current.mode {
            Some(LightMode::Temperature) => old.temperature != current.temperature,
            Some(LightMode::Rgb) => {
                old.xy != current.xy || old.encoded_hs() != current.encoded_hs()
            }
            None => false,
        };
        if mode_changed || value_changed {
            self.color_dirty.set(true);
        }
        if mode_changed {
            emit(0x300, 8);
            emit(0x300, 0x4001);
        }
        let color_end = scheduler
            .observe(LightDimension::Color)
            .is_none_or(|observation| observation.final_submitted && !observation.current);
        let unknown_edge = match current.mode {
            Some(LightMode::Temperature) => {
                value_changed && (old.temperature.is_none() || current.temperature.is_none())
            }
            Some(LightMode::Rgb) => {
                (old.xy != current.xy && (old.xy.is_none() || current.xy.is_none()))
                    || (old.hs != current.hs && (old.hs.is_none() || current.hs.is_none()))
            }
            None => false,
        };
        if self.color_dirty.get()
            && (mode_changed
                || color_end
                || now.saturating_sub(self.last_color_report.get()) >= 1000
                || unknown_edge)
        {
            // A mode boundary reannounces its current color even when a
            // numeric encoding is unchanged. HS/XY also describe active CT;
            // inactive device RGB memory never becomes a color report.
            if current.mode == Some(LightMode::Temperature) {
                emit(0x300, 7);
            }
            if profile.has_rgb() {
                for attribute in [0, 1, 3, 4] {
                    emit(0x300, attribute);
                }
            }
            self.color_dirty.set(false);
            self.last_color_report.set(now);
        }
        if self.take_remaining_report(0) {
            emit(8, 1);
        }
        if self.take_remaining_report(1) {
            emit(0x300, 2);
        }
    }
}
