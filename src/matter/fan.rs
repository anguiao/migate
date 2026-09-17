use crate::device::{
    Capability, CommandOutcome, DeviceCommand, DeviceService, FeatureIdentity, Property,
    PropertyState, PropertyValue, SwingMode,
};
use rs_matter::{
    dm::{Cluster, Dataver, ReadContext, WriteContext, clusters::decl::fan_control},
    error::{Error, ErrorCode},
    tlv::Nullable,
    with,
};
use std::cell::RefCell;

fn valid_fan_levels(capabilities: &[Capability]) -> Option<Vec<u16>> {
    let values = capabilities
        .iter()
        .find_map(|capability| match capability {
            Capability::FanSpeeds(values) => Some(values),
            _ => None,
        })?;
    let non_auto = values.iter().filter(|value| **value != 0).count();
    let unique = values
        .iter()
        .enumerate()
        .all(|(index, value)| !values[..index].contains(value));
    (non_auto > 0 && non_auto <= 100 && unique).then(|| values.clone())
}

pub(super) fn capability_shape(capabilities: &[Capability]) -> (bool, bool, bool) {
    let levels = valid_fan_levels(capabilities);
    let multi_speed = levels.is_some();
    let auto = levels.as_ref().is_some_and(|values| values.contains(&0));
    let rocking = capabilities.iter().any(|capability| {
        matches!(capability, Capability::SwingModes(values) if values.iter().any(|mode| *mode != SwingMode::Off))
    });
    (multi_speed, auto, rocking)
}

pub(super) const fn cluster(multi_speed: bool, auto: bool, rocking: bool) -> Cluster<'static> {
    let mut features = fan_control::Feature::empty();
    if multi_speed {
        features = features.union(fan_control::Feature::MULTI_SPEED);
    }
    if auto {
        features = features.union(fan_control::Feature::AUTO);
    }
    if rocking {
        features = features.union(fan_control::Feature::ROCKING);
    }
    let cluster = fan_control::FULL_CLUSTER.with_features(features.bits());
    let cluster = if multi_speed && rocking {
        cluster.with_attrs(with!(
            required;
            fan_control::AttributeId::SpeedMax
                | fan_control::AttributeId::SpeedSetting
                | fan_control::AttributeId::SpeedCurrent
                | fan_control::AttributeId::RockSupport
                | fan_control::AttributeId::RockSetting
        ))
    } else if multi_speed {
        cluster.with_attrs(with!(
            required;
            fan_control::AttributeId::SpeedMax
                | fan_control::AttributeId::SpeedSetting
                | fan_control::AttributeId::SpeedCurrent
        ))
    } else if rocking {
        cluster.with_attrs(with!(
            required;
            fan_control::AttributeId::RockSupport | fan_control::AttributeId::RockSetting
        ))
    } else {
        cluster.with_attrs(with!(required))
    };
    cluster.with_cmds(with!()).with_events(with!())
}

pub(super) struct FanHandler {
    service: DeviceService,
    feature: FeatureIdentity,
    capabilities: RefCell<Vec<Capability>>,
    dataver: Dataver,
}

impl FanHandler {
    pub(super) fn new(
        service: DeviceService,
        feature: FeatureIdentity,
        capabilities: Vec<Capability>,
        seed: u32,
    ) -> Self {
        Self {
            service,
            feature,
            capabilities: RefCell::new(capabilities),
            dataver: Dataver::new(seed),
        }
    }

    pub(super) fn set_capabilities(&self, capabilities: Vec<Capability>) {
        *self.capabilities.borrow_mut() = capabilities;
    }

    pub(super) fn dataver(&self) -> &Dataver {
        &self.dataver
    }

    fn current(&self, property: Property) -> Option<PropertyValue> {
        self.service
            .snapshot(&self.feature)
            .and_then(|snapshot| snapshot.property(property).cloned())
            .and_then(|state| match state {
                PropertyState::Current { value, .. } => Some(value),
                PropertyState::LastKnown { .. } | PropertyState::Unknown { .. } => None,
            })
    }

    fn power(&self) -> Option<bool> {
        match self.current(Property::Power) {
            Some(PropertyValue::Power(value)) => Some(value),
            _ => None,
        }
    }

    fn current_speed(&self) -> Option<u16> {
        match self.current(Property::FanSpeed) {
            Some(PropertyValue::FanSpeed(value)) => Some(value),
            _ => None,
        }
    }

    fn speeds(&self) -> Vec<u16> {
        valid_fan_levels(&self.capabilities.borrow())
            .unwrap_or_default()
            .into_iter()
            .filter(|value| *value != 0)
            .collect()
    }

    fn has_auto(&self) -> bool {
        valid_fan_levels(&self.capabilities.borrow()).is_some_and(|values| values.contains(&0))
    }

    fn rock_support_value(&self) -> fan_control::RockBitmap {
        let mut support = fan_control::RockBitmap::empty();
        if let Some(modes) =
            self.capabilities
                .borrow()
                .iter()
                .find_map(|capability| match capability {
                    Capability::SwingModes(values) => Some(values.clone()),
                    _ => None,
                })
        {
            if modes
                .iter()
                .any(|mode| matches!(mode, SwingMode::Horizontal | SwingMode::Both))
            {
                support |= fan_control::RockBitmap::ROCK_LEFT_RIGHT;
            }
            if modes
                .iter()
                .any(|mode| matches!(mode, SwingMode::Vertical | SwingMode::Both))
            {
                support |= fan_control::RockBitmap::ROCK_UP_DOWN;
            }
        }
        support
    }

    fn speed_index(&self) -> Result<Option<u8>, Error> {
        if self.power() == Some(false) {
            return Ok(Some(0));
        }
        if self.power() != Some(true) {
            return Err(ErrorCode::Failure.into());
        }
        let speeds = self.speeds();
        if speeds.is_empty() {
            return Ok(Some(1));
        }
        let speed = self.current_speed().ok_or(ErrorCode::Failure)?;
        if speed == 0 && self.has_auto() {
            return Ok(None);
        }
        speeds
            .iter()
            .position(|candidate| *candidate == speed)
            .map(|index| Some((index + 1) as u8))
            .ok_or_else(|| ErrorCode::Failure.into())
    }

    fn speed_max_value(&self) -> Result<u8, Error> {
        let count = self.speeds().len();
        if count == 0 {
            return Err(ErrorCode::AttributeNotFound.into());
        }
        Ok(count as u8)
    }

    fn mode_for_index(&self, index: u8) -> Result<fan_control::FanModeEnum, Error> {
        let speeds = self.speeds();
        let maximum = if speeds.is_empty() {
            1
        } else {
            self.speed_max_value()?
        };
        Ok(if maximum == 1 || index == maximum {
            fan_control::FanModeEnum::High
        } else if index == 1 {
            fan_control::FanModeEnum::Low
        } else {
            fan_control::FanModeEnum::Medium
        })
    }

    fn mode_sequence_value(&self) -> fan_control::FanModeSequenceEnum {
        let count = self.speeds().len();
        match (count, self.has_auto()) {
            (0 | 1, false) => fan_control::FanModeSequenceEnum::OffHigh,
            (2, false) => fan_control::FanModeSequenceEnum::OffLowHigh,
            (_, false) => fan_control::FanModeSequenceEnum::OffLowMedHigh,
            (0 | 1, true) => fan_control::FanModeSequenceEnum::OffHighAuto,
            (2, true) => fan_control::FanModeSequenceEnum::OffLowHighAuto,
            (_, true) => fan_control::FanModeSequenceEnum::OffLowMedHighAuto,
        }
    }

    fn commands_for_index(&self, index: u8) -> Result<Vec<DeviceCommand>, Error> {
        if index == 0 {
            return Ok(vec![DeviceCommand::SetPower(false)]);
        }
        let speeds = self.speeds();
        if speeds.is_empty() {
            if index != 1 {
                return Err(ErrorCode::ConstraintError.into());
            }
            return Ok(vec![DeviceCommand::SetPower(true)]);
        }
        let speed = speeds
            .get(usize::from(index - 1))
            .copied()
            .ok_or(ErrorCode::ConstraintError)?;
        let mut commands = vec![DeviceCommand::SetFanSpeed(speed)];
        if self.power() != Some(true) {
            commands.push(DeviceCommand::SetPower(true));
        }
        Ok(commands)
    }

    fn commands_for_auto(&self) -> Result<Vec<DeviceCommand>, Error> {
        if !self.has_auto() {
            return Err(ErrorCode::ConstraintError.into());
        }
        let mut commands = vec![DeviceCommand::SetFanSpeed(0)];
        if self.power() != Some(true) {
            commands.push(DeviceCommand::SetPower(true));
        }
        Ok(commands)
    }

    fn commands_for_mode(
        &self,
        mode: fan_control::FanModeEnum,
    ) -> Result<Vec<DeviceCommand>, Error> {
        let maximum = self.speeds().len().max(1) as u8;
        match mode {
            fan_control::FanModeEnum::Off => self.commands_for_index(0),
            fan_control::FanModeEnum::Low if maximum >= 2 => self.commands_for_index(1),
            fan_control::FanModeEnum::Medium if maximum >= 3 => {
                self.commands_for_index(maximum.div_ceil(2))
            }
            fan_control::FanModeEnum::High | fan_control::FanModeEnum::On => {
                self.commands_for_index(maximum)
            }
            fan_control::FanModeEnum::Auto => self.commands_for_auto(),
            fan_control::FanModeEnum::Smart => {
                if self.has_auto() {
                    self.commands_for_auto()
                } else {
                    self.commands_for_index(maximum)
                }
            }
            _ => Err(ErrorCode::ConstraintError.into()),
        }
    }

    async fn command_batch(&self, commands: Vec<DeviceCommand>) -> Result<(), Error> {
        match self.service.command_batch(&self.feature, commands).await {
            CommandOutcome::Accepted => Ok(()),
            CommandOutcome::Unsupported | CommandOutcome::Rejected(_) => {
                Err(ErrorCode::ConstraintError.into())
            }
            _ => Err(ErrorCode::Failure.into()),
        }
    }
}

impl fan_control::ClusterAsyncHandler for FanHandler {
    const CLUSTER: Cluster<'static> = fan_control::FULL_CLUSTER;

    fn dataver(&self) -> u32 {
        self.dataver.get()
    }

    fn dataver_changed(&self) {
        self.dataver.changed();
    }

    async fn fan_mode(&self, _ctx: impl ReadContext) -> Result<fan_control::FanModeEnum, Error> {
        match self.speed_index()? {
            Some(0) => Ok(fan_control::FanModeEnum::Off),
            Some(index) => self.mode_for_index(index),
            None => Ok(fan_control::FanModeEnum::Auto),
        }
    }

    async fn fan_mode_sequence(
        &self,
        _ctx: impl ReadContext,
    ) -> Result<fan_control::FanModeSequenceEnum, Error> {
        Ok(self.mode_sequence_value())
    }

    async fn percent_setting(&self, _ctx: impl ReadContext) -> Result<Nullable<u8>, Error> {
        self.speed_index()?.map_or_else(
            || Ok(Nullable::none()),
            |index| {
                if index == 0 {
                    Ok(Nullable::some(0))
                } else {
                    let speeds = self.speeds();
                    let maximum = if speeds.is_empty() {
                        1
                    } else {
                        self.speed_max_value()?
                    };
                    Ok(Nullable::some(
                        (u16::from(index) * 100 / u16::from(maximum)) as u8,
                    ))
                }
            },
        )
    }

    async fn percent_current(&self, _ctx: impl ReadContext) -> Result<u8, Error> {
        self.percent_setting(_ctx)
            .await?
            .into_option()
            .ok_or_else(|| ErrorCode::Failure.into())
    }

    async fn speed_max(&self, _ctx: impl ReadContext) -> Result<u8, Error> {
        self.speed_max_value()
    }

    async fn speed_setting(&self, _ctx: impl ReadContext) -> Result<Nullable<u8>, Error> {
        Ok(self
            .speed_index()?
            .map_or_else(Nullable::none, Nullable::some))
    }

    async fn speed_current(&self, _ctx: impl ReadContext) -> Result<u8, Error> {
        self.speed_index()?.ok_or_else(|| ErrorCode::Failure.into())
    }

    async fn rock_support(&self, _ctx: impl ReadContext) -> Result<fan_control::RockBitmap, Error> {
        Ok(self.rock_support_value())
    }

    async fn rock_setting(&self, _ctx: impl ReadContext) -> Result<fan_control::RockBitmap, Error> {
        match self.current(Property::Oscillation) {
            Some(PropertyValue::Oscillation(false)) => Ok(fan_control::RockBitmap::empty()),
            Some(PropertyValue::Oscillation(true)) => Ok(self.rock_support_value()),
            _ => Err(ErrorCode::Failure.into()),
        }
    }

    async fn set_fan_mode(
        &self,
        _ctx: impl WriteContext,
        value: fan_control::FanModeEnum,
    ) -> Result<(), Error> {
        self.command_batch(self.commands_for_mode(value)?).await
    }

    async fn set_percent_setting(
        &self,
        _ctx: impl WriteContext,
        value: Nullable<u8>,
    ) -> Result<(), Error> {
        let Some(percent) = value.into_option() else {
            return Ok(());
        };
        if percent > 100 {
            return Err(ErrorCode::ConstraintError.into());
        }
        if percent == 0 {
            return self.command_batch(self.commands_for_index(0)?).await;
        }
        let maximum = self.speeds().len().max(1) as u16;
        let index = (maximum * u16::from(percent)).div_ceil(100) as u8;
        self.command_batch(self.commands_for_index(index)?).await
    }

    async fn set_speed_setting(
        &self,
        _ctx: impl WriteContext,
        value: Nullable<u8>,
    ) -> Result<(), Error> {
        match value.into_option() {
            Some(index) => self.command_batch(self.commands_for_index(index)?).await,
            None => Ok(()),
        }
    }

    async fn set_rock_setting(
        &self,
        _ctx: impl WriteContext,
        value: fan_control::RockBitmap,
    ) -> Result<(), Error> {
        let support = self.rock_support_value();
        if !(value & !support).is_empty() {
            return Err(ErrorCode::ConstraintError.into());
        }
        self.command_batch(vec![DeviceCommand::SetOscillation(!value.is_empty())])
            .await
    }

    async fn handle_step(
        &self,
        _ctx: impl rs_matter::dm::InvokeContext,
        _request: fan_control::StepRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::CommandNotFound.into())
    }
}
