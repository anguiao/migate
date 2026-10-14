//! Group fanout over the shared lighting executor.
//!
//! A group command fixes the applied member set, every member's actual start,
//! the unified target, bounds and one origin timestamp before any await. Each
//! member is then committed independently; the group entry only waits for the
//! first complete acceptance, or for every member to fail. Member actions
//! execute through the single-light control chain after the response; the
//! group retains no action history or completion tracking.

use super::{
    LightingExecutor, Plan,
    controllers::level,
    planning::{ColorPlan, ColorState, LevelPlan},
};
use crate::device::{
    Capability, Category, Command, CommandError, Function, Group, LightDimension, NumericRange,
    brightness_to_matter_level,
};
use futures_util::{StreamExt, stream::FuturesUnordered};
use std::{future::Future, pin::Pin, rc::Rc};

/// A command normalized at the protocol entry before any member plan exists.
/// Endpoint Options, OnLevel and ExecuteIfOff have already been interpreted
/// once; members never re-read their hidden endpoint settings.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Fanout {
    /// A definite on/off target; Toggle was already resolved at the entry.
    Power(bool),
    Level(LevelPlan),
    Color(ColorPlan),
}

type MemberFuture = Pin<Box<dyn Future<Output = Result<(), CommandError>>>>;

/// Temporary acceptance waiters for a submitted command. The member control
/// chains already own execution, independently of this response.
pub(crate) struct GroupDispatch {
    pending: FuturesUnordered<MemberFuture>,
}

impl GroupDispatch {
    /// Return on the first complete acceptance, or after every member fails.
    pub(crate) async fn accepted(mut self) -> Result<(), CommandError> {
        let mut first_error = None;
        while let Some(result) = self.pending.next().await {
            match result {
                Ok(()) => return Ok(()),
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        Err(first_error.expect("a group has at least one member"))
    }
}

/// The actual member color state used to fix per-member starts. Values are
/// filtered by freshness, so a recovered path never revives an old cache, and
/// a temperature observation outside the member range is never clamped.
fn member_color(function: &Function, executor: &LightingExecutor, now: u64) -> ColorState {
    let mut state = executor.observed_color(
        Some(function.function_key()),
        function.fresh_snapshot().snapshot.lighting,
        function.description().temperature_range(),
    );
    state.continuing_hs = executor.resumed_hs_at(function.function_key(), now);
    state
}

fn rgb_capable(function: &Function) -> Result<(), CommandError> {
    function
        .description()
        .capabilities
        .contains(&Capability::ColorRgb)
        .then_some(())
        .ok_or(CommandError::Unsupported)
}

fn temperature_range(function: &Function) -> Result<NumericRange, CommandError> {
    function
        .description()
        .temperature_range()
        .ok_or(CommandError::Unsupported)
}

fn light_function(function: &Function) -> Result<(), CommandError> {
    (function.description().category == Category::Light)
        .then_some(())
        .ok_or(CommandError::Unsupported)
}

fn current_level(function: &Function) -> Option<u8> {
    function
        .fresh_snapshot()
        .snapshot
        .lighting
        .and_then(|lighting| lighting.brightness_percent)
        .map(|percent| {
            brightness_to_matter_level(percent.max(1.0)).expect("validated member brightness")
        })
}

/// Resolve one level intent against a member's actual level. A member without
/// a usable start fails alone; every other member still executes.
fn plan_level(function: &Function, plan: LevelPlan) -> Result<level::Task, CommandError> {
    light_function(function)?;
    if !function
        .description()
        .capabilities
        .contains(&Capability::Brightness)
    {
        return Err(CommandError::Unsupported);
    }
    plan.resolve(current_level(function))
}

impl LightingExecutor {
    /// Commit one normalized group command to the current applied member set.
    /// Each distinct member is planned and submitted synchronously with the
    /// same trigger origin. Every member is submitted before awaiting any
    /// acceptance; planning and submission only affect that member's chain.
    pub(crate) fn fanout(
        self: &Rc<Self>,
        group: &Group,
        command: Fanout,
        forced_power: Option<bool>,
    ) -> GroupDispatch {
        let now = self.clock.now();
        let pending = group
            .functions()
            .into_iter()
            .map(
                |function| match self.submit_member(&function, command, forced_power, now) {
                    Ok(waiter) => waiter,
                    Err(error) => Box::pin(async move { Err(error) }) as MemberFuture,
                },
            )
            .collect();
        GroupDispatch { pending }
    }

    /// A Stop completes synchronously; other commands return only their
    /// acceptance waiter, without retaining their later completion.
    fn submit_member(
        self: &Rc<Self>,
        function: &Function,
        command: Fanout,
        forced_power: Option<bool>,
        now: u64,
    ) -> Result<MemberFuture, CommandError> {
        let plan = match command {
            Fanout::Power(on) => {
                let waiter =
                    self.dispatch_power(function, if on { Command::On } else { Command::Off });
                return Ok(Box::pin(async move { waiter.accepted().await.map(|_| ()) }));
            }
            Fanout::Level(LevelPlan::Stop) => {
                self.stop(function, LightDimension::Brightness)?;
                return Ok(Box::pin(async { Ok(()) }));
            }
            Fanout::Color(ColorPlan::Stop) => {
                self.stop(function, LightDimension::Color)?;
                return Ok(Box::pin(async { Ok(()) }));
            }
            Fanout::Color(ColorPlan::StopHs) => {
                self.stop_hs(function)?;
                return Ok(Box::pin(async { Ok(()) }));
            }
            Fanout::Level(plan) => Plan::Level(plan_level(function, plan)?),
            Fanout::Color(plan) => self.plan_color(function, plan, now)?,
        };
        let action = self.begin_at(function, plan, forced_power, now)?;
        Ok(Box::pin(async move { action.accepted().await.map(|_| ()) }))
    }

    /// Resolve one color intent against a member's actual state. Relative
    /// commands continue each member's own trajectory; the effective
    /// temperature bounds were fixed once at the group entry.
    fn plan_color(
        &self,
        function: &Function,
        plan: ColorPlan,
        now: u64,
    ) -> Result<Plan, CommandError> {
        light_function(function)?;
        let temperature = if plan.is_temperature() {
            Some(temperature_range(function)?)
        } else {
            rgb_capable(function)?;
            None
        };
        Ok(Plan::Color {
            task: plan.resolve(member_color(function, self, now))?,
            temperature,
        })
    }
}
