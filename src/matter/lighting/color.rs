use super::{AdjustmentKind, LightingHandler};
use crate::device::{
    Capability, DeviceCommand, NumericRange, Property, PropertyState, PropertyValue, RgbColor,
};
use rs_matter::{
    dm::{
        Cluster, InvokeContext, ReadContext, WriteContext,
        clusters::app::color_control::{RgbGamma, SetDeviceColor},
        clusters::decl::color_control,
    },
    error::{Error, ErrorCode},
    tlv::Nullable,
    with,
};

pub(in crate::matter) const fn color_cluster(xy: bool, temperature: bool) -> Cluster<'static> {
    let mut features = color_control::Feature::empty();
    if xy {
        features = features.union(color_control::Feature::XY);
    }
    if temperature {
        features = features.union(color_control::Feature::COLOR_TEMPERATURE);
    }
    let cluster = color_control::FULL_CLUSTER.with_features(features.bits());
    let cluster = if xy && temperature {
        cluster.with_attrs(with!(
            required;
            color_control::AttributeId::CurrentX
                | color_control::AttributeId::CurrentY
                | color_control::AttributeId::RemainingTime
                | color_control::AttributeId::ColorTemperatureMireds
                | color_control::AttributeId::ColorMode
                | color_control::AttributeId::Options
                | color_control::AttributeId::EnhancedColorMode
                | color_control::AttributeId::ColorCapabilities
                | color_control::AttributeId::ColorTempPhysicalMinMireds
                | color_control::AttributeId::ColorTempPhysicalMaxMireds
                | color_control::AttributeId::CoupleColorTempToLevelMinMireds
                | color_control::AttributeId::StartUpColorTemperatureMireds
        ))
    } else if xy {
        cluster.with_attrs(with!(
            required;
            color_control::AttributeId::CurrentX
                | color_control::AttributeId::CurrentY
                | color_control::AttributeId::RemainingTime
                | color_control::AttributeId::ColorMode
                | color_control::AttributeId::Options
                | color_control::AttributeId::EnhancedColorMode
                | color_control::AttributeId::ColorCapabilities
        ))
    } else {
        cluster.with_attrs(with!(
            required;
            color_control::AttributeId::RemainingTime
                | color_control::AttributeId::ColorTemperatureMireds
                | color_control::AttributeId::ColorMode
                | color_control::AttributeId::Options
                | color_control::AttributeId::EnhancedColorMode
                | color_control::AttributeId::ColorCapabilities
                | color_control::AttributeId::ColorTempPhysicalMinMireds
                | color_control::AttributeId::ColorTempPhysicalMaxMireds
                | color_control::AttributeId::CoupleColorTempToLevelMinMireds
                | color_control::AttributeId::StartUpColorTemperatureMireds
        ))
    };
    let cluster = if xy && temperature {
        cluster.with_cmds(with!(
            color_control::CommandId::MoveToColor
                | color_control::CommandId::MoveColor
                | color_control::CommandId::StepColor
                | color_control::CommandId::MoveToColorTemperature
                | color_control::CommandId::StopMoveStep
                | color_control::CommandId::MoveColorTemperature
                | color_control::CommandId::StepColorTemperature
        ))
    } else if xy {
        cluster.with_cmds(with!(
            color_control::CommandId::MoveToColor
                | color_control::CommandId::MoveColor
                | color_control::CommandId::StepColor
                | color_control::CommandId::StopMoveStep
        ))
    } else {
        cluster.with_cmds(with!(
            color_control::CommandId::MoveToColorTemperature
                | color_control::CommandId::StopMoveStep
                | color_control::CommandId::MoveColorTemperature
                | color_control::CommandId::StepColorTemperature
        ))
    };
    cluster.with_events(with!())
}

impl color_control::ClusterHandler for LightingHandler {
    const CLUSTER: Cluster<'static> = color_cluster(true, true);

    fn dataver(&self) -> u32 {
        self.color_dataver.get()
    }
    fn dataver_changed(&self) {
        self.color_dataver.changed();
    }
    fn current_x(&self, _ctx: impl ReadContext) -> Result<u16, Error> {
        self.color()
            .map(rgb_to_xy)
            .map(|value| value.0)
            .ok_or(ErrorCode::Failure.into())
    }
    fn current_y(&self, _ctx: impl ReadContext) -> Result<u16, Error> {
        self.color()
            .map(rgb_to_xy)
            .map(|value| value.1)
            .ok_or(ErrorCode::Failure.into())
    }
    fn remaining_time(&self, _ctx: impl ReadContext) -> Result<u16, Error> {
        Ok(self.adjustment_remaining_ds())
    }
    fn color_temperature_mireds(&self, _ctx: impl ReadContext) -> Result<u16, Error> {
        let kelvin = self.color_temperature().ok_or(ErrorCode::Failure)?;
        u16::try_from((1_000_000_u32 + kelvin / 2) / kelvin).map_err(|_| ErrorCode::Failure.into())
    }
    fn color_mode(&self, _ctx: impl ReadContext) -> Result<color_control::ColorModeEnum, Error> {
        if self.capabilities.borrow().contains(&Capability::Color)
            && self.color_temperature_range().is_some()
        {
            Err(ErrorCode::Failure.into())
        } else if self.capabilities.borrow().contains(&Capability::Color) {
            Ok(color_control::ColorModeEnum::CurrentXAndCurrentY)
        } else {
            Ok(color_control::ColorModeEnum::ColorTemperatureMireds)
        }
    }
    fn options(&self, _ctx: impl ReadContext) -> Result<color_control::OptionsBitmap, Error> {
        Ok(self.color_options.get())
    }
    fn number_of_primaries(&self, _ctx: impl ReadContext) -> Result<Nullable<u8>, Error> {
        Ok(Nullable::none())
    }
    fn enhanced_color_mode(
        &self,
        _ctx: impl ReadContext,
    ) -> Result<color_control::EnhancedColorModeEnum, Error> {
        if self.capabilities.borrow().contains(&Capability::Color)
            && self.color_temperature_range().is_some()
        {
            Err(ErrorCode::Failure.into())
        } else if self.capabilities.borrow().contains(&Capability::Color) {
            Ok(color_control::EnhancedColorModeEnum::CurrentXAndCurrentY)
        } else {
            Ok(color_control::EnhancedColorModeEnum::ColorTemperatureMireds)
        }
    }
    fn color_capabilities(
        &self,
        _ctx: impl ReadContext,
    ) -> Result<color_control::ColorCapabilitiesBitmap, Error> {
        let mut capabilities = color_control::ColorCapabilitiesBitmap::empty();
        if self.color_temperature_range().is_some() {
            capabilities |= color_control::ColorCapabilitiesBitmap::COLOR_TEMPERATURE;
        }
        if self.capabilities.borrow().contains(&Capability::Color) {
            capabilities |= color_control::ColorCapabilitiesBitmap::XY;
        }
        Ok(capabilities)
    }
    fn color_temp_physical_min_mireds(&self, _ctx: impl ReadContext) -> Result<u16, Error> {
        let range = self
            .color_temperature_range()
            .ok_or(ErrorCode::AttributeNotFound)?;
        Ok((1_000_000.0 / range.maximum).ceil() as u16)
    }
    fn color_temp_physical_max_mireds(&self, _ctx: impl ReadContext) -> Result<u16, Error> {
        let range = self
            .color_temperature_range()
            .ok_or(ErrorCode::AttributeNotFound)?;
        Ok((1_000_000.0 / range.minimum).floor() as u16)
    }
    fn couple_color_temp_to_level_min_mireds(&self, _ctx: impl ReadContext) -> Result<u16, Error> {
        self.mired_bounds()
            .map(|(minimum, _)| minimum)
            .ok_or(ErrorCode::AttributeNotFound.into())
    }
    fn start_up_color_temperature_mireds(
        &self,
        _ctx: impl ReadContext,
    ) -> Result<Nullable<u16>, Error> {
        Err(ErrorCode::AttributeNotFound.into())
    }
    fn set_options(
        &self,
        ctx: impl WriteContext,
        value: color_control::OptionsBitmap,
    ) -> Result<(), Error> {
        self.color_options.set(value);
        ctx.notify_changed();
        Ok(())
    }
    fn set_start_up_color_temperature_mireds(
        &self,
        _ctx: impl WriteContext,
        _value: Nullable<u16>,
    ) -> Result<(), Error> {
        Err(ErrorCode::UnsupportedAccess.into())
    }

    fn handle_move_to_hue(
        &self,
        _ctx: impl InvokeContext,
        _request: color_control::MoveToHueRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::InvalidCommand.into())
    }
    fn handle_move_hue(
        &self,
        _ctx: impl InvokeContext,
        _request: color_control::MoveHueRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::InvalidCommand.into())
    }
    fn handle_step_hue(
        &self,
        _ctx: impl InvokeContext,
        _request: color_control::StepHueRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::InvalidCommand.into())
    }
    fn handle_move_to_saturation(
        &self,
        _ctx: impl InvokeContext,
        _request: color_control::MoveToSaturationRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::InvalidCommand.into())
    }
    fn handle_move_saturation(
        &self,
        _ctx: impl InvokeContext,
        _request: color_control::MoveSaturationRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::InvalidCommand.into())
    }
    fn handle_step_saturation(
        &self,
        _ctx: impl InvokeContext,
        _request: color_control::StepSaturationRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::InvalidCommand.into())
    }
    fn handle_move_to_hue_and_saturation(
        &self,
        _ctx: impl InvokeContext,
        _request: color_control::MoveToHueAndSaturationRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::InvalidCommand.into())
    }
    fn handle_move_to_color(
        &self,
        _ctx: impl InvokeContext,
        _request: color_control::MoveToColorRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::Failure.into())
    }
    fn handle_move_color(
        &self,
        _ctx: impl InvokeContext,
        _request: color_control::MoveColorRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::Failure.into())
    }
    fn handle_step_color(
        &self,
        _ctx: impl InvokeContext,
        _request: color_control::StepColorRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::Failure.into())
    }
    fn handle_move_to_color_temperature(
        &self,
        _ctx: impl InvokeContext,
        _request: color_control::MoveToColorTemperatureRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::Failure.into())
    }
    fn handle_enhanced_move_to_hue(
        &self,
        _ctx: impl InvokeContext,
        _request: color_control::EnhancedMoveToHueRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::InvalidCommand.into())
    }
    fn handle_enhanced_move_hue(
        &self,
        _ctx: impl InvokeContext,
        _request: color_control::EnhancedMoveHueRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::InvalidCommand.into())
    }
    fn handle_enhanced_step_hue(
        &self,
        _ctx: impl InvokeContext,
        _request: color_control::EnhancedStepHueRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::InvalidCommand.into())
    }
    fn handle_enhanced_move_to_hue_and_saturation(
        &self,
        _ctx: impl InvokeContext,
        _request: color_control::EnhancedMoveToHueAndSaturationRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::InvalidCommand.into())
    }
    fn handle_color_loop_set(
        &self,
        _ctx: impl InvokeContext,
        _request: color_control::ColorLoopSetRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::InvalidCommand.into())
    }
    fn handle_stop_move_step(
        &self,
        _ctx: impl InvokeContext,
        _request: color_control::StopMoveStepRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::Failure.into())
    }
    fn handle_move_color_temperature(
        &self,
        _ctx: impl InvokeContext,
        _request: color_control::MoveColorTemperatureRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::Failure.into())
    }
    fn handle_step_color_temperature(
        &self,
        _ctx: impl InvokeContext,
        _request: color_control::StepColorTemperatureRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::Failure.into())
    }
}

pub(in crate::matter) fn rgb_to_xy(color: RgbColor) -> (u16, u16) {
    let linear = |channel: u8| {
        let value = f64::from(channel) / 255.0;
        if value <= 0.04045 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    let red = linear(color.red);
    let green = linear(color.green);
    let blue = linear(color.blue);
    let x = 0.4124 * red + 0.3576 * green + 0.1805 * blue;
    let y = 0.2126 * red + 0.7152 * green + 0.0722 * blue;
    let z = 0.0193 * red + 0.1192 * green + 0.9505 * blue;
    let total = x + y + z;
    if total <= f64::EPSILON {
        return (0, 0);
    }
    (
        (x / total * f64::from(0xfeff_u16)).round() as u16,
        (y / total * f64::from(0xfeff_u16)).round() as u16,
    )
}

pub(super) fn kelvin_to_mired(kelvin: u32) -> Option<u16> {
    (kelvin > 0)
        .then(|| (1_000_000_u32 + kelvin / 2) / kelvin)
        .and_then(|value| u16::try_from(value).ok())
}

impl LightingHandler {
    pub(super) fn color_temperature(&self) -> Option<u32> {
        match self
            .service
            .snapshot(&self.feature)?
            .property(Property::ColorTemperature)?
        {
            PropertyState::Current {
                value: PropertyValue::ColorTemperature(value),
                ..
            } => Some(*value),
            _ => None,
        }
    }

    pub(super) fn current_mireds(&self) -> Option<u16> {
        kelvin_to_mired(self.color_temperature()?)
    }

    pub(super) fn color(&self) -> Option<RgbColor> {
        match self
            .service
            .snapshot(&self.feature)?
            .property(Property::Color)?
        {
            PropertyState::Current {
                value: PropertyValue::Color(value),
                ..
            } => Some(*value),
            _ => None,
        }
    }

    pub(super) fn color_temperature_range(&self) -> Option<NumericRange> {
        self.capabilities
            .borrow()
            .iter()
            .find_map(|capability| match capability {
                Capability::ColorTemperature(range) => Some(*range),
                _ => None,
            })
    }

    pub(super) fn quantized_kelvin_for_mired(&self, mireds: u16) -> Result<u32, Error> {
        if mireds == 0 {
            return Err(ErrorCode::ConstraintError.into());
        }
        let range = self
            .color_temperature_range()
            .ok_or(ErrorCode::InvalidCommand)?;
        let kelvin = 1_000_000.0 / f64::from(mireds);
        if !kelvin.is_finite() || kelvin < range.minimum || kelvin > range.maximum {
            return Err(ErrorCode::ConstraintError.into());
        }
        let quantized =
            range.minimum + ((kelvin - range.minimum) / range.step).round() * range.step;
        Ok(quantized.clamp(range.minimum, range.maximum).round() as u32)
    }

    pub(super) fn mired_bounds(&self) -> Option<(u16, u16)> {
        let range = self.color_temperature_range()?;
        Some((
            (1_000_000.0 / range.maximum).ceil() as u16,
            (1_000_000.0 / range.minimum).floor() as u16,
        ))
    }

    pub(super) fn should_execute_color(
        &self,
        mask: color_control::OptionsBitmap,
        override_value: color_control::OptionsBitmap,
    ) -> Result<bool, Error> {
        if self.power() == Some(true) {
            return Ok(true);
        }
        if self.power().is_none() {
            return Err(ErrorCode::Failure.into());
        }
        let effective = (self.color_options.get() & !mask) | (override_value & mask);
        Ok(effective.contains(color_control::OptionsBitmap::EXECUTE_IF_OFF))
    }

    pub(in crate::matter) async fn invoke_color(
        &self,
        ctx: &impl InvokeContext,
    ) -> Result<(), Error> {
        match color_control::CommandId::try_from(ctx.cmd().cmd_id)? {
            color_control::CommandId::MoveToColorTemperature => {
                ctx.data().structure()?;
                let request = color_control::MoveToColorTemperatureRequest::new(ctx.data().clone());
                let mireds = request.color_temperature_mireds()?;
                let transition = request.transition_time()?;
                if !self
                    .should_execute_color(request.options_mask()?, request.options_override()?)?
                {
                    return Ok(());
                }
                let kelvin = self.quantized_kelvin_for_mired(mireds)?;
                if transition == 0 {
                    self.command(DeviceCommand::SetColorTemperature(kelvin))
                        .await
                } else {
                    let start_mireds = self.current_mireds().ok_or(ErrorCode::Failure)?;
                    let intent = self
                        .service
                        .begin_command_intent(&self.feature, [Property::ColorTemperature]);
                    self.start_adjustment(
                        intent,
                        u32::from(transition),
                        AdjustmentKind::ColorTemperature {
                            start_mireds,
                            target_mireds: mireds,
                        },
                    )
                }
            }
            color_control::CommandId::MoveToColor => {
                ctx.data().structure()?;
                let request = color_control::MoveToColorRequest::new(ctx.data().clone());
                let x = request.color_x()?;
                let y = request.color_y()?;
                let transition = request.transition_time()?;
                if !self
                    .should_execute_color(request.options_mask()?, request.options_override()?)?
                {
                    return Ok(());
                }
                if x > 0xfeff || y > 0xfeff {
                    return Err(ErrorCode::ConstraintError.into());
                }
                if transition > 0 {
                    let (start_x, start_y) =
                        self.color().map(rgb_to_xy).ok_or(ErrorCode::Failure)?;
                    let intent = self
                        .service
                        .begin_command_intent(&self.feature, [Property::Color]);
                    return self.start_adjustment(
                        intent,
                        u32::from(transition),
                        AdjustmentKind::Xy {
                            start_x,
                            start_y,
                            target_x: x,
                            target_y: y,
                        },
                    );
                }
                let (red, green, blue) = SetDeviceColor::Xy { x, y }.to_rgb(RgbGamma::SRgb);
                self.command(DeviceCommand::SetColor(RgbColor { red, green, blue }))
                    .await
            }
            color_control::CommandId::StepColor => {
                ctx.data().structure()?;
                let request = color_control::StepColorRequest::new(ctx.data().clone());
                if !self
                    .should_execute_color(request.options_mask()?, request.options_override()?)?
                {
                    return Ok(());
                }
                let (start_x, start_y) = self.color().map(rgb_to_xy).ok_or(ErrorCode::Failure)?;
                let target_x = i32::from(start_x)
                    .saturating_add(i32::from(request.step_x()?))
                    .clamp(0, i32::from(0xfeff_u16)) as u16;
                let target_y = i32::from(start_y)
                    .saturating_add(i32::from(request.step_y()?))
                    .clamp(0, i32::from(0xfeff_u16)) as u16;
                let transition = request.transition_time()?;
                if transition == 0 {
                    let (red, green, blue) = SetDeviceColor::Xy {
                        x: target_x,
                        y: target_y,
                    }
                    .to_rgb(RgbGamma::SRgb);
                    self.command(DeviceCommand::SetColor(RgbColor { red, green, blue }))
                        .await
                } else {
                    let intent = self
                        .service
                        .begin_command_intent(&self.feature, [Property::Color]);
                    self.start_adjustment(
                        intent,
                        u32::from(transition),
                        AdjustmentKind::Xy {
                            start_x,
                            start_y,
                            target_x,
                            target_y,
                        },
                    )
                }
            }
            color_control::CommandId::MoveColor => {
                ctx.data().structure()?;
                let request = color_control::MoveColorRequest::new(ctx.data().clone());
                if !self
                    .should_execute_color(request.options_mask()?, request.options_override()?)?
                {
                    return Ok(());
                }
                let rate_x = request.rate_x()?;
                let rate_y = request.rate_y()?;
                if rate_x == 0 && rate_y == 0 {
                    return Err(ErrorCode::ConstraintError.into());
                }
                let (start_x, start_y) = self.color().map(rgb_to_xy).ok_or(ErrorCode::Failure)?;
                let target_x = if rate_x < 0 {
                    0
                } else if rate_x > 0 {
                    0xfeff
                } else {
                    start_x
                };
                let target_y = if rate_y < 0 {
                    0
                } else if rate_y > 0 {
                    0xfeff
                } else {
                    start_y
                };
                let axis_ds = |start: u16, target: u16, rate: i16| {
                    if rate == 0 {
                        0
                    } else {
                        (u32::from(start.abs_diff(target)) * 10)
                            .div_ceil(u32::from(rate.unsigned_abs()))
                    }
                };
                let duration = axis_ds(start_x, target_x, rate_x)
                    .max(axis_ds(start_y, target_y, rate_y))
                    .max(1);
                let intent = self
                    .service
                    .begin_command_intent(&self.feature, [Property::Color]);
                self.start_adjustment(
                    intent,
                    duration,
                    AdjustmentKind::XyMove {
                        start_x,
                        start_y,
                        rate_x,
                        rate_y,
                    },
                )
            }
            color_control::CommandId::StepColorTemperature => {
                ctx.data().structure()?;
                let request = color_control::StepColorTemperatureRequest::new(ctx.data().clone());
                if !self
                    .should_execute_color(request.options_mask()?, request.options_override()?)?
                {
                    return Ok(());
                }
                let (physical_min, physical_max) =
                    self.mired_bounds().ok_or(ErrorCode::InvalidCommand)?;
                let minimum = match request.color_temperature_minimum_mireds()? {
                    0 => physical_min,
                    value => value.max(physical_min),
                };
                let maximum = match request.color_temperature_maximum_mireds()? {
                    0 => physical_max,
                    value => value.min(physical_max),
                };
                if minimum > maximum {
                    return Err(ErrorCode::ConstraintError.into());
                }
                let start_mireds = self.current_mireds().ok_or(ErrorCode::Failure)?;
                let size = request.step_size()?;
                let target_mireds = match request.step_mode()? {
                    color_control::StepModeEnum::Up => {
                        start_mireds.saturating_add(size).min(maximum)
                    }
                    color_control::StepModeEnum::Down => {
                        start_mireds.saturating_sub(size).max(minimum)
                    }
                };
                let transition = request.transition_time()?;
                if transition == 0 {
                    self.command(DeviceCommand::SetColorTemperature(
                        self.quantized_kelvin_for_mired(target_mireds)?,
                    ))
                    .await
                } else {
                    let intent = self
                        .service
                        .begin_command_intent(&self.feature, [Property::ColorTemperature]);
                    self.start_adjustment(
                        intent,
                        u32::from(transition),
                        AdjustmentKind::ColorTemperature {
                            start_mireds,
                            target_mireds,
                        },
                    )
                }
            }
            color_control::CommandId::MoveColorTemperature => {
                ctx.data().structure()?;
                let request = color_control::MoveColorTemperatureRequest::new(ctx.data().clone());
                if !self
                    .should_execute_color(request.options_mask()?, request.options_override()?)?
                {
                    return Ok(());
                }
                let (physical_min, physical_max) =
                    self.mired_bounds().ok_or(ErrorCode::InvalidCommand)?;
                let minimum = match request.color_temperature_minimum_mireds()? {
                    0 => physical_min,
                    value => value.max(physical_min),
                };
                let maximum = match request.color_temperature_maximum_mireds()? {
                    0 => physical_max,
                    value => value.min(physical_max),
                };
                if minimum > maximum {
                    return Err(ErrorCode::ConstraintError.into());
                }
                let rate = request.rate()?;
                let start_mireds = self.current_mireds().ok_or(ErrorCode::Failure)?;
                match request.move_mode()? {
                    color_control::MoveModeEnum::Stop => {
                        self.adjustment.borrow_mut().take();
                        self.wake.notify(usize::MAX);
                        Ok(())
                    }
                    _ if rate == 0 => Err(ErrorCode::ConstraintError.into()),
                    mode
                    @ (color_control::MoveModeEnum::Up | color_control::MoveModeEnum::Down) => {
                        let target_mireds = if matches!(mode, color_control::MoveModeEnum::Up) {
                            maximum
                        } else {
                            minimum
                        };
                        let duration = (u32::from(start_mireds.abs_diff(target_mireds)) * 10
                            / u32::from(rate))
                        .max(1);
                        let intent = self
                            .service
                            .begin_command_intent(&self.feature, [Property::ColorTemperature]);
                        self.start_adjustment(
                            intent,
                            duration,
                            AdjustmentKind::ColorTemperature {
                                start_mireds,
                                target_mireds,
                            },
                        )
                    }
                }
            }
            color_control::CommandId::StopMoveStep => {
                ctx.data().structure()?;
                let request = color_control::StopMoveStepRequest::new(ctx.data().clone());
                request.options_mask()?;
                request.options_override()?;
                self.adjustment.borrow_mut().take();
                self.wake.notify(usize::MAX);
                self.service.stop_adjustment(&self.feature, Property::Color);
                self.service
                    .stop_adjustment(&self.feature, Property::ColorTemperature);
                Ok(())
            }
            _ => Err(ErrorCode::InvalidCommand.into()),
        }
    }
}
