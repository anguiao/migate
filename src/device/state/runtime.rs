use crate::device::{
    self, AmbientTemperature, Function, FunctionKey, FunctionKind, FunctionState, LightSnapshot,
    MeasurementSnapshot, Power, StateSnapshot, TemperatureSourceState, command::CommandCoordinator,
};

pub(super) enum RuntimeValue {
    Outlet(Box<ControlRuntime>),
    Relay(Box<RelayRuntime>),
    Light(Box<LightRuntime>),
    Curtain(Box<CurtainRuntime>),
    Fan(Box<FanRuntime>),
    AirConditioner(Box<AirConditionerRuntime>),
    Heater(Box<HeaterRuntime>),
    Temperature(MeasurementRuntime),
    Humidity(MeasurementRuntime),
    CarbonDioxide(MeasurementRuntime),
    Illuminance(MeasurementRuntime),
    Contact(StateSnapshot),
    Motion(StateSnapshot),
    Occupancy(StateSnapshot),
}

pub(super) struct MeasurementRuntime {
    pub(super) snapshot: MeasurementSnapshot,
    pub(super) temperature_source: Option<TemperatureSourceState>,
}
impl MeasurementRuntime {
    fn new(temperature: bool) -> Self {
        Self {
            snapshot: MeasurementSnapshot {
                value: None,
                source_time: None,
                received_at: None,
            },
            temperature_source: temperature.then_some(TemperatureSourceState::Unavailable),
        }
    }
}

pub(super) struct ControlRuntime {
    pub(super) power: Power,
    commands: CommandCoordinator,
}
impl ControlRuntime {
    fn new() -> Self {
        Self {
            power: Power::Unknown,
            commands: CommandCoordinator::new(),
        }
    }
}

pub(super) struct RelayRuntime {
    control: ControlRuntime,
    pub(super) mode: Option<device::RelayMode>,
}

pub(super) struct LightRuntime {
    control: ControlRuntime,
    snapshot: LightSnapshot,
}

/// Planning may use a fresh on observation, while cached off remains conservative.
#[derive(Default)]
pub(in crate::device) struct PowerConfirmation(bool);
impl PowerConfirmation {
    pub(in crate::device) fn observe(&mut self, power: bool, fresh: bool) {
        if fresh || !power {
            self.0 = power;
        }
    }
    pub(in crate::device) fn needs_power_on(&self) -> bool {
        !self.0
    }
    pub(in crate::device) fn power_off_sent(&mut self) {
        self.0 = false;
    }
}

pub(in crate::device) struct FanRuntime {
    pub(in crate::device) snapshot: device::FanSnapshot,
    pub(in crate::device) commands: device::step_command::StepCoordinator<device::FanCommand>,
    pub(in crate::device) power_confirmation: PowerConfirmation,
}

pub(in crate::device) struct HeaterRuntime {
    pub(in crate::device) thermostat: device::thermostat::ThermostatObservation,
    pub(super) current_temperature: device::thermostat::TemperatureSourceRuntime,
    pub(in crate::device) commands: device::step_command::StepCoordinator<device::HeaterCommand>,
}
impl HeaterRuntime {
    fn snapshot(&self) -> device::HeaterSnapshot {
        let observation = &self.thermostat;
        let current = self.current_temperature;
        device::HeaterSnapshot {
            power: observation.power,
            temperature: observation.target_temperature,
            current_temperature: current.measurement,
            temperature_source: current.source,
        }
    }
}

pub(in crate::device) struct AirConditionerRuntime {
    pub(in crate::device) thermostat: device::thermostat::ThermostatObservation,
    pub(in crate::device) commands:
        device::step_command::StepCoordinator<device::AirConditionerCommand>,
    pub(in crate::device) power_confirmation: PowerConfirmation,
    pub(super) mode: device::model::PropertyObservation<device::AirConditionerMode>,
    pub(super) speed: device::model::PropertyObservation<device::AirConditionerFanSpeed>,
    pub(super) swing: device::model::PropertyObservation<bool>,
    pub(super) ambient_source: Option<AmbientSource>,
    pub(in crate::device) constraints: Option<device::air_conditioner::AirConditionerConstraints>,
    pub(in crate::device) dynamic_constraints: bool,
    pub(in crate::device) constraints_current: bool,
    pub(in crate::device) planning_mode: Option<device::AirConditionerMode>,
    pub(in crate::device) observed_mode: Option<device::AirConditionerMode>,
    pub(in crate::device) mode_revision: u64,
}
impl AirConditionerRuntime {
    fn snapshot(&self) -> device::AirConditionerSnapshot {
        let observation = &self.thermostat;
        device::AirConditionerSnapshot {
            power: observation.power,
            mode: self.mode,
            temperature: observation.target_temperature,
            speed: self.speed,
            swing: self.swing,
        }
    }

    pub(super) fn ambient_temperature(&self) -> AmbientTemperature {
        match &self.ambient_source {
            None => AmbientTemperature::Unbound,
            Some(source) => match &source.function {
                None => AmbientTemperature::SourceMissing,
                Some(function) => match function
                    .snapshot()
                    .temperature_source()
                    .expect("ambient source is a temperature function")
                {
                    TemperatureSourceState::Unavailable => AmbientTemperature::SourceUnavailable,
                    TemperatureSourceState::AwaitingReading => AmbientTemperature::AwaitingReading,
                    TemperatureSourceState::Available(value) => {
                        AmbientTemperature::Available(value)
                    }
                },
            },
        }
    }
}

#[derive(Clone)]
pub(super) struct AmbientSource {
    pub(super) key: FunctionKey,
    pub(super) function: Option<Function>,
}

pub(super) struct CurtainRuntime {
    pub(super) snapshot: device::CurtainSnapshot,
    pub(super) commands: device::curtain_command::CurtainCoordinator,
    pub(super) projection: device::curtain_projection::CurtainProjection,
}

impl RuntimeValue {
    fn new(kind: FunctionKind) -> Self {
        match kind {
            FunctionKind::Outlet => Self::Outlet(Box::new(ControlRuntime::new())),
            FunctionKind::Relay => Self::Relay(Box::new(RelayRuntime {
                control: ControlRuntime::new(),
                mode: None,
            })),
            FunctionKind::Light(profile) => Self::Light(Box::new(LightRuntime {
                control: ControlRuntime::new(),
                snapshot: LightSnapshot {
                    brightness_percent: None,
                    temperature_kelvin: None,
                    color: None,
                    mode: profile.inferred_color_mode(),
                },
            })),
            FunctionKind::Curtain => Self::Curtain(Box::new(CurtainRuntime {
                snapshot: device::CurtainSnapshot::default(),
                commands: device::curtain_command::CurtainCoordinator::default(),
                projection: device::curtain_projection::CurtainProjection::default(),
            })),
            FunctionKind::Fan { .. } => Self::Fan(Box::new(FanRuntime {
                snapshot: device::FanSnapshot::default(),
                commands: device::step_command::StepCoordinator::default(),
                power_confirmation: PowerConfirmation::default(),
            })),
            FunctionKind::AirConditioner { .. } => {
                Self::AirConditioner(Box::new(AirConditionerRuntime {
                    thermostat: device::thermostat::ThermostatObservation::default(),
                    commands: device::step_command::StepCoordinator::default(),
                    power_confirmation: PowerConfirmation::default(),
                    mode: device::model::PropertyObservation::default(),
                    speed: device::model::PropertyObservation::default(),
                    swing: device::model::PropertyObservation::default(),
                    ambient_source: None,
                    constraints: None,
                    dynamic_constraints: false,
                    constraints_current: false,
                    planning_mode: None,
                    observed_mode: None,
                    mode_revision: 0,
                }))
            }
            FunctionKind::Heater { .. } => Self::Heater(Box::new(HeaterRuntime {
                thermostat: device::thermostat::ThermostatObservation::default(),
                current_temperature: device::thermostat::TemperatureSourceRuntime::default(),
                commands: device::step_command::StepCoordinator::default(),
            })),
            FunctionKind::Temperature(_) => Self::Temperature(MeasurementRuntime::new(true)),
            FunctionKind::Humidity(_) => Self::Humidity(MeasurementRuntime::new(false)),
            FunctionKind::CarbonDioxide(_) => Self::CarbonDioxide(MeasurementRuntime::new(false)),
            FunctionKind::Illuminance(_) => Self::Illuminance(MeasurementRuntime::new(false)),
            FunctionKind::Contact => Self::Contact(Self::unknown_state()),
            FunctionKind::Motion => Self::Motion(Self::unknown_state()),
            FunctionKind::Occupancy => Self::Occupancy(Self::unknown_state()),
        }
    }

    fn unknown_state() -> StateSnapshot {
        StateSnapshot {
            active: None,
            input: None,
            source_time: None,
            received_at: None,
        }
    }

    pub(super) fn snapshot(&self) -> FunctionState {
        match self {
            Self::Outlet(control) => FunctionState::Outlet {
                power: control.power,
            },
            Self::Relay(relay) => FunctionState::Relay {
                power: relay.control.power,
                mode: relay.mode,
            },
            Self::Light(light) => FunctionState::Light {
                power: light.control.power,
                lighting: light.snapshot,
            },
            Self::Curtain(curtain) => FunctionState::Curtain {
                curtain: curtain.snapshot,
            },
            Self::Fan(fan) => FunctionState::Fan { fan: fan.snapshot },
            Self::AirConditioner(runtime) => FunctionState::AirConditioner {
                air_conditioner: runtime.snapshot(),
            },
            Self::Heater(runtime) => FunctionState::Heater {
                heater: runtime.snapshot(),
            },
            Self::Temperature(measurement) => FunctionState::Temperature {
                measurement: measurement.snapshot,
            },
            Self::Humidity(measurement) => FunctionState::Humidity {
                measurement: measurement.snapshot,
            },
            Self::CarbonDioxide(measurement) => FunctionState::CarbonDioxide {
                measurement: measurement.snapshot,
            },
            Self::Illuminance(measurement) => FunctionState::Illuminance {
                measurement: measurement.snapshot,
            },
            Self::Contact(state) => FunctionState::Contact {
                state: state.clone(),
            },
            Self::Motion(state) => FunctionState::Motion {
                state: state.clone(),
            },
            Self::Occupancy(state) => FunctionState::Occupancy {
                state: state.clone(),
            },
        }
    }
}

pub(in crate::device) struct RuntimeState {
    pub(super) value: RuntimeValue,
    pub(in crate::device) revision: u64,
}
impl RuntimeState {
    pub(super) fn new(kind: FunctionKind) -> Self {
        Self {
            value: RuntimeValue::new(kind),
            revision: 0,
        }
    }

    fn control(&self) -> Option<&ControlRuntime> {
        match &self.value {
            RuntimeValue::Outlet(control) => Some(control),
            RuntimeValue::Relay(relay) => Some(&relay.control),
            RuntimeValue::Light(light) => Some(&light.control),
            _ => None,
        }
    }

    fn control_mut(&mut self) -> Option<&mut ControlRuntime> {
        match &mut self.value {
            RuntimeValue::Outlet(control) => Some(control),
            RuntimeValue::Relay(relay) => Some(&mut relay.control),
            RuntimeValue::Light(light) => Some(&mut light.control),
            _ => None,
        }
    }

    fn light(&self) -> Option<&LightRuntime> {
        match &self.value {
            RuntimeValue::Light(light) => Some(light),
            _ => None,
        }
    }

    fn light_mut(&mut self) -> Option<&mut LightRuntime> {
        match &mut self.value {
            RuntimeValue::Light(light) => Some(light),
            _ => None,
        }
    }

    pub(in crate::device) fn commands(&self) -> Option<&CommandCoordinator> {
        self.control().map(|control| &control.commands)
    }

    pub(in crate::device) fn commands_mut(&mut self) -> Option<&mut CommandCoordinator> {
        self.control_mut().map(|control| &mut control.commands)
    }

    pub(in crate::device) fn power(&self) -> Option<Power> {
        self.control().map(|control| control.power)
    }

    pub(super) fn set_power(&mut self, power: Power) {
        if let Some(control) = self.control_mut() {
            control.power = power;
        }
    }

    pub(super) fn light_parts(&self) -> Option<(Power, LightSnapshot)> {
        self.light()
            .map(|light| (light.control.power, light.snapshot))
    }

    pub(super) fn set_light_parts(&mut self, power: Power, lighting: LightSnapshot) {
        let light = self.light_mut().expect("light has lighting state");
        light.control.power = power;
        light.snapshot = lighting;
    }

    pub(super) fn lighting_mut(&mut self) -> Option<&mut LightSnapshot> {
        self.light_mut().map(|light| &mut light.snapshot)
    }

    pub(super) fn measurement_mut(&mut self) -> Option<&mut MeasurementRuntime> {
        match &mut self.value {
            RuntimeValue::Temperature(measurement)
            | RuntimeValue::Humidity(measurement)
            | RuntimeValue::CarbonDioxide(measurement)
            | RuntimeValue::Illuminance(measurement) => Some(measurement),
            _ => None,
        }
    }

    pub(super) fn state_sensor_mut(&mut self) -> Option<&mut StateSnapshot> {
        match &mut self.value {
            RuntimeValue::Contact(state)
            | RuntimeValue::Motion(state)
            | RuntimeValue::Occupancy(state) => Some(state),
            _ => None,
        }
    }

    pub(super) fn temperature_source(&self) -> Option<TemperatureSourceState> {
        match &self.value {
            RuntimeValue::Temperature(measurement) => measurement.temperature_source,
            RuntimeValue::Heater(heater) => Some(heater.current_temperature.source),
            _ => None,
        }
    }

    pub(in crate::device) fn fan(&self) -> Option<&FanRuntime> {
        match &self.value {
            RuntimeValue::Fan(fan) => Some(fan),
            _ => None,
        }
    }

    pub(in crate::device) fn fan_mut(&mut self) -> Option<&mut FanRuntime> {
        match &mut self.value {
            RuntimeValue::Fan(fan) => Some(fan),
            _ => None,
        }
    }

    pub(in crate::device) fn heater(&self) -> Option<&HeaterRuntime> {
        match &self.value {
            RuntimeValue::Heater(heater) => Some(heater),
            _ => None,
        }
    }

    pub(in crate::device) fn heater_mut(&mut self) -> Option<&mut HeaterRuntime> {
        match &mut self.value {
            RuntimeValue::Heater(heater) => Some(heater),
            _ => None,
        }
    }

    pub(in crate::device) fn air_conditioner(&self) -> Option<&AirConditionerRuntime> {
        match &self.value {
            RuntimeValue::AirConditioner(runtime) => Some(runtime),
            _ => None,
        }
    }

    pub(in crate::device) fn air_conditioner_mut(&mut self) -> Option<&mut AirConditionerRuntime> {
        match &mut self.value {
            RuntimeValue::AirConditioner(runtime) => Some(runtime),
            _ => None,
        }
    }

    pub(in crate::device) fn curtain_commands(
        &self,
    ) -> Option<&device::curtain_command::CurtainCoordinator> {
        match &self.value {
            RuntimeValue::Curtain(curtain) => Some(&curtain.commands),
            _ => None,
        }
    }

    pub(in crate::device) fn curtain_commands_mut(
        &mut self,
    ) -> Option<&mut device::curtain_command::CurtainCoordinator> {
        match &mut self.value {
            RuntimeValue::Curtain(curtain) => Some(&mut curtain.commands),
            _ => None,
        }
    }

    pub(in crate::device) fn update_curtain_projection(
        &mut self,
        update: impl FnOnce(&mut device::curtain_projection::CurtainProjection),
    ) -> bool {
        let RuntimeValue::Curtain(curtain) = &mut self.value else {
            unreachable!("curtain function has curtain state")
        };
        let before = curtain.projection.target(curtain.snapshot);
        update(&mut curtain.projection);
        let changed = before != curtain.projection.target(curtain.snapshot);
        if changed {
            self.revision = self.revision.wrapping_add(1);
        }
        changed
    }
}
