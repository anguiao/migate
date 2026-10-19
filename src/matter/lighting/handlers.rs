use super::{Lighting, plan, plan_error};
use crate::device::{Command, LightDimension};
use crate::lighting::{hs, level};
use rs_matter::{
    dm::{
        Cluster, HandlerContext, InvokeContext, ReadContext, WriteContext,
        clusters::decl::{color_control as c, level_control as l, on_off as o},
    },
    error::{Error, ErrorCode},
    tlv::Nullable,
    with,
};

pub(in crate::matter) struct OnOffHandler<'a>(pub(in crate::matter) &'a Lighting);
pub(in crate::matter) struct LevelHandler<'a>(pub(in crate::matter) &'a Lighting);
pub(in crate::matter) struct ColorHandler<'a, const RGB: bool>(pub(in crate::matter) &'a Lighting);

impl o::ClusterAsyncHandler for OnOffHandler<'_> {
    const CLUSTER: Cluster<'static> = o::FULL_CLUSTER
        .with_features(0)
        .with_attrs(|a, _, _| a.id == 0 || a.id >= 0xfff8)
        .with_cmds(with!(
            o::CommandId::Off | o::CommandId::On | o::CommandId::Toggle
        ));
    fn dataver(&self) -> u32 {
        self.0.sync_versions();
        self.0.report.on_off_version.get()
    }
    fn dataver_changed(&self) {
        self.0.report.on_off_version.changed();
    }
    async fn run(&self, ctx: impl HandlerContext) -> Result<(), Error> {
        self.0.run(ctx).await
    }
    async fn on_off(&self, _ctx: impl ReadContext) -> Result<bool, Error> {
        self.0
            .refresh()
            .power
            .ok_or_else(|| ErrorCode::Failure.into())
    }
    async fn handle_on(&self, ctx: impl InvokeContext) -> Result<(), Error> {
        validate_power(&ctx)?;
        self.0.power(Command::On).await
    }
    async fn handle_off(&self, ctx: impl InvokeContext) -> Result<(), Error> {
        validate_power(&ctx)?;
        self.0.power(Command::Off).await
    }
    async fn handle_toggle(&self, ctx: impl InvokeContext) -> Result<(), Error> {
        validate_power(&ctx)?;
        self.0.power(Command::Toggle).await
    }
    async fn handle_off_with_effect(
        &self,
        _ctx: impl InvokeContext,
        _request: o::OffWithEffectRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::CommandNotFound.into())
    }
    async fn handle_on_with_recall_global_scene(
        &self,
        _ctx: impl InvokeContext,
    ) -> Result<(), Error> {
        Err(ErrorCode::CommandNotFound.into())
    }
    async fn handle_on_with_timed_off(
        &self,
        _ctx: impl InvokeContext,
        _request: o::OnWithTimedOffRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::CommandNotFound.into())
    }
}
fn validate_power(ctx: &impl InvokeContext) -> Result<(), Error> {
    ctx.data()
        .structure()
        .and_then(|_| ctx.data().raw_value())
        .map_err(|_| ErrorCode::InvalidCommand.into())
        .map(|_| ())
}

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
