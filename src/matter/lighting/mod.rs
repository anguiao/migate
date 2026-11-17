mod color;
mod dispatch;
pub(super) use dispatch::{Dispatch, IDENTIFY};
mod handlers;
mod plan;
mod report;
#[cfg(test)]
mod tests;

use super::units;
use crate::device::lighting::{ActionPlan, Scheduler, Task, clock::Clock};
use crate::device::{Command, CommandError, Function, LightAction, LightDimension, LightProfile};
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
fn plan_error(function: &Function, command: impl std::fmt::Debug, error: plan::PlanError) -> Error {
    log::warn!(
        "Matter {}/{} {command:?} planning failed: {error:?}",
        function.function_key().device.id,
        function.function_key().id,
    );
    match error {
        plan::PlanError::InvalidCommand => ErrorCode::InvalidCommand,
        plan::PlanError::Constraint => ErrorCode::ConstraintError,
        plan::PlanError::InvalidState => ErrorCode::InvalidInState,
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
            Rc::new(crate::device::lighting::clock::SystemClock::new()),
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
        let scheduler = Scheduler::new(function.clone(), clock);
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
            hs_trajectory: self.scheduler.observe(LightDimension::Color).is_some_and(
                |observation| {
                    observation.current
                        && matches!(
                            observation.task,
                            Task::Color(crate::device::lighting::color::Task::Hs(_))
                        )
                },
            ),
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
    fn refresh(&self) -> Projection {
        self.report
            .refresh(&self.function, &self.scheduler, self.profile)
    }
    fn sync_versions(&self) -> Projection {
        self.report
            .sync_versions(&self.function, &self.scheduler, self.profile)
    }
    fn remaining_time(&self, dimension: LightDimension) -> u16 {
        self.report.sync_remaining(&self.scheduler);
        self.report.remaining_time(dimension)
    }
    fn begin(&self, plan: ActionPlan) -> Result<Rc<LightAction>, CommandError> {
        self.refresh();
        let action = self.scheduler.begin(plan)?;
        self.report.sync_remaining(&self.scheduler);
        Ok(action)
    }
    async fn apply(
        &self,
        command: &str,
        plan: Result<plan::Plan, plan::PlanError>,
    ) -> Result<(), Error> {
        self.execute(plan.map_err(|error| plan_error(&self.function, command, error))?)
            .await
            .map_err(|error| super::command_error(&self.function, command, error))
    }
    async fn execute(&self, plan: plan::Plan) -> Result<(), CommandError> {
        match plan {
            plan::Plan::Ignore => Ok(()),
            plan::Plan::Stop(dimension) => self.stop(dimension),
            plan::Plan::Execute(plan) => {
                let action = self.begin(plan)?;
                action.accepted().await?;
                if matches!(plan.task, Task::Color(_)) {
                    self.refresh();
                }
                Ok(())
            }
        }
    }
    async fn power(&self, command: Command) -> Result<(), Error> {
        let on_error = |error| super::command_error(&self.function, command, error);
        match plan::power(command, &self.plan_input())
            .map_err(|error| plan_error(&self.function, command, error))?
        {
            plan::PowerPlan::Execute(command) => {
                self.function.execute(command).await.map_err(on_error)?;
            }
            plan::PowerPlan::Begin(plan) => {
                self.begin(plan)
                    .map_err(on_error)?
                    .accepted()
                    .await
                    .map_err(on_error)?;
            }
        }
        Ok(())
    }
    fn stop(&self, dimension: LightDimension) -> Result<(), CommandError> {
        self.refresh();
        self.scheduler.stop(dimension)?;
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
