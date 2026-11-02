use super::super::{Lighting, plan, plan_error};
use crate::{device::LightDimension, lighting::hs};
use rs_matter::{
    dm::{Cluster, InvokeContext, ReadContext, WriteContext, clusters::decl::color_control as c},
    error::{Error, ErrorCode},
    tlv::Nullable,
    with,
};

pub(in crate::matter) struct ColorHandler<'a, const RGB: bool>(pub(in crate::matter) &'a Lighting);

fn hs_direction(direction: c::DirectionEnum) -> hs::Direction {
    match direction {
        c::DirectionEnum::Up => hs::Direction::Up,
        c::DirectionEnum::Down => hs::Direction::Down,
        c::DirectionEnum::Shortest => hs::Direction::Shortest,
        c::DirectionEnum::Longest => hs::Direction::Longest,
    }
}

fn color_move_mode(mode: c::MoveModeEnum) -> plan::MoveMode {
    match mode {
        c::MoveModeEnum::Up => plan::MoveMode::Up,
        c::MoveModeEnum::Down => plan::MoveMode::Down,
        c::MoveModeEnum::Stop => plan::MoveMode::Stop,
    }
}

fn color_step_mode(mode: c::StepModeEnum) -> plan::StepMode {
    match mode {
        c::StepModeEnum::Up => plan::StepMode::Up,
        c::StepModeEnum::Down => plan::StepMode::Down,
    }
}

macro_rules! unsupported_color {
    ($($name:ident:$request:ident),* $(,)?) => {$ (
        async fn $name(&self,_ctx:impl InvokeContext,_request:c::$request<'_>)->Result<(),Error>{Err(ErrorCode::CommandNotFound.into())}
    )*};
}
impl<const RGB: bool> ColorHandler<'_, RGB> {
    async fn apply(&self, plan: Result<plan::Plan, plan::PlanError>) -> Result<(), Error> {
        self.0.execute(plan.map_err(plan_error)?).await
    }
}
impl<const RGB: bool> c::ClusterAsyncHandler for ColorHandler<'_, RGB> {
    const CLUSTER: Cluster<'static> = if RGB {
        c::FULL_CLUSTER
            .with_features(0x19)
            .with_attrs(
                |a, _, _| matches!(a.id,0..=4|7|8|15|16|0x4001|0x400a..=0x400d|0xfff8..=0xfffd),
            )
            .with_cmds(with!(
                c::CommandId::MoveToHue
                    | c::CommandId::MoveHue
                    | c::CommandId::StepHue
                    | c::CommandId::MoveToSaturation
                    | c::CommandId::MoveSaturation
                    | c::CommandId::StepSaturation
                    | c::CommandId::MoveToHueAndSaturation
                    | c::CommandId::MoveToColor
                    | c::CommandId::MoveColor
                    | c::CommandId::StepColor
                    | c::CommandId::MoveToColorTemperature
                    | c::CommandId::MoveColorTemperature
                    | c::CommandId::StepColorTemperature
                    | c::CommandId::StopMoveStep
            ))
    } else {
        c::FULL_CLUSTER
            .with_features(0x10)
            .with_attrs(|a, _, _| matches!(a.id,2|7|8|15|16|0x4001|0x400a..=0x400d|0xfff8..=0xfffd))
            .with_cmds(with!(
                c::CommandId::MoveToColorTemperature
                    | c::CommandId::MoveColorTemperature
                    | c::CommandId::StepColorTemperature
                    | c::CommandId::StopMoveStep
            ))
    };
    fn dataver(&self) -> u32 {
        self.0.sync_versions();
        self.0.report.color_version.get()
    }
    fn dataver_changed(&self) {
        self.0.report.color_version.changed();
    }
    async fn remaining_time(&self, _ctx: impl ReadContext) -> Result<u16, Error> {
        Ok(self.0.remaining_time(LightDimension::Color))
    }
    async fn current_hue(&self, _ctx: impl ReadContext) -> Result<u8, Error> {
        self.0
            .refresh()
            .hs
            .map(|value| value.hue.round().clamp(0.0, 254.0) as u8)
            .ok_or_else(|| ErrorCode::Failure.into())
    }
    async fn current_saturation(&self, _ctx: impl ReadContext) -> Result<u8, Error> {
        self.0
            .refresh()
            .hs
            .map(|value| value.saturation.round().clamp(0.0, 254.0) as u8)
            .ok_or_else(|| ErrorCode::Failure.into())
    }
    async fn current_x(&self, _ctx: impl ReadContext) -> Result<u16, Error> {
        self.0
            .refresh()
            .xy
            .map(|v| v.0)
            .ok_or_else(|| ErrorCode::Failure.into())
    }
    async fn current_y(&self, _ctx: impl ReadContext) -> Result<u16, Error> {
        self.0
            .refresh()
            .xy
            .map(|v| v.1)
            .ok_or_else(|| ErrorCode::Failure.into())
    }
    async fn color_temperature_mireds(&self, _ctx: impl ReadContext) -> Result<u16, Error> {
        self.0
            .refresh()
            .temperature
            .ok_or_else(|| ErrorCode::Failure.into())
    }
    async fn color_mode(&self, _ctx: impl ReadContext) -> Result<c::ColorModeEnum, Error> {
        match self.0.refresh().color_mode {
            Some(0) => Ok(c::ColorModeEnum::CurrentHueAndCurrentSaturation),
            Some(1) => Ok(c::ColorModeEnum::CurrentXAndCurrentY),
            Some(2) => Ok(c::ColorModeEnum::ColorTemperatureMireds),
            _ => Err(ErrorCode::Failure.into()),
        }
    }
    async fn enhanced_color_mode(
        &self,
        _ctx: impl ReadContext,
    ) -> Result<c::EnhancedColorModeEnum, Error> {
        match self.0.refresh().color_mode {
            Some(0) => Ok(c::EnhancedColorModeEnum::CurrentHueAndCurrentSaturation),
            Some(1) => Ok(c::EnhancedColorModeEnum::CurrentXAndCurrentY),
            Some(2) => Ok(c::EnhancedColorModeEnum::ColorTemperatureMireds),
            _ => Err(ErrorCode::Failure.into()),
        }
    }
    async fn options(&self, _ctx: impl ReadContext) -> Result<c::OptionsBitmap, Error> {
        Ok(c::OptionsBitmap::from_bits_retain(
            self.0.color_options.get(),
        ))
    }
    async fn number_of_primaries(&self, _ctx: impl ReadContext) -> Result<Nullable<u8>, Error> {
        Ok(Nullable::none())
    }
    async fn color_capabilities(
        &self,
        _ctx: impl ReadContext,
    ) -> Result<c::ColorCapabilitiesBitmap, Error> {
        Ok(c::ColorCapabilitiesBitmap::from_bits_retain(if RGB {
            0x19
        } else {
            0x10
        }))
    }
    async fn color_temp_physical_min_mireds(&self, _ctx: impl ReadContext) -> Result<u16, Error> {
        Ok(self.0.bounds().ok_or(ErrorCode::InvalidState)?.0)
    }
    async fn color_temp_physical_max_mireds(&self, _ctx: impl ReadContext) -> Result<u16, Error> {
        Ok(self.0.bounds().ok_or(ErrorCode::InvalidState)?.1)
    }
    async fn couple_color_temp_to_level_min_mireds(
        &self,
        _ctx: impl ReadContext,
    ) -> Result<u16, Error> {
        Ok(self.0.bounds().ok_or(ErrorCode::InvalidState)?.0)
    }
    async fn set_options(
        &self,
        ctx: impl WriteContext,
        value: c::OptionsBitmap,
    ) -> Result<(), Error> {
        if value.bits() & !1 != 0 {
            return Err(ErrorCode::ConstraintError.into());
        }
        let changed = self.0.color_options.get() != value.bits();
        self.0.color_options.set(value.bits());
        if changed {
            ctx.notify_changed();
        }
        Ok(())
    }
    async fn handle_move_to_hue(
        &self,
        _ctx: impl InvokeContext,
        r: c::MoveToHueRequest<'_>,
    ) -> Result<(), Error> {
        let (hue, direction, duration) = (r.hue()?, r.direction()?, r.transition_time()?);
        let (mask, override_) = (r.options_mask()?.bits(), r.options_override()?.bits());
        let input = self.0.plan_input();
        self.apply(plan::move_to_hue(
            hue,
            hs_direction(direction),
            duration,
            plan::Options::new(mask, override_),
            &input,
        ))
        .await
    }
    async fn handle_move_hue(
        &self,
        _ctx: impl InvokeContext,
        r: c::MoveHueRequest<'_>,
    ) -> Result<(), Error> {
        let (mode, rate) = (r.move_mode()?, r.rate()?);
        let (mask, override_) = (r.options_mask()?.bits(), r.options_override()?.bits());
        let input = self.0.plan_input();
        self.apply(plan::move_hue(
            color_move_mode(mode),
            rate,
            plan::Options::new(mask, override_),
            &input,
        ))
        .await
    }
    async fn handle_step_hue(
        &self,
        _ctx: impl InvokeContext,
        r: c::StepHueRequest<'_>,
    ) -> Result<(), Error> {
        let (mode, size, duration) = (r.step_mode()?, r.step_size()?, r.transition_time()?);
        let (mask, override_) = (r.options_mask()?.bits(), r.options_override()?.bits());
        let input = self.0.plan_input();
        self.apply(plan::step_hue(
            color_step_mode(mode),
            size,
            duration,
            plan::Options::new(mask, override_),
            &input,
        ))
        .await
    }
    async fn handle_move_to_saturation(
        &self,
        _ctx: impl InvokeContext,
        r: c::MoveToSaturationRequest<'_>,
    ) -> Result<(), Error> {
        let (saturation, duration) = (r.saturation()?, r.transition_time()?);
        let (mask, override_) = (r.options_mask()?.bits(), r.options_override()?.bits());
        let input = self.0.plan_input();
        self.apply(plan::move_to_saturation(
            saturation,
            duration,
            plan::Options::new(mask, override_),
            &input,
        ))
        .await
    }
    async fn handle_move_saturation(
        &self,
        _ctx: impl InvokeContext,
        r: c::MoveSaturationRequest<'_>,
    ) -> Result<(), Error> {
        let (mode, rate) = (r.move_mode()?, r.rate()?);
        let (mask, override_) = (r.options_mask()?.bits(), r.options_override()?.bits());
        let input = self.0.plan_input();
        self.apply(plan::move_saturation(
            color_move_mode(mode),
            rate,
            plan::Options::new(mask, override_),
            &input,
        ))
        .await
    }
    async fn handle_step_saturation(
        &self,
        _ctx: impl InvokeContext,
        r: c::StepSaturationRequest<'_>,
    ) -> Result<(), Error> {
        let (mode, size, duration) = (r.step_mode()?, r.step_size()?, r.transition_time()?);
        let (mask, override_) = (r.options_mask()?.bits(), r.options_override()?.bits());
        let input = self.0.plan_input();
        self.apply(plan::step_saturation(
            color_step_mode(mode),
            size,
            duration,
            plan::Options::new(mask, override_),
            &input,
        ))
        .await
    }
    async fn handle_move_to_hue_and_saturation(
        &self,
        _ctx: impl InvokeContext,
        r: c::MoveToHueAndSaturationRequest<'_>,
    ) -> Result<(), Error> {
        let (hue, saturation, duration) = (r.hue()?, r.saturation()?, r.transition_time()?);
        let (mask, override_) = (r.options_mask()?.bits(), r.options_override()?.bits());
        let input = self.0.plan_input();
        self.apply(plan::move_to_hue_and_saturation(
            hue,
            saturation,
            duration,
            plan::Options::new(mask, override_),
            &input,
        ))
        .await
    }
    async fn handle_move_to_color_temperature(
        &self,
        _ctx: impl InvokeContext,
        r: c::MoveToColorTemperatureRequest<'_>,
    ) -> Result<(), Error> {
        let (target, duration) = (r.color_temperature_mireds()?, r.transition_time()?);
        let (mask, override_) = (r.options_mask()?.bits(), r.options_override()?.bits());
        let input = self.0.plan_input();
        self.apply(plan::move_to_color_temperature(
            target,
            duration,
            plan::Options::new(mask, override_),
            &input,
        ))
        .await
    }
    async fn handle_move_to_color(
        &self,
        _ctx: impl InvokeContext,
        r: c::MoveToColorRequest<'_>,
    ) -> Result<(), Error> {
        let (x, y, duration) = (r.color_x()?, r.color_y()?, r.transition_time()?);
        let (mask, override_) = (r.options_mask()?.bits(), r.options_override()?.bits());
        let input = self.0.plan_input();
        self.apply(plan::move_to_color(
            x,
            y,
            duration,
            plan::Options::new(mask, override_),
            &input,
        ))
        .await
    }
    async fn handle_move_color(
        &self,
        _ctx: impl InvokeContext,
        r: c::MoveColorRequest<'_>,
    ) -> Result<(), Error> {
        let (rate_x, rate_y) = (r.rate_x()?, r.rate_y()?);
        let (mask, override_) = (r.options_mask()?.bits(), r.options_override()?.bits());
        let input = self.0.plan_input();
        self.apply(plan::move_color(
            rate_x,
            rate_y,
            plan::Options::new(mask, override_),
            &input,
        ))
        .await
    }
    async fn handle_step_color(
        &self,
        _ctx: impl InvokeContext,
        r: c::StepColorRequest<'_>,
    ) -> Result<(), Error> {
        let (x, y, duration) = (r.step_x()?, r.step_y()?, r.transition_time()?);
        let (mask, override_) = (r.options_mask()?.bits(), r.options_override()?.bits());
        let input = self.0.plan_input();
        self.apply(plan::step_color(
            x,
            y,
            duration,
            plan::Options::new(mask, override_),
            &input,
        ))
        .await
    }
    async fn handle_stop_move_step(
        &self,
        _ctx: impl InvokeContext,
        r: c::StopMoveStepRequest<'_>,
    ) -> Result<(), Error> {
        let (mask, override_) = (r.options_mask()?.bits(), r.options_override()?.bits());
        let input = self.0.plan_input();
        self.apply(plan::stop_move_step(
            plan::Options::new(mask, override_),
            &input,
        ))
        .await
    }
    async fn handle_move_color_temperature(
        &self,
        _ctx: impl InvokeContext,
        r: c::MoveColorTemperatureRequest<'_>,
    ) -> Result<(), Error> {
        let (mode, rate, min, max, mask, override_) = (
            r.move_mode()?,
            r.rate()?,
            r.color_temperature_minimum_mireds()?,
            r.color_temperature_maximum_mireds()?,
            r.options_mask()?.bits(),
            r.options_override()?.bits(),
        );
        let input = self.0.plan_input();
        self.apply(plan::move_color_temperature(
            color_move_mode(mode),
            rate,
            min,
            max,
            plan::Options::new(mask, override_),
            &input,
        ))
        .await
    }
    async fn handle_step_color_temperature(
        &self,
        _ctx: impl InvokeContext,
        r: c::StepColorTemperatureRequest<'_>,
    ) -> Result<(), Error> {
        let (mode, size, duration, min, max, mask, override_) = (
            r.step_mode()?,
            r.step_size()?,
            r.transition_time()?,
            r.color_temperature_minimum_mireds()?,
            r.color_temperature_maximum_mireds()?,
            r.options_mask()?.bits(),
            r.options_override()?.bits(),
        );
        let input = self.0.plan_input();
        self.apply(plan::step_color_temperature(
            color_step_mode(mode),
            size,
            duration,
            min,
            max,
            plan::Options::new(mask, override_),
            &input,
        ))
        .await
    }
    unsupported_color!(handle_enhanced_move_to_hue:EnhancedMoveToHueRequest,
        handle_enhanced_move_hue:EnhancedMoveHueRequest,handle_enhanced_step_hue:EnhancedStepHueRequest,
        handle_enhanced_move_to_hue_and_saturation:EnhancedMoveToHueAndSaturationRequest,handle_color_loop_set:ColorLoopSetRequest);
}
