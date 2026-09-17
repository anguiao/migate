use crate::device::{
    Capability, CommandOutcome, DeviceCommand, DeviceService, FeatureIdentity, FeatureRole,
    HvacMode, NumericRange, Property, PropertyState, PropertyValue,
};
use rs_matter::{
    dm::{Cluster, Dataver, ReadContext, WriteContext, clusters::decl::thermostat},
    error::{Error, ErrorCode},
    tlv::Nullable,
    with,
};
use std::{cell::RefCell, future::Future};

pub(super) fn capability_shape(
    capabilities: &[Capability],
    role: FeatureRole,
) -> (bool, bool, bool) {
    let modes = capabilities.iter().find_map(|capability| match capability {
        Capability::HvacModes(values) => Some(values.as_slice()),
        _ => None,
    });
    let heating = role == FeatureRole::BathHeaterClimate
        || modes.is_some_and(|values| values.contains(&HvacMode::Heat));
    let cooling = modes.is_some_and(|values| values.contains(&HvacMode::Cool));
    let local_temperature = capabilities
        .iter()
        .any(|capability| matches!(capability, Capability::Temperature(_)));
    (heating, cooling, local_temperature)
}

pub(super) const fn cluster(
    heating: bool,
    cooling: bool,
    local_temperature: bool,
) -> Cluster<'static> {
    // One physical target cannot provide the independent setpoints required by Auto mode.
    let mut features = thermostat::Feature::empty();
    if heating {
        features = features.union(thermostat::Feature::HEATING);
    }
    if cooling {
        features = features.union(thermostat::Feature::COOLING);
    }
    if !local_temperature {
        features = features.union(thermostat::Feature::LOCAL_TEMPERATURE_NOT_EXPOSED);
    }
    let cluster = thermostat::FULL_CLUSTER.with_features(features.bits());
    let cluster = if heating && cooling {
        cluster.with_attrs(with!(
            required;
            thermostat::AttributeId::AbsMinHeatSetpointLimit
                | thermostat::AttributeId::AbsMaxHeatSetpointLimit
                | thermostat::AttributeId::AbsMinCoolSetpointLimit
                | thermostat::AttributeId::AbsMaxCoolSetpointLimit
                | thermostat::AttributeId::OccupiedHeatingSetpoint
                | thermostat::AttributeId::OccupiedCoolingSetpoint
        ))
    } else if heating {
        cluster.with_attrs(with!(
            required;
            thermostat::AttributeId::AbsMinHeatSetpointLimit
                | thermostat::AttributeId::AbsMaxHeatSetpointLimit
                | thermostat::AttributeId::OccupiedHeatingSetpoint
        ))
    } else {
        cluster.with_attrs(with!(
            required;
            thermostat::AttributeId::AbsMinCoolSetpointLimit
                | thermostat::AttributeId::AbsMaxCoolSetpointLimit
                | thermostat::AttributeId::OccupiedCoolingSetpoint
        ))
    };
    cluster
        .with_cmds(with!(thermostat::CommandId::SetpointRaiseLower))
        .with_events(with!())
}

pub(super) struct ThermostatHandler {
    service: DeviceService,
    feature: FeatureIdentity,
    capabilities: RefCell<Vec<Capability>>,
    dataver: Dataver,
}

impl ThermostatHandler {
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

    fn target(&self) -> Option<f64> {
        match self.current(Property::TargetTemperature) {
            Some(PropertyValue::Temperature(value)) => Some(value),
            _ => None,
        }
    }

    fn target_range(&self) -> Result<NumericRange, Error> {
        self.capabilities
            .borrow()
            .iter()
            .find_map(|capability| match capability {
                Capability::TargetTemperature(range) => Some(*range),
                _ => None,
            })
            .ok_or_else(|| ErrorCode::Failure.into())
    }

    fn modes(&self) -> Vec<HvacMode> {
        self.capabilities
            .borrow()
            .iter()
            .find_map(|capability| match capability {
                Capability::HvacModes(values) => Some(values.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }

    fn active_mode(&self) -> Result<HvacMode, Error> {
        if self.power() == Some(false) {
            return Ok(HvacMode::Off);
        }
        if self.power() != Some(true) {
            return Err(ErrorCode::Failure.into());
        }
        if self.feature.role == FeatureRole::BathHeaterClimate {
            return Ok(HvacMode::Heat);
        }
        match self.current(Property::HvacMode) {
            Some(PropertyValue::HvacMode(mode)) if mode != HvacMode::Auto => Ok(mode),
            _ => Err(ErrorCode::Failure.into()),
        }
    }

    fn setpoint_mode(&self) -> Result<HvacMode, Error> {
        // Power does not erase the configured target or its last confirmed heat/cool mode.
        if self.feature.role == FeatureRole::BathHeaterClimate {
            return Ok(HvacMode::Heat);
        }
        match self.current(Property::HvacMode) {
            Some(PropertyValue::HvacMode(HvacMode::Heat)) => Ok(HvacMode::Heat),
            Some(PropertyValue::HvacMode(HvacMode::Cool)) => Ok(HvacMode::Cool),
            _ => Err(ErrorCode::InvalidState.into()),
        }
    }

    fn centi(value: f64) -> Result<i16, Error> {
        let centi = (value * 100.0).round();
        if !centi.is_finite() || centi < f64::from(i16::MIN) || centi > f64::from(i16::MAX) {
            return Err(ErrorCode::ConstraintError.into());
        }
        Ok(centi as i16)
    }

    fn temperature_from_centi(&self, value: i16) -> Result<f64, Error> {
        let value = f64::from(value) / 100.0;
        let range = self.target_range()?;
        if !range.accepts(value) {
            return Err(ErrorCode::ConstraintError.into());
        }
        Ok(value)
    }

    fn ensure_active_setpoint(&self, mode: HvacMode) -> Result<(), Error> {
        if self.setpoint_mode()? == mode {
            Ok(())
        } else {
            Err(ErrorCode::InvalidState.into())
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

    async fn setpoint(&self, mode: HvacMode, value: i16) -> Result<(), Error> {
        self.ensure_active_setpoint(mode)?;
        let value = self.temperature_from_centi(value)?;
        self.command_batch(vec![DeviceCommand::SetTargetTemperature(value)])
            .await
    }
}

impl thermostat::ClusterAsyncHandler for ThermostatHandler {
    const CLUSTER: Cluster<'static> = thermostat::FULL_CLUSTER;

    fn dataver(&self) -> u32 {
        self.dataver.get()
    }

    fn dataver_changed(&self) {
        self.dataver.changed();
    }

    async fn local_temperature(&self, _ctx: impl ReadContext) -> Result<Nullable<i16>, Error> {
        match self.current(Property::CurrentTemperature) {
            Some(PropertyValue::Temperature(value)) => Ok(Nullable::some(Self::centi(value)?)),
            _ => Ok(Nullable::none()),
        }
    }

    fn occupied_cooling_setpoint(
        &self,
        _ctx: impl ReadContext,
    ) -> impl Future<Output = Result<i16, Error>> {
        std::future::ready(
            self.ensure_active_setpoint(HvacMode::Cool)
                .and_then(|()| Self::centi(self.target().ok_or(ErrorCode::Failure)?)),
        )
    }

    fn occupied_heating_setpoint(
        &self,
        _ctx: impl ReadContext,
    ) -> impl Future<Output = Result<i16, Error>> {
        std::future::ready(
            self.ensure_active_setpoint(HvacMode::Heat)
                .and_then(|()| Self::centi(self.target().ok_or(ErrorCode::Failure)?)),
        )
    }

    fn abs_min_heat_setpoint_limit(
        &self,
        _ctx: impl ReadContext,
    ) -> impl Future<Output = Result<i16, Error>> {
        std::future::ready(
            self.target_range()
                .and_then(|range| Self::centi(range.minimum)),
        )
    }
    fn abs_max_heat_setpoint_limit(
        &self,
        _ctx: impl ReadContext,
    ) -> impl Future<Output = Result<i16, Error>> {
        std::future::ready(
            self.target_range()
                .and_then(|range| Self::centi(range.maximum)),
        )
    }
    fn abs_min_cool_setpoint_limit(
        &self,
        _ctx: impl ReadContext,
    ) -> impl Future<Output = Result<i16, Error>> {
        std::future::ready(
            self.target_range()
                .and_then(|range| Self::centi(range.minimum)),
        )
    }
    fn abs_max_cool_setpoint_limit(
        &self,
        _ctx: impl ReadContext,
    ) -> impl Future<Output = Result<i16, Error>> {
        std::future::ready(
            self.target_range()
                .and_then(|range| Self::centi(range.maximum)),
        )
    }
    async fn control_sequence_of_operation(
        &self,
        _ctx: impl ReadContext,
    ) -> Result<thermostat::ControlSequenceOfOperationEnum, Error> {
        let (heating, cooling, _) =
            capability_shape(&self.capabilities.borrow(), self.feature.role);
        Ok(match (heating, cooling) {
            (true, true) => thermostat::ControlSequenceOfOperationEnum::CoolingAndHeating,
            (true, false) => thermostat::ControlSequenceOfOperationEnum::HeatingOnly,
            (false, true) => thermostat::ControlSequenceOfOperationEnum::CoolingOnly,
            (false, false) => return Err(ErrorCode::Failure.into()),
        })
    }

    async fn system_mode(
        &self,
        _ctx: impl ReadContext,
    ) -> Result<thermostat::SystemModeEnum, Error> {
        Ok(match self.active_mode()? {
            HvacMode::Off => thermostat::SystemModeEnum::Off,
            HvacMode::Cool => thermostat::SystemModeEnum::Cool,
            HvacMode::Heat => thermostat::SystemModeEnum::Heat,
            HvacMode::Dry => thermostat::SystemModeEnum::Dry,
            HvacMode::FanOnly => thermostat::SystemModeEnum::FanOnly,
            HvacMode::Auto => return Err(ErrorCode::Failure.into()),
        })
    }

    async fn set_occupied_cooling_setpoint(
        &self,
        _ctx: impl WriteContext,
        value: i16,
    ) -> Result<(), Error> {
        self.setpoint(HvacMode::Cool, value).await
    }

    async fn set_occupied_heating_setpoint(
        &self,
        _ctx: impl WriteContext,
        value: i16,
    ) -> Result<(), Error> {
        self.setpoint(HvacMode::Heat, value).await
    }

    async fn set_control_sequence_of_operation(
        &self,
        _ctx: impl WriteContext,
        _value: thermostat::ControlSequenceOfOperationEnum,
    ) -> Result<(), Error> {
        Ok(())
    }

    async fn set_system_mode(
        &self,
        _ctx: impl WriteContext,
        value: thermostat::SystemModeEnum,
    ) -> Result<(), Error> {
        let mode = match value {
            thermostat::SystemModeEnum::Off => {
                return self
                    .command_batch(vec![DeviceCommand::SetPower(false)])
                    .await;
            }
            thermostat::SystemModeEnum::Cool => HvacMode::Cool,
            thermostat::SystemModeEnum::Heat => HvacMode::Heat,
            thermostat::SystemModeEnum::Dry => HvacMode::Dry,
            thermostat::SystemModeEnum::FanOnly => HvacMode::FanOnly,
            _ => return Err(ErrorCode::ConstraintError.into()),
        };
        if self.feature.role == FeatureRole::BathHeaterClimate {
            if mode != HvacMode::Heat {
                return Err(ErrorCode::ConstraintError.into());
            }
            return self
                .command_batch(vec![DeviceCommand::SetPower(true)])
                .await;
        }
        if !self.modes().contains(&mode) {
            return Err(ErrorCode::ConstraintError.into());
        }
        let mut commands = vec![DeviceCommand::SetHvacMode(mode)];
        if self.power() != Some(true) {
            commands.push(DeviceCommand::SetPower(true));
        }
        self.command_batch(commands).await
    }

    async fn handle_setpoint_raise_lower(
        &self,
        _ctx: impl rs_matter::dm::InvokeContext,
        request: thermostat::SetpointRaiseLowerRequest<'_>,
    ) -> Result<(), Error> {
        let (heating, cooling, _) =
            capability_shape(&self.capabilities.borrow(), self.feature.role);
        let mode = match request.mode()? {
            thermostat::SetpointRaiseLowerModeEnum::Heat if heating => HvacMode::Heat,
            thermostat::SetpointRaiseLowerModeEnum::Cool if cooling => HvacMode::Cool,
            thermostat::SetpointRaiseLowerModeEnum::Both => match self.setpoint_mode()? {
                HvacMode::Heat if heating => HvacMode::Heat,
                HvacMode::Cool if cooling => HvacMode::Cool,
                _ => return Err(ErrorCode::InvalidCommand.into()),
            },
            _ => return Err(ErrorCode::InvalidCommand.into()),
        };
        self.ensure_active_setpoint(mode)?;
        let current = self.target().ok_or(ErrorCode::Failure)?;
        let range = self.target_range()?;
        let target =
            (current + f64::from(request.amount()?) / 10.0).clamp(range.minimum, range.maximum);
        let centi = Self::centi(target)?;
        self.setpoint(mode, centi).await
    }

    async fn handle_set_weekly_schedule(
        &self,
        _ctx: impl rs_matter::dm::InvokeContext,
        _request: thermostat::SetWeeklyScheduleRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::CommandNotFound.into())
    }

    async fn handle_get_weekly_schedule<P: rs_matter::tlv::TLVBuilderParent>(
        &self,
        _ctx: impl rs_matter::dm::InvokeContext,
        _request: thermostat::GetWeeklyScheduleRequest<'_>,
        _response: thermostat::GetWeeklyScheduleResponseBuilder<P>,
    ) -> Result<P, Error> {
        Err(ErrorCode::CommandNotFound.into())
    }

    async fn handle_clear_weekly_schedule(
        &self,
        _ctx: impl rs_matter::dm::InvokeContext,
    ) -> Result<(), Error> {
        Err(ErrorCode::CommandNotFound.into())
    }

    async fn handle_set_active_schedule_request(
        &self,
        _ctx: impl rs_matter::dm::InvokeContext,
        _request: thermostat::SetActiveScheduleRequestRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::CommandNotFound.into())
    }

    async fn handle_set_active_preset_request(
        &self,
        _ctx: impl rs_matter::dm::InvokeContext,
        _request: thermostat::SetActivePresetRequestRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::CommandNotFound.into())
    }

    async fn handle_add_thermostat_suggestion<P: rs_matter::tlv::TLVBuilderParent>(
        &self,
        _ctx: impl rs_matter::dm::InvokeContext,
        _request: thermostat::AddThermostatSuggestionRequest<'_>,
        _response: thermostat::AddThermostatSuggestionResponseBuilder<P>,
    ) -> Result<P, Error> {
        Err(ErrorCode::CommandNotFound.into())
    }

    async fn handle_remove_thermostat_suggestion(
        &self,
        _ctx: impl rs_matter::dm::InvokeContext,
        _request: thermostat::RemoveThermostatSuggestionRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::CommandNotFound.into())
    }

    async fn handle_atomic_request<P: rs_matter::tlv::TLVBuilderParent>(
        &self,
        _ctx: impl rs_matter::dm::InvokeContext,
        _request: thermostat::AtomicRequestRequest<'_>,
        _response: thermostat::AtomicResponseBuilder<P>,
    ) -> Result<P, Error> {
        Err(ErrorCode::CommandNotFound.into())
    }
}
