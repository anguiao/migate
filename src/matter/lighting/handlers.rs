use super::{
    Lighting,
    controllers::{self, color},
};
use crate::device::{Command, LightDimension, LightMode};
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
            let power = self.0.refresh().power;
            let task = self.0.level.borrow().move_to_level(
                request.level()?,
                $with,
                request.transition_time()?.into_option(),
                request.options_mask()?.bits(),
                request.options_override()?.bits(),
                power,
            )?;
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
            let power = self.0.refresh().power;
            let task = self.0.level.borrow().move_level(
                $with,
                request.move_mode()?,
                request.rate()?.into_option(),
                request.options_mask()?.bits(),
                request.options_override()?.bits(),
                power,
            )?;
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
            let power = self.0.refresh().power;
            let task = self.0.level.borrow().step(
                $with,
                request.step_mode()?,
                request.step_size()?,
                request.transition_time()?.into_option(),
                request.options_mask()?.bits(),
                request.options_override()?.bits(),
                power,
            )?;
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
                    self.0.level.borrow().options,
                    mask,
                    override_,
                )?
            {
                return Ok(());
            }
            self.0.stop(LightDimension::Brightness)
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
        Ok(self.0.level.borrow().remaining_time.value)
    }
    async fn options(&self, _ctx: impl ReadContext) -> Result<l::OptionsBitmap, Error> {
        Ok(l::OptionsBitmap::from_bits_retain(
            self.0.level.borrow().options,
        ))
    }
    async fn on_level(&self, _ctx: impl ReadContext) -> Result<Nullable<u8>, Error> {
        Ok(Nullable::new(self.0.level.borrow().on_level))
    }
    async fn set_options(
        &self,
        ctx: impl WriteContext,
        value: l::OptionsBitmap,
    ) -> Result<(), Error> {
        if value.bits() & !1 != 0 {
            return Err(ErrorCode::ConstraintError.into());
        }
        let changed = self.0.level.borrow().options != value.bits();
        self.0.level.borrow_mut().options = value.bits();
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
        let changed = self.0.level.borrow().on_level != value;
        self.0.level.borrow_mut().on_level = value;
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
impl<const RGB: bool> ColorHandler<'_, RGB> {
    fn proceed(&self, mask: c::OptionsBitmap, override_: c::OptionsBitmap) -> Result<bool, Error> {
        let power = self.0.refresh().power;
        controllers::should_continue(
            power,
            self.0.color.borrow().options,
            mask.bits(),
            override_.bits(),
        )
    }
    fn temperature_task(&self, target: u16, transition: u16) -> Result<color::Task, Error> {
        if target > color::XY_MAX || transition == u16::MAX {
            return Err(ErrorCode::ConstraintError.into());
        }
        let target = target.clamp(self.0.bounds.0, self.0.bounds.1);
        let start = if transition == 0 {
            self.0
                .color
                .borrow()
                .color_temperature_mireds
                .unwrap_or(target)
        } else {
            self.0
                .color
                .borrow()
                .color_temperature_mireds
                .ok_or(ErrorCode::InvalidState)?
        };
        Ok(color::Task::ColorTemperatureMoveTo {
            start,
            target_mireds: target,
            transition_ms: transition as u64 * 100,
        })
    }
    fn xy_task(&self, x: u16, y: u16, transition: u16) -> Result<color::Task, Error> {
        if x > color::XY_MAX || y > color::XY_MAX || transition == u16::MAX {
            return Err(ErrorCode::ConstraintError.into());
        }
        let (start_x, start_y) = if transition == 0 {
            self.0.color.borrow().xy.unwrap_or((x, y))
        } else {
            self.0.color.borrow().xy.ok_or(ErrorCode::InvalidState)?
        };
        Ok(color::Task::XyMoveTo {
            start_x,
            start_y,
            target_x: x,
            target_y: y,
            transition_ms: transition as u64 * 100,
        })
    }
    fn bounds(&self, min: u16, max: u16) -> Result<(u16, u16), Error> {
        if min > color::XY_MAX || max > color::XY_MAX {
            return Err(ErrorCode::ConstraintError.into());
        }
        let lo = if min == 0 {
            self.0.bounds.0
        } else {
            min.max(self.0.bounds.0)
        };
        let hi = if max == 0 {
            self.0.bounds.1
        } else {
            max.min(self.0.bounds.1)
        };
        if lo > hi {
            return Err(ErrorCode::InvalidCommand.into());
        }
        Ok((lo, hi))
    }
}
impl<const RGB: bool> c::ClusterAsyncHandler for ColorHandler<'_, RGB> {
    const CLUSTER: Cluster<'static> = if RGB {
        c::FULL_CLUSTER
            .with_features(0x18)
            .with_attrs(
                |a, _, _| matches!(a.id,2..=4|7|8|15|16|0x4001|0x400a..=0x400d|0xfff8..=0xfffd),
            )
            .with_cmds(with!(
                c::CommandId::MoveToColor
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
        self.0.color_version.get()
    }
    fn dataver_changed(&self) {
        self.0.color_version.changed();
    }
    async fn remaining_time(&self, _ctx: impl ReadContext) -> Result<u16, Error> {
        Ok(self.0.color.borrow().remaining_time.value)
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
        match self.0.refresh().mode {
            Some(LightMode::Rgb) => Ok(c::ColorModeEnum::CurrentXAndCurrentY),
            Some(LightMode::Temperature) => Ok(c::ColorModeEnum::ColorTemperatureMireds),
            None => Err(ErrorCode::Failure.into()),
        }
    }
    async fn enhanced_color_mode(
        &self,
        _ctx: impl ReadContext,
    ) -> Result<c::EnhancedColorModeEnum, Error> {
        match self.0.refresh().mode {
            Some(LightMode::Rgb) => Ok(c::EnhancedColorModeEnum::CurrentXAndCurrentY),
            Some(LightMode::Temperature) => Ok(c::EnhancedColorModeEnum::ColorTemperatureMireds),
            None => Err(ErrorCode::Failure.into()),
        }
    }
    async fn options(&self, _ctx: impl ReadContext) -> Result<c::OptionsBitmap, Error> {
        Ok(c::OptionsBitmap::from_bits_retain(
            self.0.color.borrow().options,
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
            0x18
        } else {
            0x10
        }))
    }
    async fn color_temp_physical_min_mireds(&self, _ctx: impl ReadContext) -> Result<u16, Error> {
        Ok(self.0.bounds.0)
    }
    async fn color_temp_physical_max_mireds(&self, _ctx: impl ReadContext) -> Result<u16, Error> {
        Ok(self.0.bounds.1)
    }
    async fn couple_color_temp_to_level_min_mireds(
        &self,
        _ctx: impl ReadContext,
    ) -> Result<u16, Error> {
        Ok(self.0.bounds.0)
    }
    async fn set_options(
        &self,
        ctx: impl WriteContext,
        value: c::OptionsBitmap,
    ) -> Result<(), Error> {
        if value.bits() & !1 != 0 {
            return Err(ErrorCode::ConstraintError.into());
        }
        let changed = self.0.color.borrow().options != value.bits();
        self.0.color.borrow_mut().options = value.bits();
        if changed {
            ctx.notify_changed();
        }
        Ok(())
    }
    async fn handle_move_to_color_temperature(
        &self,
        _ctx: impl InvokeContext,
        r: c::MoveToColorTemperatureRequest<'_>,
    ) -> Result<(), Error> {
        let target = r.color_temperature_mireds()?;
        let duration = r.transition_time()?;
        if target > color::XY_MAX || duration == u16::MAX {
            return Err(ErrorCode::ConstraintError.into());
        }
        if !self.proceed(r.options_mask()?, r.options_override()?)? {
            return Ok(());
        }
        self.0
            .execute_color(self.temperature_task(target, duration)?)
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
        self.0.execute_color(self.xy_task(x, y, duration)?).await
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
            return self.0.stop(LightDimension::Color);
        }
        let (start_x, start_y) = self.0.color.borrow().xy.ok_or(ErrorCode::InvalidState)?;
        self.0
            .execute_color(color::Task::XyMove {
                start_x,
                start_y,
                rate_x,
                rate_y,
            })
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
        let (start_x, start_y) = self.0.color.borrow().xy.ok_or(ErrorCode::InvalidState)?;
        self.0
            .execute_color(self.xy_task(
                (start_x as i32 + x as i32).clamp(0, color::XY_MAX as i32) as u16,
                (start_y as i32 + y as i32).clamp(0, color::XY_MAX as i32) as u16,
                duration,
            )?)
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
        self.0.stop(LightDimension::Color)
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
            return self.0.stop(LightDimension::Color);
        }
        let (min_mireds, max_mireds) = self.bounds(min, max)?;
        let start = self
            .0
            .color
            .borrow()
            .color_temperature_mireds
            .ok_or(ErrorCode::InvalidState)?;
        let rate_mireds_per_sec = if mode == c::MoveModeEnum::Up {
            rate as i32
        } else {
            -(rate as i32)
        };
        self.0
            .execute_color(color::Task::ColorTemperatureMove {
                start,
                rate_mireds_per_sec,
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
        let start = self
            .0
            .color
            .borrow()
            .color_temperature_mireds
            .ok_or(ErrorCode::InvalidState)?;
        let target = if mode == c::StepModeEnum::Up {
            start.saturating_add(size.min(hi.saturating_sub(start)))
        } else {
            start.saturating_sub(size.min(start.saturating_sub(lo)))
        };
        // Matter 1.6 preserves the requested Step rate when a bound clips
        // the distance. XY Step intentionally retains its whole transition.
        let transition_ms =
            (duration as u64 * 100 * start.abs_diff(target) as u64).div_ceil(size as u64);
        self.0
            .execute_color(color::Task::ColorTemperatureMoveTo {
                start,
                target_mireds: target,
                transition_ms,
            })
            .await
    }
    unsupported_color!(handle_move_to_hue:MoveToHueRequest,handle_move_hue:MoveHueRequest,handle_step_hue:StepHueRequest,
        handle_move_to_saturation:MoveToSaturationRequest,handle_move_saturation:MoveSaturationRequest,handle_step_saturation:StepSaturationRequest,
        handle_move_to_hue_and_saturation:MoveToHueAndSaturationRequest,handle_enhanced_move_to_hue:EnhancedMoveToHueRequest,
        handle_enhanced_move_hue:EnhancedMoveHueRequest,handle_enhanced_step_hue:EnhancedStepHueRequest,
        handle_enhanced_move_to_hue_and_saturation:EnhancedMoveToHueAndSaturationRequest,handle_color_loop_set:ColorLoopSetRequest);
}
