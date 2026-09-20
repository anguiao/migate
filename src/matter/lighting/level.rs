use super::{AdjustmentKind, LightingHandler};
use crate::device::{
    Capability, DeviceCommand, NumericRange, Percent, Property, PropertyState, PropertyValue,
};
use rs_matter::{
    dm::{Cluster, InvokeContext, ReadContext, WriteContext, clusters::decl::level_control},
    error::{Error, ErrorCode},
    tlv::Nullable,
    with,
};

pub(in crate::matter) const LEVEL_CLUSTER: Cluster<'static> = level_control::FULL_CLUSTER
    .with_features(level_control::Feature::ON_OFF.bits() | level_control::Feature::LIGHTING.bits())
    .with_attrs(with!(
        required;
        level_control::AttributeId::CurrentLevel
            | level_control::AttributeId::RemainingTime
            | level_control::AttributeId::MinLevel
            | level_control::AttributeId::MaxLevel
            | level_control::AttributeId::Options
            | level_control::AttributeId::OnLevel
            | level_control::AttributeId::StartUpCurrentLevel
    ))
    .with_cmds(with!(
        level_control::CommandId::MoveToLevel
            | level_control::CommandId::Move
            | level_control::CommandId::Step
            | level_control::CommandId::Stop
            | level_control::CommandId::MoveToLevelWithOnOff
            | level_control::CommandId::MoveWithOnOff
            | level_control::CommandId::StepWithOnOff
            | level_control::CommandId::StopWithOnOff
    ))
    .with_events(with!());

impl level_control::ClusterHandler for LightingHandler {
    const CLUSTER: Cluster<'static> = LEVEL_CLUSTER;

    fn dataver(&self) -> u32 {
        self.level_dataver.get()
    }

    fn dataver_changed(&self) {
        self.level_dataver.changed();
    }

    fn current_level(&self, _ctx: impl ReadContext) -> Result<Nullable<u8>, Error> {
        Ok(self
            .current_level_value()
            .map_or_else(Nullable::none, Nullable::some))
    }

    fn remaining_time(&self, _ctx: impl ReadContext) -> Result<u16, Error> {
        Ok(self.adjustment_remaining_ds())
    }

    fn min_level(&self, _ctx: impl ReadContext) -> Result<u8, Error> {
        Ok(1)
    }

    fn max_level(&self, _ctx: impl ReadContext) -> Result<u8, Error> {
        Ok(254)
    }

    fn options(&self, _ctx: impl ReadContext) -> Result<level_control::OptionsBitmap, Error> {
        Ok(self.level_options.get())
    }

    fn on_level(&self, _ctx: impl ReadContext) -> Result<Nullable<u8>, Error> {
        Ok(self.on_level.borrow().clone())
    }

    fn start_up_current_level(&self, _ctx: impl ReadContext) -> Result<Nullable<u8>, Error> {
        Err(ErrorCode::AttributeNotFound.into())
    }

    fn set_options(
        &self,
        _ctx: impl WriteContext,
        value: level_control::OptionsBitmap,
    ) -> Result<(), Error> {
        self.level_options.set(value);
        _ctx.notify_changed();
        Ok(())
    }

    fn set_on_level(&self, ctx: impl WriteContext, value: Nullable<u8>) -> Result<(), Error> {
        if value.clone().into_option().is_some_and(|value| value == 0) {
            return Err(ErrorCode::ConstraintError.into());
        }
        *self.on_level.borrow_mut() = value;
        ctx.notify_changed();
        Ok(())
    }

    fn set_start_up_current_level(
        &self,
        _ctx: impl WriteContext,
        _value: Nullable<u8>,
    ) -> Result<(), Error> {
        Err(ErrorCode::UnsupportedAccess.into())
    }

    fn handle_move_to_level(
        &self,
        _ctx: impl InvokeContext,
        _request: level_control::MoveToLevelRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::Failure.into())
    }

    fn handle_move(
        &self,
        _ctx: impl InvokeContext,
        _request: level_control::MoveRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::Failure.into())
    }

    fn handle_step(
        &self,
        _ctx: impl InvokeContext,
        _request: level_control::StepRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::Failure.into())
    }

    fn handle_stop(
        &self,
        _ctx: impl InvokeContext,
        _request: level_control::StopRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::Failure.into())
    }

    fn handle_move_to_level_with_on_off(
        &self,
        _ctx: impl InvokeContext,
        _request: level_control::MoveToLevelWithOnOffRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::Failure.into())
    }

    fn handle_move_with_on_off(
        &self,
        _ctx: impl InvokeContext,
        _request: level_control::MoveWithOnOffRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::Failure.into())
    }

    fn handle_step_with_on_off(
        &self,
        _ctx: impl InvokeContext,
        _request: level_control::StepWithOnOffRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::Failure.into())
    }

    fn handle_stop_with_on_off(
        &self,
        _ctx: impl InvokeContext,
        _request: level_control::StopWithOnOffRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::Failure.into())
    }

    fn handle_move_to_closest_frequency(
        &self,
        _ctx: impl InvokeContext,
        _request: level_control::MoveToClosestFrequencyRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::InvalidCommand.into())
    }
}

pub(super) fn matter_level(value: Percent) -> Option<u8> {
    (value.get() > 0.0).then(|| (value.get() * 254.0 / 100.0).round().clamp(1.0, 254.0) as u8)
}

impl LightingHandler {
    pub(super) fn brightness(&self) -> Option<Percent> {
        match self
            .service
            .snapshot(&self.feature)?
            .property(Property::Brightness)?
        {
            PropertyState::Current {
                value: PropertyValue::Percent(value),
                ..
            } => Some(*value),
            _ => None,
        }
    }

    pub(super) fn current_level_value(&self) -> Option<u8> {
        self.brightness().and_then(matter_level)
    }

    pub(super) fn brightness_range(&self) -> Option<NumericRange> {
        self.capabilities
            .borrow()
            .iter()
            .find_map(|capability| match capability {
                Capability::Brightness(range) => Some(*range),
                _ => None,
            })
    }

    pub(super) fn commands_for_level(
        &self,
        level: u8,
        with_on_off: bool,
    ) -> Result<Vec<DeviceCommand>, Error> {
        let range = self.brightness_range().ok_or(ErrorCode::InvalidCommand)?;
        let raw = f64::from(level) * 100.0 / 254.0;
        let quantized = (range.minimum + ((raw - range.minimum) / range.step).round() * range.step)
            .clamp(range.minimum, range.maximum);
        let percent = Percent::new(quantized).map_err(|_| ErrorCode::ConstraintError)?;
        let mut commands = if with_on_off {
            if level == 0 {
                vec![DeviceCommand::SetPower(false)]
            } else {
                vec![
                    DeviceCommand::SetBrightness(percent),
                    DeviceCommand::SetPower(true),
                ]
            }
        } else {
            vec![DeviceCommand::SetBrightness(percent)]
        };
        if level > 0
            && self
                .level_options
                .get()
                .contains(level_control::OptionsBitmap::COUPLE_COLOR_TEMP_TO_LEVEL)
            && !self.capabilities.borrow().contains(&Capability::Color)
            && let Some((minimum, maximum)) = self.mired_bounds()
        {
            let mireds =
                f64::from(maximum) - f64::from(maximum - minimum) * f64::from(level) / 254.0;
            let kelvin = 1_000_000.0 / mireds;
            let range = self
                .color_temperature_range()
                .ok_or(ErrorCode::InvalidCommand)?;
            let quantized = (range.minimum
                + ((kelvin - range.minimum) / range.step).round() * range.step)
                .clamp(range.minimum, range.maximum);
            commands.insert(
                commands.len().min(1),
                DeviceCommand::SetColorTemperature(quantized.round() as u32),
            );
        }
        Ok(commands)
    }

    pub(super) async fn set_level(&self, level: u8, with_on_off: bool) -> Result<(), Error> {
        self.command_batch(self.commands_for_level(level, with_on_off)?)
            .await
    }

    pub(super) fn should_execute_level(
        &self,
        with_on_off: bool,
        mask: level_control::OptionsBitmap,
        override_value: level_control::OptionsBitmap,
    ) -> Result<bool, Error> {
        if with_on_off || self.power() == Some(true) {
            return Ok(true);
        }
        if self.power().is_none() {
            return Err(ErrorCode::Failure.into());
        }
        let effective = (self.level_options.get() & !mask) | (override_value & mask);
        Ok(effective.contains(level_control::OptionsBitmap::EXECUTE_IF_OFF))
    }

    pub(in crate::matter) async fn invoke_level(
        &self,
        ctx: &impl InvokeContext,
    ) -> Result<(), Error> {
        let command = level_control::CommandId::try_from(ctx.cmd().cmd_id)?;
        match command {
            level_control::CommandId::MoveToLevel => {
                ctx.data().structure()?;
                let request = level_control::MoveToLevelRequest::new(ctx.data().clone());
                let level = request.level()?;
                if level == u8::MAX {
                    return Err(ErrorCode::ConstraintError.into());
                }
                let transition = request.transition_time()?.into_option().unwrap_or(0);
                if !self.should_execute_level(
                    false,
                    request.options_mask()?,
                    request.options_override()?,
                )? {
                    return Ok(());
                }
                if transition == 0 {
                    self.set_level(level, false).await
                } else {
                    let start = self.current_level_value().ok_or(ErrorCode::Failure)?;
                    let intent = self
                        .service
                        .begin_command_intent(&self.feature, [Property::Brightness]);
                    self.start_adjustment(
                        intent,
                        u32::from(transition),
                        AdjustmentKind::Brightness {
                            start,
                            target: level.max(1),
                            with_on_off: false,
                        },
                    )
                }
            }
            level_control::CommandId::MoveToLevelWithOnOff => {
                ctx.data().structure()?;
                let request = level_control::MoveToLevelWithOnOffRequest::new(ctx.data().clone());
                let level = request.level()?;
                if level == u8::MAX {
                    return Err(ErrorCode::ConstraintError.into());
                }
                let transition = request.transition_time()?.into_option().unwrap_or(0);
                request.options_mask()?;
                request.options_override()?;
                if transition == 0 {
                    self.set_level(level, true).await
                } else {
                    let start = self.current_level_value().ok_or(ErrorCode::Failure)?;
                    let intent = self.service.begin_command_intent(
                        &self.feature,
                        [Property::Brightness, Property::Power],
                    );
                    self.start_adjustment(
                        intent,
                        u32::from(transition),
                        AdjustmentKind::Brightness {
                            start,
                            target: level,
                            with_on_off: true,
                        },
                    )
                }
            }
            level_control::CommandId::Step | level_control::CommandId::StepWithOnOff => {
                ctx.data().structure()?;
                let with_on_off = command == level_control::CommandId::StepWithOnOff;
                let (mode, size, transition, mask, override_value) = if with_on_off {
                    let request = level_control::StepWithOnOffRequest::new(ctx.data().clone());
                    (
                        request.step_mode()?,
                        request.step_size()?,
                        request.transition_time()?.into_option().unwrap_or(0),
                        request.options_mask()?,
                        request.options_override()?,
                    )
                } else {
                    let request = level_control::StepRequest::new(ctx.data().clone());
                    (
                        request.step_mode()?,
                        request.step_size()?,
                        request.transition_time()?.into_option().unwrap_or(0),
                        request.options_mask()?,
                        request.options_override()?,
                    )
                };
                if !self.should_execute_level(with_on_off, mask, override_value)? {
                    return Ok(());
                }
                let current = self.current_level_value().ok_or(ErrorCode::Failure)?;
                let target = match mode {
                    level_control::StepModeEnum::Up => current.saturating_add(size).min(254),
                    level_control::StepModeEnum::Down => {
                        let target = current.saturating_sub(size);
                        if with_on_off && target <= 1 {
                            0
                        } else {
                            target.max(1)
                        }
                    }
                };
                if transition == 0 {
                    self.set_level(target, with_on_off).await
                } else {
                    let properties = if with_on_off {
                        vec![Property::Brightness, Property::Power]
                    } else {
                        vec![Property::Brightness]
                    };
                    let intent = self.service.begin_command_intent(&self.feature, properties);
                    self.start_adjustment(
                        intent,
                        u32::from(transition),
                        AdjustmentKind::Brightness {
                            start: current,
                            target,
                            with_on_off,
                        },
                    )
                }
            }
            level_control::CommandId::Stop | level_control::CommandId::StopWithOnOff => {
                ctx.data().structure()?;
                if command == level_control::CommandId::StopWithOnOff {
                    let request = level_control::StopWithOnOffRequest::new(ctx.data().clone());
                    request.options_mask()?;
                    request.options_override()?;
                } else {
                    let request = level_control::StopRequest::new(ctx.data().clone());
                    request.options_mask()?;
                    request.options_override()?;
                }
                self.adjustment.borrow_mut().take();
                self.wake.notify(usize::MAX);
                self.service
                    .stop_adjustment(&self.feature, Property::Brightness);
                Ok(())
            }
            level_control::CommandId::Move | level_control::CommandId::MoveWithOnOff => {
                ctx.data().structure()?;
                let with_on_off = command == level_control::CommandId::MoveWithOnOff;
                let (mode, rate, mask, override_value) = if with_on_off {
                    let request = level_control::MoveWithOnOffRequest::new(ctx.data().clone());
                    (
                        request.move_mode()?,
                        request.rate()?.into_option(),
                        request.options_mask()?,
                        request.options_override()?,
                    )
                } else {
                    let request = level_control::MoveRequest::new(ctx.data().clone());
                    (
                        request.move_mode()?,
                        request.rate()?.into_option(),
                        request.options_mask()?,
                        request.options_override()?,
                    )
                };
                if !self.should_execute_level(with_on_off, mask, override_value)? {
                    return Ok(());
                }
                let rate = rate
                    .filter(|rate| *rate > 0)
                    .ok_or(ErrorCode::ConstraintError)?;
                let start = self.current_level_value().ok_or(ErrorCode::Failure)?;
                let target = match mode {
                    level_control::MoveModeEnum::Up => 254,
                    level_control::MoveModeEnum::Down if with_on_off => 0,
                    level_control::MoveModeEnum::Down => 1,
                };
                let duration = u16::from(start.abs_diff(target))
                    .saturating_mul(10)
                    .div_ceil(u16::from(rate));
                let properties = if with_on_off {
                    vec![Property::Brightness, Property::Power]
                } else {
                    vec![Property::Brightness]
                };
                let intent = self.service.begin_command_intent(&self.feature, properties);
                self.start_adjustment(
                    intent,
                    u32::from(duration.max(1)),
                    AdjustmentKind::Brightness {
                        start,
                        target,
                        with_on_off,
                    },
                )
            }
            level_control::CommandId::MoveToClosestFrequency => {
                Err(ErrorCode::InvalidCommand.into())
            }
        }
    }
}
