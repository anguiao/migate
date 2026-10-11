mod controllers;
mod handlers;
#[cfg(test)]
mod tests;

use super::publication::LightPublication;
use crate::device::{
    ActionToken, Command, CommandError, Function, LightAction, LightColorTarget, LightDimension,
    LightMode, LightTarget, NumericRange, brightness_from_matter_level, brightness_to_matter_level,
};
use controllers::{
    clock::{Clock, SystemClock},
    color::{self, ColorControlState},
    hs,
    level::{self, LevelControlState},
};
use futures_lite::future::or;
use futures_util::FutureExt;
pub(super) use handlers::{ColorHandler, LevelHandler, OnOffHandler};
use rs_matter::{
    dm::{Dataver, HandlerContext},
    error::{Error, ErrorCode},
};
use std::{
    cell::{Cell, Ref, RefCell, RefMut},
    rc::Rc,
};

#[derive(Clone, Copy)]
enum Task {
    Level(level::Task),
    Color(color::Task),
}
impl Task {
    fn dimension(self) -> LightDimension {
        match self {
            Self::Level(_) => LightDimension::Brightness,
            Self::Color(_) => LightDimension::Color,
        }
    }
    fn duration(self) -> u64 {
        match self {
            Self::Level(task) => task.duration(),
            Self::Color(task) => task.duration(),
        }
    }
    fn frame(
        self,
        elapsed: u64,
        range: Option<NumericRange>,
    ) -> Result<(LightTarget, bool), Error> {
        match self {
            Self::Level(task) => {
                let (level, end, power) = task.frame(elapsed);
                Ok((
                    LightTarget {
                        power,
                        brightness_percent: Some(
                            brightness_from_matter_level(level).map_err(command_error)?,
                        ),
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
                            .expect("temperature task requires a temperature range")
                            .kelvin_from_mireds(mireds)
                            .map_err(command_error)?,
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
#[derive(Clone, Copy, Default)]
struct Projection {
    power: Option<bool>,
    level: Option<u8>,
    xy: Option<(u16, u16)>,
    temperature: Option<u16>,
    mode: Option<LightMode>,
    hs: Option<hs::Hs>,
    color_mode: Option<u8>,
}
impl Projection {
    fn encoded_hs(self) -> Option<(u8, u8)> {
        self.hs
            .map(|value| (value.hue.round() as u8, value.saturation.round() as u8))
    }
}
pub(super) struct Lighting {
    function: Function,
    endpoint: u16,
    mapping: LightPublication,
    clock: Rc<dyn Clock>,
    level: Option<RefCell<LevelControlState>>,
    color: Option<RefCell<ColorControlState>>,
    active: RefCell<[Option<Active>; 2]>,
    rgb_mode: Cell<u8>,
    hue_hint: Cell<f64>,
    pub(super) on_off_version: Dataver,
    pub(super) level_version: Dataver,
    pub(super) color_version: Dataver,
    versioned: Cell<Projection>,
    observed: Cell<Projection>,
    level_dirty: Cell<bool>,
    color_dirty: Cell<bool>,
    last_level_report: Cell<u64>,
    last_color_report: Cell<u64>,
    remaining_dirty: Cell<[bool; 2]>,
}
impl Drop for Lighting {
    fn drop(&mut self) {
        for active in self.active.get_mut().iter().flatten() {
            self.function.cancel_light_action(active.token);
        }
    }
}
pub(super) fn command_error(error: CommandError) -> Error {
    log::warn!("Matter lighting command failed: {error}");
    match error {
        CommandError::Unsupported => ErrorCode::CommandNotFound,
        CommandError::Timeout => ErrorCode::RxTimeout,
        CommandError::UnknownState => ErrorCode::InvalidState,
        _ => ErrorCode::Failure,
    }
    .into()
}
impl Lighting {
    pub(super) fn new(
        function: Function,
        endpoint: u16,
        on_off: Dataver,
        level: Dataver,
        color: Dataver,
        mapping: LightPublication,
    ) -> Self {
        Self::with_clock(
            function,
            endpoint,
            on_off,
            level,
            color,
            mapping,
            Rc::new(SystemClock::new()),
        )
    }
    fn with_clock(
        function: Function,
        endpoint: u16,
        on_off: Dataver,
        level: Dataver,
        color: Dataver,
        mapping: LightPublication,
        clock: Rc<dyn Clock>,
    ) -> Self {
        let result = Self {
            function,
            endpoint,
            mapping,
            clock,
            level: mapping
                .brightness
                .then(|| RefCell::new(LevelControlState::default())),
            color: mapping
                .has_color_control()
                .then(|| RefCell::new(ColorControlState::default())),
            active: RefCell::new([None, None]),
            rgb_mode: Cell::new(0),
            hue_hint: Cell::new(0.0),
            on_off_version: on_off,
            level_version: level,
            color_version: color,
            versioned: Cell::new(Projection::default()),
            observed: Cell::new(Projection::default()),
            level_dirty: Cell::new(false),
            color_dirty: Cell::new(false),
            last_level_report: Cell::new(0),
            last_color_report: Cell::new(0),
            remaining_dirty: Cell::new([false, false]),
        };
        let projection = result.refresh();
        result.versioned.set(projection);
        result.observed.set(projection);
        result
    }
    pub(super) fn publication(&self) -> LightPublication {
        self.mapping
    }
    fn level_state(&self) -> Ref<'_, LevelControlState> {
        self.level
            .as_ref()
            .expect("level handler owns its level state")
            .borrow()
    }
    fn level_state_mut(&self) -> RefMut<'_, LevelControlState> {
        self.level
            .as_ref()
            .expect("level handler owns its level state")
            .borrow_mut()
    }
    fn color_state(&self) -> Ref<'_, ColorControlState> {
        self.color
            .as_ref()
            .expect("color handler owns its color state")
            .borrow()
    }
    fn color_state_mut(&self) -> RefMut<'_, ColorControlState> {
        self.color
            .as_ref()
            .expect("color handler owns its color state")
            .borrow_mut()
    }
    fn refresh(&self) -> Projection {
        self.accept_color_metadata();
        let snapshot = self.function.snapshot();
        let lighting = snapshot.lighting.as_ref();
        let temperature = self.mapping.temperature.as_ref().and_then(|published| {
            lighting
                .and_then(|lighting| lighting.temperature_kelvin)
                .map(|value| {
                    published
                        .range
                        .mireds_from_kelvin(value)
                        .expect("validated snapshot temperature")
                })
        });
        let (xy, hs) = if self.mapping.rgb {
            match lighting.and_then(|lighting| lighting.mode) {
                Some(LightMode::Temperature) => {
                    let xy = temperature.map(color::temperature_to_xy);
                    let hs = xy.map(|(x, y)| hs::rgb_to_hs(color::xy_to_rgb(x, y)));
                    (xy, hs)
                }
                Some(LightMode::Rgb) => (
                    lighting
                        .and_then(|lighting| lighting.color)
                        .and_then(color::rgb_to_xy),
                    lighting
                        .and_then(|lighting| lighting.color)
                        .map(|rgb| self.rgb_hs(rgb)),
                ),
                None => (None, None),
            }
        } else {
            (None, None)
        };
        let result = Projection {
            power: snapshot.power.value(),
            // Observed zero is a known physical value; Matter's nonzero minimum
            // projects it to MinLevel without inferring the independent power state.
            level: if self.mapping.brightness {
                lighting
                    .and_then(|lighting| lighting.brightness_percent)
                    .map(|value| {
                        brightness_to_matter_level(value.max(1.0))
                            .expect("validated snapshot brightness")
                    })
            } else {
                None
            },
            xy,
            temperature,
            mode: lighting.and_then(|lighting| lighting.mode),
            hs,
            color_mode: lighting
                .and_then(|lighting| lighting.mode)
                .map(|mode| match mode {
                    LightMode::Rgb => self.rgb_mode.get(),
                    LightMode::Temperature => 2,
                }),
        };
        if let Some(level) = &self.level {
            level.borrow_mut().current_level = result.level;
        }
        if let Some(color) = &self.color {
            let mut color = color.borrow_mut();
            color.xy = result.xy;
            color.color_temperature_mireds = result.temperature;
            color.mode = result.mode;
        }
        result
    }
    fn rgb_hs(&self, rgb: crate::device::Rgb) -> hs::Hs {
        let mut value = hs::rgb_to_hs(rgb);
        // RGB cannot encode a hue at zero saturation. Retain accepted hue
        // selections, including a hue-only trajectory through neutral white.
        if let Some(active) = &self.active.borrow()[1]
            && active.accepted
            && let Task::Color(color::Task::Hs(task)) = active.task
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
                || (value.saturation == 0.0 && self.function.light_action_current(active.token)))
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
    fn accept_color_metadata(&self) {
        let mut actions = self.active.borrow_mut();
        let Some(active) = &mut actions[1] else {
            return;
        };
        let ended = active.action.ended().now_or_never();
        if active.accepted || !active.action.accepted_while_current() {
            return;
        }
        // Receipt ownership outlives the Invoke waiter. The runner and getters
        // commit representation metadata even if that waiter has been dropped.
        active.accepted = true;
        match active.task {
            Task::Color(color::Task::Hs(task)) => {
                self.rgb_mode.set(0);
                self.hue_hint.set(
                    task.frame(if matches!(ended, Some(Ok(_))) {
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
    fn hs_start(&self) -> Result<hs::Hs, Error> {
        self.refresh()
            .hs
            .ok_or_else(|| ErrorCode::InvalidState.into())
    }
    fn xy_start(&self) -> Result<(u16, u16), Error> {
        self.refresh()
            .xy
            .ok_or_else(|| ErrorCode::InvalidState.into())
    }
    fn continuing_hs(&self) -> Result<(hs::HsTask, hs::Hs), Error> {
        let start = self.hs_start()?;
        let actions = self.active.borrow();
        let task = match &actions[1] {
            Some(active) if self.function.light_action_current(active.token) => {
                if let Task::Color(color::Task::Hs(task)) = active.task {
                    task.resume(self.clock.now().saturating_sub(active.start))
                } else {
                    hs::HsTask::stationary(start)
                }
            }
            _ => hs::HsTask::stationary(start),
        };
        Ok((task, start))
    }
    fn stop_hs(&self) -> Result<(), Error> {
        let is_hs = self.active.borrow()[1]
            .as_ref()
            .is_some_and(|active| matches!(active.task, Task::Color(color::Task::Hs(_))));
        if is_hs {
            self.stop(LightDimension::Color)?;
        }
        Ok(())
    }
    fn sync_versions(&self) -> Projection {
        let current = self.refresh();
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
    fn update_remaining(&self, dimension: LightDimension, millis: u64, start: bool) {
        let index = if dimension == LightDimension::Brightness {
            0
        } else {
            1
        };
        let (old, new, report) = if index == 0 {
            let Some(level) = &self.level else {
                return;
            };
            let mut level = level.borrow_mut();
            let old = level.remaining_time.value;
            let report = level.remaining_time.write(millis, start, u16::MAX);
            (old, level.remaining_time.value, report)
        } else {
            let Some(color) = &self.color else {
                return;
            };
            let mut color = color.borrow_mut();
            let old = color.remaining_time.value;
            let report = color.remaining_time.write(millis, start, 0xfffe);
            (old, color.remaining_time.value, report)
        };
        if old != new {
            if index == 0 {
                self.level_version.changed();
            } else {
                self.color_version.changed();
            }
        }
        if report {
            let mut dirty = self.remaining_dirty.get();
            dirty[index] = true;
            self.remaining_dirty.set(dirty);
        }
    }
    fn begin(&self, task: Task, forced_power: Option<bool>) -> Result<Rc<LightAction>, Error> {
        self.refresh();
        let (mut target, final_frame) = task.frame(0, self.mapping.temperature.map(|t| t.range))?;
        if forced_power.is_some() {
            target.power = forced_power;
        }
        let dimension = task.dimension();
        let action = Rc::new(
            self.function
                .begin_light_action(dimension, target, final_frame)
                .map_err(command_error)?,
        );
        let index = if dimension == LightDimension::Brightness {
            0
        } else {
            1
        };
        self.active.borrow_mut()[index] = Some(Active {
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
    async fn execute_level(&self, task: Option<level::Task>) -> Result<(), Error> {
        if let Some(task) = task {
            self.begin(Task::Level(task), None)?
                .accepted()
                .await
                .map_err(command_error)?;
        }
        Ok(())
    }
    async fn execute_color(&self, task: color::Task) -> Result<(), Error> {
        let action = self.begin(Task::Color(task), None)?;
        if let color::Task::Hs(task) = task
            && task.duration() > 0
            && task.is_achromatic()
        {
            self.function
                .accept_light_state_only_startup(action.token())
                .map_err(command_error)?;
        }
        action.accepted().await.map_err(command_error)?;
        self.refresh();
        Ok(())
    }
    async fn power(&self, command: Command) -> Result<(), Error> {
        let projection = self.refresh();
        let on = match command {
            Command::On => true,
            Command::Off => false,
            Command::Toggle => !projection.power.ok_or(ErrorCode::InvalidState)?,
        };
        let on_level = self
            .level
            .as_ref()
            .and_then(|level| level.borrow().on_level);
        // Repeated On keeps the existing level. Repeated Off still cancels actions.
        if let Some(preset) = on_level.filter(|_| projection.power != Some(on)) {
            let target = if on { preset } else { 1 };
            let task = level::Task::MoveToLevel {
                with_on_off: false,
                start: target,
                target,
                transition_ms: 0,
            };
            self.begin(Task::Level(task), Some(on))?
                .accepted()
                .await
                .map_err(command_error)?;
        } else {
            self.function
                .execute(if on { Command::On } else { Command::Off })
                .await
                .map_err(command_error)?;
        }
        Ok(())
    }
    fn stop(&self, dimension: LightDimension) -> Result<(), Error> {
        self.refresh();
        self.function.stop_light(dimension).map_err(command_error)?;
        let index = if dimension == LightDimension::Brightness {
            0
        } else {
            1
        };
        self.active.borrow_mut()[index] = None;
        self.update_remaining(dimension, 0, false);
        Ok(())
    }
    fn advance(&self) {
        self.refresh();
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
                .frame(elapsed, self.mapping.temperature.map(|t| t.range))
                .expect("validated controller task");
            let mut remaining = active.task.duration().saturating_sub(elapsed);
            let local_startup = !active.accepted
                && matches!(active.task, Task::Color(color::Task::Hs(task)) if task.is_achromatic());
            if final_frame || target != active.last || local_startup {
                let submitted = self
                    .function
                    .submit_light_frame(active.token, target, final_frame)
                    .and_then(|()| {
                        if local_startup {
                            // A preceding write may have prevented local acceptance
                            // at startup. Recheck after its gate has settled, even
                            // when all subsequent RGB frames are identical.
                            self.function
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
                        log::warn!("Matter lighting action ended: {error}");
                        actions[index] = None;
                        remaining = 0;
                    }
                }
            }
            drop(actions);
            self.update_remaining(dimension, remaining, false);
        }
    }
    fn reports(&self, ctx: &impl HandlerContext) {
        let now = self.clock.now();
        let current = self.sync_versions();
        let old = self.observed.replace(current);
        if old.power != current.power {
            ctx.notify_attr_changed(self.endpoint, 6, 0);
        }
        if old.level != current.level {
            self.level_dirty.set(true);
            if old.level.is_none() || current.level.is_none() {
                self.last_level_report.set(now.saturating_sub(1000));
            }
        }
        let level_end = self.active.borrow()[0]
            .as_ref()
            .is_none_or(|a| a.final_submitted && !self.function.light_action_current(a.token));
        if self.level_dirty.get()
            && (level_end
                || now.saturating_sub(self.last_level_report.get()) >= 1000
                || old.level.is_none()
                || current.level.is_none())
        {
            ctx.notify_attr_changed(self.endpoint, 8, 0);
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
            ctx.notify_attr_changed(self.endpoint, 0x300, 8);
            ctx.notify_attr_changed(self.endpoint, 0x300, 0x4001);
        }
        let color_end = self.active.borrow()[1]
            .as_ref()
            .is_none_or(|a| a.final_submitted && !self.function.light_action_current(a.token));
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
                ctx.notify_attr_changed(self.endpoint, 0x300, 7);
            }
            if self.mapping.rgb {
                for attribute in [0, 1, 3, 4] {
                    ctx.notify_attr_changed(self.endpoint, 0x300, attribute);
                }
            }
            self.color_dirty.set(false);
            self.last_color_report.set(now);
        }
        let dirty = self.remaining_dirty.replace([false, false]);
        if dirty[0] {
            ctx.notify_attr_changed(self.endpoint, 8, 1);
        }
        if dirty[1] {
            ctx.notify_attr_changed(self.endpoint, 0x300, 2);
        }
    }
    pub(super) async fn run(&self, ctx: impl HandlerContext) -> Result<(), Error> {
        let mut snapshots = self.function.subscribe();
        let mut actions = self.function.subscribe_light_actions();
        loop {
            self.advance();
            self.reports(&ctx);
            let ticking = self.active.borrow().iter().any(Option::is_some)
                || self.level_dirty.get()
                || self.color_dirty.get();
            let now = self.clock.now();
            let deadline = self
                .active
                .borrow()
                .iter()
                .flatten()
                .filter(|a| !a.final_submitted)
                .map(|a| a.start.saturating_add(a.task.duration()))
                .fold(now.saturating_add(20), u64::min);
            or(
                or(
                    async {
                        snapshots.changed().await;
                    },
                    async {
                        match &mut actions {
                            Some(actions) => {
                                actions.changed().await;
                            }
                            None => std::future::pending().await,
                        }
                    },
                ),
                async {
                    if ticking {
                        self.clock.wait_until(deadline).await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                },
            )
            .await;
        }
    }
}
