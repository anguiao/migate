mod handlers;
mod settings;
#[cfg(test)]
mod tests;

use crate::device::{
    Command, CommandError, Function, LightAction, LightDimension, LightMode, LightProfile,
    NumericRange, brightness_to_matter_level,
};
use crate::lighting::{
    Scheduler, Task,
    clock::{Clock, SystemClock},
    color, hs, level,
};
use futures_lite::future::or;
pub(super) use handlers::{ColorHandler, LevelHandler, OnOffHandler};
use rs_matter::{
    dm::{Dataver, HandlerContext},
    error::{Error, ErrorCode},
};
use settings::{color::ColorControlState, level::LevelControlState};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

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
    profile: LightProfile,
    range: Option<NumericRange>,
    bounds: Option<(u16, u16)>,
    scheduler: Scheduler,
    level: RefCell<LevelControlState>,
    color: RefCell<ColorControlState>,
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
    remaining_versions: Cell<[u64; 2]>,
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
    ) -> Result<Self, Error> {
        Self::with_clock(
            function,
            endpoint,
            on_off,
            level,
            color,
            Rc::new(SystemClock::new()),
        )
    }
    fn with_clock(
        function: Function,
        endpoint: u16,
        on_off: Dataver,
        level: Dataver,
        color: Dataver,
        clock: Rc<dyn Clock>,
    ) -> Result<Self, Error> {
        let description = function.description();
        let profile = description
            .light_profile()
            .map_err(|_| ErrorCode::InvalidState)?;
        let range = description.temperature_range();
        let bounds = range
            .map(|range| range.mired_bounds().map_err(command_error))
            .transpose()?;
        let scheduler = Scheduler::new(function.clone(), range, clock);
        let result = Self {
            function,
            endpoint,
            profile,
            range,
            bounds,
            scheduler,
            level: RefCell::new(LevelControlState::default()),
            color: RefCell::new(ColorControlState::default()),
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
            remaining_versions: Cell::new([0, 0]),
        };
        let projection = result.refresh();
        result.versioned.set(projection);
        result.observed.set(projection);
        Ok(result)
    }
    pub(super) fn rgb(&self) -> bool {
        self.profile == LightProfile::ExtendedColor
    }
    pub(super) fn ct(&self) -> bool {
        matches!(
            self.profile,
            LightProfile::ColorTemperature | LightProfile::ExtendedColor
        )
    }
    pub(super) fn level(&self) -> bool {
        matches!(
            self.profile,
            LightProfile::Dimmable | LightProfile::ColorTemperature | LightProfile::ExtendedColor
        )
    }
    pub(super) fn profile(&self) -> LightProfile {
        self.profile
    }
    fn refresh(&self) -> Projection {
        self.accept_color_metadata();
        let snapshot = self.function.snapshot();
        let lighting = snapshot
            .lighting
            .as_ref()
            .expect("lighting Function snapshot");
        let temperature = self.range.and_then(|range| {
            lighting.temperature_kelvin.map(|value| {
                range
                    .mireds_from_kelvin(value)
                    .expect("validated snapshot temperature")
            })
        });
        let (xy, hs) = if self.rgb() {
            match lighting.mode {
                Some(LightMode::Temperature) => {
                    let xy = temperature.map(color::temperature_to_xy);
                    let hs = xy.map(|(x, y)| hs::rgb_to_hs(color::xy_to_rgb(x, y)));
                    (xy, hs)
                }
                Some(LightMode::Rgb) => (
                    lighting.color.and_then(color::rgb_to_xy),
                    lighting.color.map(|rgb| self.rgb_hs(rgb)),
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
            level: lighting.brightness_percent.map(|value| {
                brightness_to_matter_level(value.max(1.0)).expect("validated snapshot brightness")
            }),
            xy,
            temperature,
            mode: lighting.mode,
            hs,
            color_mode: lighting.mode.map(|mode| match mode {
                LightMode::Rgb => self.rgb_mode.get(),
                LightMode::Temperature => 2,
            }),
        };
        self.level.borrow_mut().current_level = result.level;
        let mut color = self.color.borrow_mut();
        color.xy = result.xy;
        color.color_temperature_mireds = result.temperature;
        color.mode = result.mode;
        result
    }
    fn rgb_hs(&self, rgb: crate::device::Rgb) -> hs::Hs {
        let mut value = hs::rgb_to_hs(rgb);
        // RGB cannot encode a hue at zero saturation. Retain accepted hue
        // selections, including a hue-only trajectory through neutral white.
        if let Some(observation) = self.scheduler.observe(LightDimension::Color)
            && observation.accepted
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
    fn accept_color_metadata(&self) {
        let Some(acceptance) = self.scheduler.accept_color() else {
            return;
        };
        // Receipt ownership outlives the Invoke waiter. The runner and getters
        // commit representation metadata even if that waiter has been dropped.
        match acceptance.task {
            Task::Color(color::Task::Hs(task)) => {
                self.rgb_mode.set(0);
                self.hue_hint.set(
                    task.frame(if acceptance.ended_ok {
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
        Ok((self.scheduler.continue_hs(start), start))
    }
    fn stop_hs(&self) -> Result<(), Error> {
        let is_hs = self
            .scheduler
            .observe(LightDimension::Color)
            .is_some_and(|observation| matches!(observation.task, Task::Color(color::Task::Hs(_))));
        if is_hs {
            self.stop(LightDimension::Color)?;
        }
        Ok(())
    }
    fn sync_versions(&self) -> Projection {
        self.sync_remaining_versions();
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
    fn sync_remaining_versions(&self) {
        for (index, dimension) in [(0, LightDimension::Brightness), (1, LightDimension::Color)] {
            let revision = self.scheduler.remaining_revision(dimension);
            if revision != self.remaining_versions.get()[index] {
                if index == 0 {
                    self.level_version.changed();
                } else {
                    self.color_version.changed();
                }
                let mut versions = self.remaining_versions.get();
                versions[index] = revision;
                self.remaining_versions.set(versions);
            }
        }
    }
    fn remaining_time(&self, dimension: LightDimension) -> u16 {
        self.scheduler.remaining_time(dimension)
    }
    fn begin(&self, task: Task, forced_power: Option<bool>) -> Result<Rc<LightAction>, Error> {
        self.refresh();
        self.scheduler
            .begin(task, forced_power)
            .map_err(command_error)
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
        let on_level = self.level.borrow().on_level;
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
        self.scheduler.stop(dimension).map_err(command_error)
    }
    fn advance(&self) {
        self.refresh();
        self.scheduler.advance();
    }
    fn reports(&self, ctx: &impl HandlerContext) {
        let now = self.scheduler.now();
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
        let level_end = self
            .scheduler
            .observe(LightDimension::Brightness)
            .is_none_or(|observation| observation.final_submitted && !observation.current);
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
        let color_end = self
            .scheduler
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
                ctx.notify_attr_changed(self.endpoint, 0x300, 7);
            }
            if self.rgb() {
                for attribute in [0, 1, 3, 4] {
                    ctx.notify_attr_changed(self.endpoint, 0x300, attribute);
                }
            }
            self.color_dirty.set(false);
            self.last_color_report.set(now);
        }
        if self
            .scheduler
            .take_remaining_report(LightDimension::Brightness)
        {
            ctx.notify_attr_changed(self.endpoint, 8, 1);
        }
        if self.scheduler.take_remaining_report(LightDimension::Color) {
            ctx.notify_attr_changed(self.endpoint, 0x300, 2);
        }
    }
    pub(super) async fn run(&self, ctx: impl HandlerContext) -> Result<(), Error> {
        let mut snapshots = self.function.subscribe();
        let mut actions = self
            .function
            .subscribe_light_actions()
            .expect("lighting action subscription");
        loop {
            self.advance();
            self.reports(&ctx);
            let ticking =
                self.scheduler.is_ticking() || self.level_dirty.get() || self.color_dirty.get();
            let deadline = self.scheduler.deadline();
            or(
                or(
                    async {
                        snapshots.changed().await;
                    },
                    async {
                        actions.changed().await;
                    },
                ),
                async {
                    if ticking {
                        self.scheduler.wait_until(deadline).await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                },
            )
            .await;
        }
    }
}
