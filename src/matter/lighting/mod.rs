mod controllers;
mod handlers;
#[cfg(test)]
mod tests;

pub(super) use handlers::{ColorHandler, LevelHandler, OnOffHandler};

use super::publication::LightPublication;
use crate::device::{
    Command, CommandError, Function, Group, Light, LightAction, LightDimension, LightMode,
    brightness_to_matter_level,
};
use crate::lighting::{
    LightingExecutor, Plan, Transition,
    controllers::{color, hs, level},
    group::Fanout,
    planning::{ColorPlan, ColorState, LevelPlan},
};
use controllers::{clock::Clock, color::ColorControlState, level::LevelControlState};
use futures_lite::future::or;
use futures_util::future::select_all;
use rs_matter::{
    dm::{Dataver, HandlerContext},
    error::{Error, ErrorCode},
};
use std::{
    cell::{Cell, Ref, RefCell, RefMut},
    future::Future,
    pin::Pin,
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
/// The Matter endpoint view. It owns protocol state, versions and reporting
/// cursors only; committed actions and their progress live in the shared
/// `LightingExecutor` and are never cancelled by dropping this object.
pub(super) struct Lighting {
    target: Light,
    endpoint: u16,
    mapping: LightPublication,
    clock: Rc<dyn Clock>,
    executor: Rc<LightingExecutor>,
    level: Option<RefCell<LevelControlState>>,
    color: Option<RefCell<ColorControlState>>,
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
        target: Light,
        endpoint: u16,
        on_off: Dataver,
        level: Dataver,
        color: Dataver,
        mapping: LightPublication,
        executor: Rc<LightingExecutor>,
    ) -> Self {
        let clock = executor.clock().clone();
        let result = Self {
            target,
            endpoint,
            mapping,
            clock,
            executor,
            level: mapping
                .brightness
                .then(|| RefCell::new(LevelControlState::default())),
            color: mapping
                .has_color_control()
                .then(|| RefCell::new(ColorControlState::default())),
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
        result.progress();
        result
    }

    fn function(&self) -> &Function {
        self.target.function().expect("single lighting target")
    }

    pub(super) fn group(&self) -> Option<&Group> {
        self.target.group()
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
        for key in self.target.functions().iter().map(Function::function_key) {
            self.executor.accept_color_metadata(key);
        }
        let state = self.target.state();
        let representative = state.representative.clone();
        let lighting = state.lighting.as_ref();
        let color = self.executor.observed_color(
            representative.as_ref(),
            state.lighting,
            self.mapping.temperature.map(|published| published.range),
        );
        let temperature = color.temperature_mireds;
        let (xy, hs) = if self.mapping.rgb {
            (color.xy, color.hs)
        } else {
            (None, None)
        };
        let result = Projection {
            power: state.power.value(),
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
                    LightMode::Rgb => representative
                        .as_ref()
                        .map_or(0, |key| self.executor.rgb_mode(key)),
                    LightMode::Temperature => 2,
                }),
        };
        if let Some(level) = &self.level {
            level.borrow_mut().current_level = result.level;
        }
        result
    }
    async fn stop_hs(&self) -> Result<(), Error> {
        if let Some(group) = self.target.group() {
            let dispatch = self
                .executor
                .fanout(group, Fanout::Color(ColorPlan::StopHs), None);
            dispatch.accepted().await.map_err(command_error)?;
            self.progress();
            return Ok(());
        }
        if self
            .executor
            .stop_hs(self.function())
            .map_err(command_error)?
        {
            self.progress();
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
    fn update_remaining(&self, dimension: LightDimension, transition: Option<Transition>) {
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
            let report = level
                .remaining_time
                .write(transition, self.clock.now(), u16::MAX);
            (old, level.remaining_time.value, report)
        } else {
            let Some(color) = &self.color else {
                return;
            };
            let mut color = color.borrow_mut();
            let old = color.remaining_time.value;
            let report = color
                .remaining_time
                .write(transition, self.clock.now(), 0xfffe);
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
    /// Adopt the executor-owned progress into the Matter RemainingTime
    /// attributes. This is the only remaining-time source.
    fn progress(&self) {
        let functions = self.target.functions();
        for dimension in [LightDimension::Brightness, LightDimension::Color] {
            self.update_remaining(
                dimension,
                self.executor
                    .transition(functions.iter().map(Function::function_key), dimension),
            );
        }
    }
    fn ending(&self, dimension: LightDimension) -> bool {
        match &self.target {
            Light::Single(function) => self.executor.ending(function.function_key(), dimension),
            Light::Group(group) => group
                .keys()
                .iter()
                .all(|key| self.executor.ending(key, dimension)),
        }
    }
    fn begin(&self, plan: Plan, forced_power: Option<bool>) -> Result<Rc<LightAction>, Error> {
        self.refresh();
        let action = self
            .executor
            .begin(self.function(), plan, forced_power)
            .map_err(command_error)?;
        self.progress();
        Ok(action)
    }
    async fn execute_level(&self, plan: Option<LevelPlan>) -> Result<(), Error> {
        if let Some(plan) = plan {
            if let Some(group) = self.group() {
                return self.execute_group(group, Fanout::Level(plan), None).await;
            }
            let task = plan
                .resolve(self.level_state().current_level)
                .map_err(command_error)?;
            self.begin(Plan::Level(task), None)?
                .accepted()
                .await
                .map_err(command_error)?;
        }
        Ok(())
    }
    async fn execute_color_plan(&self, plan: ColorPlan) -> Result<(), Error> {
        if let Some(group) = self.group() {
            return self.execute_group(group, Fanout::Color(plan), None).await;
        }
        let current = self.refresh();
        let task = plan
            .resolve(ColorState {
                temperature_mireds: current.temperature,
                xy: current.xy,
                hs: current.hs,
                continuing_hs: self.executor.resumed_hs(self.function().function_key()),
            })
            .map_err(command_error)?;
        self.execute_color(task).await
    }
    async fn execute_color(&self, task: color::Task) -> Result<(), Error> {
        let action = self.begin(
            Plan::Color {
                task,
                temperature: self.mapping.temperature.map(|published| published.range),
            },
            None,
        )?;
        action.accepted().await.map_err(command_error)?;
        self.refresh();
        Ok(())
    }
    /// Commit one normalized group command and wait for early success or
    /// overall failure. The current member set, starts and bounds were fixed
    /// synchronously inside the executor before any await.
    async fn execute_group(
        &self,
        group: &Group,
        command: Fanout,
        forced_power: Option<bool>,
    ) -> Result<(), Error> {
        let dispatch = self.executor.fanout(group, command, forced_power);
        self.progress();
        dispatch.accepted().await.map_err(command_error)
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
            if let Some(group) = self.target.group() {
                // OnLevel and the definite power are interpreted once; hidden
                // member endpoint settings never take part.
                let plan = LevelPlan::MoveTo {
                    target,
                    transition_ms: 0,
                    with_on_off: false,
                };
                return self
                    .execute_group(group, Fanout::Level(plan), Some(on))
                    .await;
            }
            let task = level::Task::MoveToLevel {
                with_on_off: false,
                start: target,
                target,
                transition_ms: 0,
            };
            self.begin(Plan::Level(task), Some(on))?
                .accepted()
                .await
                .map_err(command_error)?;
        } else if let Some(group) = self.target.group() {
            self.execute_group(group, Fanout::Power(on), None).await?;
        } else {
            let target = if on { Command::On } else { Command::Off };
            // The executor owns this dispatch so a dropped invoke waiter
            // cannot withdraw an unsent power command.
            self.executor
                .dispatch_power(self.function(), target)
                .accepted()
                .await
                .map_err(command_error)?;
        }
        Ok(())
    }
    async fn stop(&self, dimension: LightDimension) -> Result<(), Error> {
        if let Some(group) = self.target.group() {
            let command = match dimension {
                LightDimension::Brightness => Fanout::Level(LevelPlan::Stop),
                LightDimension::Color => Fanout::Color(ColorPlan::Stop),
            };
            let dispatch = self.executor.fanout(group, command, None);
            self.progress();
            return dispatch.accepted().await.map_err(command_error);
        }
        self.executor
            .stop(self.function(), dimension)
            .map_err(command_error)?;
        self.progress();
        Ok(())
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
        let level_end = self.ending(LightDimension::Brightness);
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
        let color_end = self.ending(LightDimension::Color);
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
            // inactive device RGB memory never becomes a color report. A CT
            // value that became unknown still dirties the attribute so a
            // subscriber re-reads and gets its specification-level unknown
            // (a non-nullable attribute reports a read failure, not a default).
            if current.mode == Some(LightMode::Temperature)
                || (old.mode == Some(LightMode::Temperature) && current.mode.is_none())
            {
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
        loop {
            // Every applied member snapshot or action change wakes reporting.
            // Membership remains fixed until the Matter instance is rebuilt.
            let waits: Vec<Pin<Box<dyn Future<Output = ()>>>> = self
                .target
                .functions()
                .iter()
                .flat_map(|function| {
                    let snapshot = {
                        let mut changes = function.subscribe();
                        Box::pin(async move {
                            changes.changed().await;
                        }) as Pin<Box<dyn Future<Output = ()>>>
                    };
                    let actions = function.subscribe_light_actions().map(|mut changes| {
                        Box::pin(async move {
                            changes.changed().await;
                        }) as Pin<Box<dyn Future<Output = ()>>>
                    });
                    [Some(snapshot), actions]
                })
                .flatten()
                .collect();
            self.progress();
            self.reports(&ctx);
            let ticking = self
                .target
                .functions()
                .iter()
                .any(|function| self.executor.has_active(function.function_key()))
                || self.level_dirty.get()
                || self.color_dirty.get();
            // The report loop only wakes on its own 20ms cadence, device
            // snapshots and light-action changes. It must never wait on an
            // executor deadline that is already in the past: an immediately
            // ready timer would spin this loop and starve the executor runner
            // sharing the same single-threaded `or`.
            let deadline = self.clock.now().saturating_add(20);
            or(
                async {
                    select_all(waits).await;
                },
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
