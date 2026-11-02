use super::super::{Lighting, plan, plan_error};
use crate::{device::LightDimension, lighting::level};
use rs_matter::{
    dm::{Cluster, InvokeContext, ReadContext, WriteContext, clusters::decl::level_control as l},
    error::{Error, ErrorCode},
    tlv::Nullable,
    with,
};

pub(in crate::matter) struct LevelHandler<'a>(pub(in crate::matter) &'a Lighting);

impl LevelHandler<'_> {
    async fn apply(&self, plan: Result<plan::Plan, plan::PlanError>) -> Result<(), Error> {
        self.0.execute(plan.map_err(plan_error)?).await
    }
}

macro_rules! level_to {
    ($name:ident,$request:ident,$with:expr) => {
        async fn $name(
            &self,
            _ctx: impl InvokeContext,
            request: l::$request<'_>,
        ) -> Result<(), Error> {
            let level = request.level()?;
            let transition = request.transition_time()?.into_option();
            let (mask, override_) = (
                request.options_mask()?.bits(),
                request.options_override()?.bits(),
            );
            let input = self.0.plan_input();
            self.apply(plan::move_to_level(
                level,
                $with,
                transition,
                plan::Options::new(mask, override_),
                &input,
            ))
            .await
        }
    };
}
macro_rules! level_move {
    ($name:ident,$request:ident,$with:expr) => {
        async fn $name(
            &self,
            _ctx: impl InvokeContext,
            request: l::$request<'_>,
        ) -> Result<(), Error> {
            let mode = match request.move_mode()? {
                l::MoveModeEnum::Up => level::Direction::Up,
                l::MoveModeEnum::Down => level::Direction::Down,
            };
            let rate = request.rate()?.into_option();
            let (mask, override_) = (
                request.options_mask()?.bits(),
                request.options_override()?.bits(),
            );
            let input = self.0.plan_input();
            self.apply(plan::move_level(
                $with,
                mode,
                rate,
                plan::Options::new(mask, override_),
                &input,
            ))
            .await
        }
    };
}
macro_rules! level_step {
    ($name:ident,$request:ident,$with:expr) => {
        async fn $name(
            &self,
            _ctx: impl InvokeContext,
            request: l::$request<'_>,
        ) -> Result<(), Error> {
            let mode = match request.step_mode()? {
                l::StepModeEnum::Up => plan::StepMode::Up,
                l::StepModeEnum::Down => plan::StepMode::Down,
            };
            let size = request.step_size()?;
            let transition = request.transition_time()?.into_option();
            let (mask, override_) = (
                request.options_mask()?.bits(),
                request.options_override()?.bits(),
            );
            let input = self.0.plan_input();
            self.apply(plan::step(
                $with,
                mode,
                size,
                transition,
                plan::Options::new(mask, override_),
                &input,
            ))
            .await
        }
    };
}
macro_rules! level_stop {
    ($name:ident,$request:ident,$with:expr) => {
        async fn $name(
            &self,
            _ctx: impl InvokeContext,
            request: l::$request<'_>,
        ) -> Result<(), Error> {
            let (mask, override_) = (
                request.options_mask()?.bits(),
                request.options_override()?.bits(),
            );
            let input = self.0.plan_input();
            self.apply(plan::stop_level(
                $with,
                plan::Options::new(mask, override_),
                &input,
            ))
            .await
        }
    };
}
impl l::ClusterAsyncHandler for LevelHandler<'_> {
    const CLUSTER: Cluster<'static> = l::FULL_CLUSTER
        .with_features(l::Feature::ON_OFF.bits())
        .with_attrs(|a, _, _| matches!(a.id,0..=3|15|17|0xfff8..=0xfffd))
        .with_cmds(with!(
            l::CommandId::MoveToLevel
                | l::CommandId::Move
                | l::CommandId::Step
                | l::CommandId::Stop
                | l::CommandId::MoveToLevelWithOnOff
                | l::CommandId::MoveWithOnOff
                | l::CommandId::StepWithOnOff
                | l::CommandId::StopWithOnOff
        ));
    fn dataver(&self) -> u32 {
        self.0.sync_versions();
        self.0.report.level_version.get()
    }
    fn dataver_changed(&self) {
        self.0.report.level_version.changed();
    }
    async fn current_level(&self, _ctx: impl ReadContext) -> Result<Nullable<u8>, Error> {
        Ok(Nullable::new(self.0.refresh().level))
    }
    async fn min_level(&self, _ctx: impl ReadContext) -> Result<u8, Error> {
        Ok(1)
    }
    async fn max_level(&self, _ctx: impl ReadContext) -> Result<u8, Error> {
        Ok(254)
    }
    async fn remaining_time(&self, _ctx: impl ReadContext) -> Result<u16, Error> {
        Ok(self.0.remaining_time(LightDimension::Brightness))
    }
    async fn options(&self, _ctx: impl ReadContext) -> Result<l::OptionsBitmap, Error> {
        Ok(l::OptionsBitmap::from_bits_retain(
            self.0.level_options.get(),
        ))
    }
    async fn on_level(&self, _ctx: impl ReadContext) -> Result<Nullable<u8>, Error> {
        Ok(Nullable::new(self.0.on_level.get()))
    }
    async fn set_options(
        &self,
        ctx: impl WriteContext,
        value: l::OptionsBitmap,
    ) -> Result<(), Error> {
        if value.bits() & !1 != 0 {
            return Err(ErrorCode::ConstraintError.into());
        }
        let changed = self.0.level_options.get() != value.bits();
        self.0.level_options.set(value.bits());
        if changed {
            ctx.notify_changed();
        }
        Ok(())
    }
    async fn set_on_level(&self, ctx: impl WriteContext, value: Nullable<u8>) -> Result<(), Error> {
        let value = value.into_option();
        if value.is_some_and(|v| !(1..=254).contains(&v)) {
            return Err(ErrorCode::ConstraintError.into());
        }
        let changed = self.0.on_level.get() != value;
        self.0.on_level.set(value);
        if changed {
            ctx.notify_changed();
        }
        Ok(())
    }
    level_to!(handle_move_to_level, MoveToLevelRequest, false);
    level_to!(
        handle_move_to_level_with_on_off,
        MoveToLevelWithOnOffRequest,
        true
    );
    level_move!(handle_move, MoveRequest, false);
    level_move!(handle_move_with_on_off, MoveWithOnOffRequest, true);
    level_step!(handle_step, StepRequest, false);
    level_step!(handle_step_with_on_off, StepWithOnOffRequest, true);
    level_stop!(handle_stop, StopRequest, false);
    level_stop!(handle_stop_with_on_off, StopWithOnOffRequest, true);
    async fn handle_move_to_closest_frequency(
        &self,
        _ctx: impl InvokeContext,
        _request: l::MoveToClosestFrequencyRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::CommandNotFound.into())
    }
}
