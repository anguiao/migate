mod dispatch;
pub(super) use dispatch::{Dispatch, IDENTIFY};
mod handlers;
mod plan;
mod report;
#[cfg(test)]
mod tests;

use super::units;
use crate::device::{Command, CommandError, Function, LightAction, LightDimension, LightProfile};
use crate::lighting::{Scheduler, Task, clock::Clock};
use futures_lite::future::or;
pub(super) use handlers::{ColorHandler, LevelHandler, OnOffHandler};
use report::{Projection, Report};
use rs_matter::{
    dm::{Dataver, HandlerContext},
    error::{Error, ErrorCode},
};
use std::{cell::Cell, rc::Rc};

/// Assembly for one published light: Function, trajectory Scheduler and
/// cluster Settings, with the Matter Report owner alongside.
pub(super) struct Lighting {
    function: Function,
    endpoint: u16,
    profile: LightProfile,
    scheduler: Scheduler,
    level_options: Cell<u8>,
    on_level: Cell<Option<u8>>,
    color_options: Cell<u8>,
    report: Report,
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
fn plan_error(error: plan::PlanError) -> Error {
    match error {
        plan::PlanError::InvalidCommand => ErrorCode::InvalidCommand,
        plan::PlanError::Constraint => ErrorCode::ConstraintError,
        plan::PlanError::InvalidState => ErrorCode::InvalidState,
        plan::PlanError::Unsupported => ErrorCode::CommandNotFound,
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
            Rc::new(crate::lighting::clock::SystemClock::new()),
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
            .kind
            .light_profile()
            .ok_or(ErrorCode::InvalidState)?;
        let scheduler = Scheduler::new(
            function.clone(),
            clock,
            units::brightness_from_level,
            units::kelvin_from_mireds,
        );
        let result = Self {
            function,
            endpoint,
            profile,
            scheduler,
            level_options: Cell::new(0),
            on_level: Cell::new(None),
            color_options: Cell::new(0),
            report: Report::new(on_off, level, color),
        };
        let projection = result.refresh();
        result.report.reset(projection);
        Ok(result)
    }
    pub(super) fn rgb(&self) -> bool {
        self.profile.has_rgb()
    }
    pub(super) fn ct(&self) -> bool {
        self.profile.temperature_interval().is_some()
    }
    pub(super) fn level(&self) -> bool {
        self.profile.has_brightness()
    }
    pub(super) fn profile(&self) -> LightProfile {
        self.profile
    }
    pub(super) fn bounds(&self) -> Option<(u16, u16)> {
        self.profile.temperature_interval().map(units::mired_bounds)
    }
    /// Retained cluster DataVersion bump for the fixed model. The report owner
    /// keeps the counters; callers do not reach into its fields.
    pub(super) fn bump_version(&self, cluster: Option<u32>) {
        self.report.bump_version(cluster, self.profile);
    }
    fn plan_input(&self) -> plan::Input {
        plan::Input {
            profile: self.profile,
            settings: self.settings(),
            state: self.plan_state(),
            trajectory: self.color_trajectory(),
        }
    }
    fn settings(&self) -> plan::Settings {
        plan::Settings {
            level_options: self.level_options.get(),
            color_options: self.color_options.get(),
            on_level: self.on_level.get(),
        }
    }
    fn plan_state(&self) -> plan::State {
        let projection = self.refresh();
        plan::State {
            power: projection.power,
            level: projection.level,
            temperature: projection.temperature,
            xy: projection.xy,
            hs: projection.hs,
        }
    }
    fn color_trajectory(&self) -> Option<plan::Trajectory> {
        let observation = self.scheduler.observe(LightDimension::Color)?;
        let Task::Color(task) = observation.task else {
            return None;
        };
        Some(plan::Trajectory {
            task,
            elapsed_ms: observation.elapsed,
            current: observation.current,
        })
    }
    fn refresh(&self) -> Projection {
        self.report
            .refresh(&self.function, &self.scheduler, self.profile)
    }
    fn sync_versions(&self) -> Projection {
        self.report
            .sync_versions(&self.function, &self.scheduler, self.profile)
    }
    fn remaining_time(&self, dimension: LightDimension) -> u16 {
        self.report.remaining_time(dimension)
    }
    fn begin(&self, task: Task, forced_power: Option<bool>) -> Result<Rc<LightAction>, Error> {
        self.refresh();
        let action = self
            .scheduler
            .begin(task, forced_power)
            .map_err(command_error)?;
        self.report.sync_remaining(&self.scheduler);
        Ok(action)
    }
    async fn execute(&self, plan: plan::Plan) -> Result<(), Error> {
        match plan {
            plan::Plan::Ignore => Ok(()),
            plan::Plan::Stop(dimension) => self.stop(dimension),
            plan::Plan::Execute { task } => {
                let action = self.begin(task, None)?;
                action.accepted().await.map_err(command_error)?;
                if matches!(task, Task::Color(_)) {
                    self.refresh();
                }
                Ok(())
            }
        }
    }
    async fn power(&self, command: Command) -> Result<(), Error> {
        match plan::power(command, &self.plan_input()).map_err(plan_error)? {
            plan::PowerPlan::Execute(command) => {
                self.function
                    .execute(command)
                    .await
                    .map_err(command_error)?;
            }
            plan::PowerPlan::Begin { task, power } => {
                self.begin(Task::Level(task), Some(power))?
                    .accepted()
                    .await
                    .map_err(command_error)?;
            }
        }
        Ok(())
    }
    fn stop(&self, dimension: LightDimension) -> Result<(), Error> {
        self.refresh();
        self.scheduler.stop(dimension).map_err(command_error)?;
        self.report.sync_remaining(&self.scheduler);
        Ok(())
    }
    fn advance(&self) {
        // Report-relevant acceptance facts are consumed before the scheduler
        // can clear an action that no longer owns its dimension.
        self.refresh();
        self.scheduler.advance();
        self.report.sync_remaining(&self.scheduler);
    }
    fn reports(&self, ctx: &impl HandlerContext) {
        let endpoint = self.endpoint;
        self.report.reports(
            &self.function,
            &self.scheduler,
            self.profile,
            |cluster, attribute| ctx.notify_attr_changed(endpoint, cluster, attribute),
        );
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
            let ticking = self.scheduler.is_ticking() || self.report.is_pending();
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
