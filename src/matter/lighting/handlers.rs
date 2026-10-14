use super::{
    Lighting,
    controllers::{self, color, hs},
};
use crate::device::{Command, LightDimension};
use crate::lighting::{controllers::MoveDirection, planning::ColorPlan};
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
pub(in crate::matter) struct ColorHandler<'a, const CT: bool, const RGB: bool>(
    pub(in crate::matter) &'a Lighting,
);

impl o::ClusterAsyncHandler for OnOffHandler<'_> {
    const CLUSTER: Cluster<'static> = o::FULL_CLUSTER
        .with_features(0)
        .with_attrs(|a, _, _| a.id == 0 || a.id >= 0xfff8)
        .with_cmds(with!(
            o::CommandId::Off | o::CommandId::On | o::CommandId::Toggle
        ));
    fn dataver(&self) -> u32 {
        self.0.sync_versions();
        self.0.on_off_version.get()
    }
    fn dataver_changed(&self) {
        self.0.on_off_version.changed();
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

macro_rules! level_to {
    ($name:ident,$request:ident,$with:expr) => {
        async fn $name(
            &self,
            _ctx: impl InvokeContext,
            request: l::$request<'_>,
        ) -> Result<(), Error> {
            let level = request.level()?;
            let transition = request.transition_time()?.into_option();
            let mask = request.options_mask()?.bits();
            let override_ = request.options_override()?.bits();
            let power = self.0.refresh().power;
            let task = self
                .0
                .level_state()
                .move_to_level(level, $with, transition, mask, override_, power)?;
            self.0.execute_level(task).await
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
            let mode = request.move_mode()?;
            let rate = request.rate()?.into_option();
            let mask = request.options_mask()?.bits();
            let override_ = request.options_override()?.bits();
            let power = self.0.refresh().power;
            let task = self
                .0
                .level_state()
                .move_level($with, mode, rate, mask, override_, power)?;
            self.0.execute_level(task).await
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
            let mode = request.step_mode()?;
            let size = request.step_size()?;
            let transition = request.transition_time()?.into_option();
            let mask = request.options_mask()?.bits();
            let override_ = request.options_override()?.bits();
            let power = self.0.refresh().power;
            let task = self
                .0
                .level_state()
                .step($with, mode, size, transition, mask, override_, power)?;
            self.0.execute_level(task).await
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
            let power = self.0.refresh().power;
            let (mask, override_) = (
                request.options_mask()?.bits(),
                request.options_override()?.bits(),
            );
            if $with {
                controllers::validate_options(mask, override_)?;
            }
            if !$with
                && !controllers::should_continue(
                    power,
                    self.0.level_state().options,
                    mask,
                    override_,
                )?
            {
                return Ok(());
            }
            self.0.stop(LightDimension::Brightness).await
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
        self.0.level_version.get()
    }
    fn dataver_changed(&self) {
        self.0.level_version.changed();
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
        Ok(self.0.level_state().remaining_time.value)
    }
    async fn options(&self, _ctx: impl ReadContext) -> Result<l::OptionsBitmap, Error> {
        Ok(l::OptionsBitmap::from_bits_retain(
            self.0.level_state().options,
        ))
    }
    async fn on_level(&self, _ctx: impl ReadContext) -> Result<Nullable<u8>, Error> {
        Ok(Nullable::new(self.0.level_state().on_level))
    }
    async fn set_options(
        &self,
        ctx: impl WriteContext,
        value: l::OptionsBitmap,
    ) -> Result<(), Error> {
        if value.bits() & !1 != 0 {
            return Err(ErrorCode::ConstraintError.into());
        }
        let changed = self.0.level_state().options != value.bits();
        self.0.level_state_mut().options = value.bits();
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
        let changed = self.0.level_state().on_level != value;
        self.0.level_state_mut().on_level = value;
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

macro_rules! unsupported_color {
    ($($name:ident:$request:ident),* $(,)?) => {$ (
        async fn $name(&self,_ctx:impl InvokeContext,_request:c::$request<'_>)->Result<(),Error>{Err(ErrorCode::CommandNotFound.into())}
    )*};
}
impl<const CT: bool, const RGB: bool> ColorHandler<'_, CT, RGB> {
    fn proceed(&self, mask: c::OptionsBitmap, override_: c::OptionsBitmap) -> Result<bool, Error> {
        let power = self.0.refresh().power;
        controllers::should_continue(
            power,
            self.0.color_state().options,
            mask.bits(),
            override_.bits(),
        )
    }
    fn bounds(&self, min: u16, max: u16) -> Result<(u16, u16), Error> {
        if min > color::XY_MAX || max > color::XY_MAX {
            return Err(ErrorCode::ConstraintError.into());
        }
        let bounds = self
            .0
            .publication()
            .temperature
            .ok_or(ErrorCode::CommandNotFound)?
            .bounds;
        let lo = if min == 0 {
            bounds.0
        } else {
            min.max(bounds.0)
        };
        let hi = if max == 0 {
            bounds.1
        } else {
            max.min(bounds.1)
        };
        if lo > hi {
            return Err(ErrorCode::InvalidCommand.into());
        }
        Ok((lo, hi))
    }
}
impl<const CT: bool, const RGB: bool> c::ClusterAsyncHandler for ColorHandler<'_, CT, RGB> {
    const CLUSTER: Cluster<'static> = c::FULL_CLUSTER
        .with_features((if CT { 0x10 } else { 0 }) | (if RGB { 0x09 } else { 0 }))
        .with_attrs(|a, _, _| match a.id {
            2 | 8 | 15 | 16 => true,
            0 | 1 | 3 | 4 => RGB,
            7 => CT,
            0x4001 | 0x400a => true,
            0x400b..=0x400d => CT,
            _ => (0xfff8..=0xfffd).contains(&a.id),
        })
        .with_cmds(|cmd, _, _| match cmd.id {
            0..=9 => RGB,
            10 | 0x4b | 0x4c => CT,
            0x47 => CT || RGB,
            _ => false,
        });
    fn dataver(&self) -> u32 {
        self.0.sync_versions();
        self.0.color_version.get()
    }
    fn dataver_changed(&self) {
        self.0.color_version.changed();
    }
    async fn remaining_time(&self, _ctx: impl ReadContext) -> Result<u16, Error> {
        Ok(self.0.color_state().remaining_time.value)
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
            self.0
                .color
                .as_ref()
                .expect("color handler owns its color state")
                .borrow()
                .options,
        ))
    }
    async fn number_of_primaries(&self, _ctx: impl ReadContext) -> Result<Nullable<u8>, Error> {
        Ok(Nullable::none())
    }
    async fn color_capabilities(
        &self,
        _ctx: impl ReadContext,
    ) -> Result<c::ColorCapabilitiesBitmap, Error> {
        Ok(c::ColorCapabilitiesBitmap::from_bits_retain(
            (if CT { 0x10 } else { 0 }) | (if RGB { 0x09 } else { 0 }),
        ))
    }
    async fn color_temp_physical_min_mireds(&self, _ctx: impl ReadContext) -> Result<u16, Error> {
        Ok(self
            .0
            .publication()
            .temperature
            .ok_or(ErrorCode::Failure)?
            .bounds
            .0)
    }
    async fn color_temp_physical_max_mireds(&self, _ctx: impl ReadContext) -> Result<u16, Error> {
        Ok(self
            .0
            .publication()
            .temperature
            .ok_or(ErrorCode::Failure)?
            .bounds
            .1)
    }
    async fn couple_color_temp_to_level_min_mireds(
        &self,
        _ctx: impl ReadContext,
    ) -> Result<u16, Error> {
        Ok(self
            .0
            .publication()
            .temperature
            .ok_or(ErrorCode::Failure)?
            .bounds
            .0)
    }
    async fn set_options(
        &self,
        ctx: impl WriteContext,
        value: c::OptionsBitmap,
    ) -> Result<(), Error> {
        if value.bits() & !1 != 0 {
            return Err(ErrorCode::ConstraintError.into());
        }
        let changed = self.0.color_state().options != value.bits();
        self.0.color_state_mut().options = value.bits();
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
        if !RGB {
            return Err(ErrorCode::CommandNotFound.into());
        }
        let (hue, direction, duration) = (r.hue()?, r.direction()?, r.transition_time()?);
        if hue == u8::MAX || duration == u16::MAX {
            return Err(ErrorCode::ConstraintError.into());
        }
        if !self.proceed(r.options_mask()?, r.options_override()?)? {
            return Ok(());
        }
        let direction = match direction {
            c::DirectionEnum::Up => hs::HueDirection::Up,
            c::DirectionEnum::Down => hs::HueDirection::Down,
            c::DirectionEnum::Shortest => hs::HueDirection::Shortest,
            c::DirectionEnum::Longest => hs::HueDirection::Longest,
        };
        self.0
            .execute_color_plan(ColorPlan::HueMoveTo {
                target: hue as f64,
                direction,
                transition_ms: duration as u64 * 100,
            })
            .await
    }
    async fn handle_move_hue(
        &self,
        _ctx: impl InvokeContext,
        r: c::MoveHueRequest<'_>,
    ) -> Result<(), Error> {
        if !RGB {
            return Err(ErrorCode::CommandNotFound.into());
        }
        let (mode, rate) = (r.move_mode()?, r.rate()?);
        if mode != c::MoveModeEnum::Stop && rate == 0 {
            return Err(ErrorCode::InvalidCommand.into());
        }
        if !self.proceed(r.options_mask()?, r.options_override()?)? {
            return Ok(());
        }
        if mode == c::MoveModeEnum::Stop {
            return self.0.stop_hs().await;
        }
        let rate = if mode == c::MoveModeEnum::Up {
            rate as f64
        } else {
            -(rate as f64)
        };
        self.0.execute_color_plan(ColorPlan::HueMove { rate }).await
    }
    async fn handle_step_hue(
        &self,
        _ctx: impl InvokeContext,
        r: c::StepHueRequest<'_>,
    ) -> Result<(), Error> {
        if !RGB {
            return Err(ErrorCode::CommandNotFound.into());
        }
        let (mode, size, duration) = (r.step_mode()?, r.step_size()?, r.transition_time()?);
        if size == 0 {
            return Err(ErrorCode::InvalidCommand.into());
        }
        if !self.proceed(r.options_mask()?, r.options_override()?)? {
            return Ok(());
        }
        let delta = if mode == c::StepModeEnum::Up {
            size as f64
        } else {
            -(size as f64)
        };
        self.0
            .execute_color_plan(ColorPlan::HueStep {
                delta,
                transition_ms: duration as u64 * 100,
            })
            .await
    }
    async fn handle_move_to_saturation(
        &self,
        _ctx: impl InvokeContext,
        r: c::MoveToSaturationRequest<'_>,
    ) -> Result<(), Error> {
        if !RGB {
            return Err(ErrorCode::CommandNotFound.into());
        }
        let (saturation, duration) = (r.saturation()?, r.transition_time()?);
        if saturation == u8::MAX || duration == u16::MAX {
            return Err(ErrorCode::ConstraintError.into());
        }
        if !self.proceed(r.options_mask()?, r.options_override()?)? {
            return Ok(());
        }
        self.0
            .execute_color_plan(ColorPlan::SaturationMoveTo {
                target: saturation as f64,
                transition_ms: duration as u64 * 100,
            })
            .await
    }
    async fn handle_move_saturation(
        &self,
        _ctx: impl InvokeContext,
        r: c::MoveSaturationRequest<'_>,
    ) -> Result<(), Error> {
        if !RGB {
            return Err(ErrorCode::CommandNotFound.into());
        }
        let (mode, rate) = (r.move_mode()?, r.rate()?);
        if mode != c::MoveModeEnum::Stop && rate == 0 {
            return Err(ErrorCode::InvalidCommand.into());
        }
        if !self.proceed(r.options_mask()?, r.options_override()?)? {
            return Ok(());
        }
        if mode == c::MoveModeEnum::Stop {
            return self.0.stop_hs().await;
        }
        let rate = if mode == c::MoveModeEnum::Up {
            rate as f64
        } else {
            -(rate as f64)
        };
        self.0
            .execute_color_plan(ColorPlan::SaturationMove { rate })
            .await
    }
    async fn handle_step_saturation(
        &self,
        _ctx: impl InvokeContext,
        r: c::StepSaturationRequest<'_>,
    ) -> Result<(), Error> {
        if !RGB {
            return Err(ErrorCode::CommandNotFound.into());
        }
        let (mode, size, duration) = (r.step_mode()?, r.step_size()?, r.transition_time()?);
        if size == 0 {
            return Err(ErrorCode::InvalidCommand.into());
        }
        if !self.proceed(r.options_mask()?, r.options_override()?)? {
            return Ok(());
        }
        let delta = if mode == c::StepModeEnum::Up {
            size as f64
        } else {
            -(size as f64)
        };
        self.0
            .execute_color_plan(ColorPlan::SaturationStep {
                delta,
                transition_ms: duration as u64 * 100,
            })
            .await
    }
    async fn handle_move_to_hue_and_saturation(
        &self,
        _ctx: impl InvokeContext,
        r: c::MoveToHueAndSaturationRequest<'_>,
    ) -> Result<(), Error> {
        if !RGB {
            return Err(ErrorCode::CommandNotFound.into());
        }
        let (hue, saturation, duration) = (r.hue()?, r.saturation()?, r.transition_time()?);
        if hue == u8::MAX || saturation == u8::MAX || duration == u16::MAX {
            return Err(ErrorCode::ConstraintError.into());
        }
        if !self.proceed(r.options_mask()?, r.options_override()?)? {
            return Ok(());
        }
        let target = hs::Hs {
            hue: hue as f64,
            saturation: saturation as f64,
        };
        self.0
            .execute_color_plan(ColorPlan::HueAndSaturationMoveTo {
                target,
                transition_ms: duration as u64 * 100,
            })
            .await
    }
    async fn handle_move_to_color_temperature(
        &self,
        _ctx: impl InvokeContext,
        r: c::MoveToColorTemperatureRequest<'_>,
    ) -> Result<(), Error> {
        if !CT {
            return Err(ErrorCode::CommandNotFound.into());
        }
        let target = r.color_temperature_mireds()?;
        let duration = r.transition_time()?;
        if target > color::XY_MAX || duration == u16::MAX {
            return Err(ErrorCode::ConstraintError.into());
        }
        if !self.proceed(r.options_mask()?, r.options_override()?)? {
            return Ok(());
        }
        let bounds = self
            .0
            .publication()
            .temperature
            .ok_or(ErrorCode::CommandNotFound)?
            .bounds;
        self.0
            .execute_color_plan(ColorPlan::TemperatureMoveTo {
                target_mireds: target.clamp(bounds.0, bounds.1),
                transition_ms: duration as u64 * 100,
            })
            .await
    }
    async fn handle_move_to_color(
        &self,
        _ctx: impl InvokeContext,
        r: c::MoveToColorRequest<'_>,
    ) -> Result<(), Error> {
        if !RGB {
            return Err(ErrorCode::CommandNotFound.into());
        }
        let (x, y, duration) = (r.color_x()?, r.color_y()?, r.transition_time()?);
        if x > color::XY_MAX || y > color::XY_MAX || duration == u16::MAX {
            return Err(ErrorCode::ConstraintError.into());
        }
        if !self.proceed(r.options_mask()?, r.options_override()?)? {
            return Ok(());
        }
        self.0
            .execute_color_plan(ColorPlan::XyMoveTo {
                x,
                y,
                transition_ms: duration as u64 * 100,
            })
            .await
    }
    async fn handle_move_color(
        &self,
        _ctx: impl InvokeContext,
        r: c::MoveColorRequest<'_>,
    ) -> Result<(), Error> {
        if !RGB {
            return Err(ErrorCode::CommandNotFound.into());
        }
        let (rate_x, rate_y) = (r.rate_x()?, r.rate_y()?);
        if !self.proceed(r.options_mask()?, r.options_override()?)? {
            return Ok(());
        }
        if rate_x == 0 && rate_y == 0 {
            return self.0.stop(LightDimension::Color).await;
        }
        self.0
            .execute_color_plan(ColorPlan::XyMove { rate_x, rate_y })
            .await
    }
    async fn handle_step_color(
        &self,
        _ctx: impl InvokeContext,
        r: c::StepColorRequest<'_>,
    ) -> Result<(), Error> {
        if !RGB {
            return Err(ErrorCode::CommandNotFound.into());
        }
        let (x, y, duration) = (r.step_x()?, r.step_y()?, r.transition_time()?);
        if x == 0 && y == 0 {
            return Err(ErrorCode::InvalidCommand.into());
        }
        if duration == u16::MAX {
            return Err(ErrorCode::ConstraintError.into());
        }
        if !self.proceed(r.options_mask()?, r.options_override()?)? {
            return Ok(());
        }
        self.0
            .execute_color_plan(ColorPlan::XyStep {
                delta_x: x,
                delta_y: y,
                transition_ms: duration as u64 * 100,
            })
            .await
    }
    async fn handle_stop_move_step(
        &self,
        _ctx: impl InvokeContext,
        r: c::StopMoveStepRequest<'_>,
    ) -> Result<(), Error> {
        if !self.proceed(r.options_mask()?, r.options_override()?)? {
            return Ok(());
        }
        self.0.stop(LightDimension::Color).await
    }
    async fn handle_move_color_temperature(
        &self,
        _ctx: impl InvokeContext,
        r: c::MoveColorTemperatureRequest<'_>,
    ) -> Result<(), Error> {
        if !CT {
            return Err(ErrorCode::CommandNotFound.into());
        }
        let (mode, rate, min, max, mask, override_) = (
            r.move_mode()?,
            r.rate()?,
            r.color_temperature_minimum_mireds()?,
            r.color_temperature_maximum_mireds()?,
            r.options_mask()?,
            r.options_override()?,
        );
        if min > color::XY_MAX || max > color::XY_MAX {
            return Err(ErrorCode::ConstraintError.into());
        }
        if mode != c::MoveModeEnum::Stop && rate == 0 {
            return Err(ErrorCode::InvalidCommand.into());
        }
        if !self.proceed(mask, override_)? {
            return Ok(());
        }
        if mode == c::MoveModeEnum::Stop {
            return self.0.stop(LightDimension::Color).await;
        }
        let (min_mireds, max_mireds) = self.bounds(min, max)?;
        self.0
            .execute_color_plan(ColorPlan::TemperatureMove {
                direction: if mode == c::MoveModeEnum::Up {
                    MoveDirection::Up
                } else {
                    MoveDirection::Down
                },
                rate,
                min_mireds,
                max_mireds,
            })
            .await
    }
    async fn handle_step_color_temperature(
        &self,
        _ctx: impl InvokeContext,
        r: c::StepColorTemperatureRequest<'_>,
    ) -> Result<(), Error> {
        if !CT {
            return Err(ErrorCode::CommandNotFound.into());
        }
        let (mode, size, duration, min, max, mask, override_) = (
            r.step_mode()?,
            r.step_size()?,
            r.transition_time()?,
            r.color_temperature_minimum_mireds()?,
            r.color_temperature_maximum_mireds()?,
            r.options_mask()?,
            r.options_override()?,
        );
        if min > color::XY_MAX || max > color::XY_MAX {
            return Err(ErrorCode::ConstraintError.into());
        }
        if size == 0 {
            return Err(ErrorCode::InvalidCommand.into());
        }
        if duration == u16::MAX {
            return Err(ErrorCode::ConstraintError.into());
        }
        if !self.proceed(mask, override_)? {
            return Ok(());
        }
        let (lo, hi) = self.bounds(min, max)?;
        self.0
            .execute_color_plan(ColorPlan::TemperatureStep {
                direction: if mode == c::StepModeEnum::Up {
                    MoveDirection::Up
                } else {
                    MoveDirection::Down
                },
                size,
                transition_ms: duration as u64 * 100,
                min_mireds: lo,
                max_mireds: hi,
            })
            .await
    }
    unsupported_color!(handle_enhanced_move_to_hue:EnhancedMoveToHueRequest,
        handle_enhanced_move_hue:EnhancedMoveHueRequest,handle_enhanced_step_hue:EnhancedStepHueRequest,
        handle_enhanced_move_to_hue_and_saturation:EnhancedMoveToHueAndSaturationRequest,handle_color_loop_set:ColorLoopSetRequest);
}
