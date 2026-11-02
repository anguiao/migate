use super::*;

impl WireFunction {
    pub(super) fn from_parsed(function: &ParsedFunction) -> Self {
        if let Some(ac) = function.air_conditioner() {
            let grid = ac.temperature().grid();
            return Self::AirConditioner {
                id: function.id().into(),
                label: function.label().into(),
                power: ac.power().into(),
                mode: WireAirConditionerEnum {
                    property: ac.mode().property().into(),
                    format: ac.mode().format(),
                    codes: ac.mode().codes(),
                },
                temperature: WireAirConditionerTemperature {
                    property: ac.temperature().property().into(),
                    format: ac.temperature().format(),
                    min: grid.min(),
                    max: grid.max(),
                    step: grid.step(),
                },
                speed: WireAirConditionerEnum {
                    property: ac.speed().property().into(),
                    format: ac.speed().format(),
                    codes: ac.speed().codes(),
                },
                swing: ac.swing().map(Into::into),
                mcn04: ac.mcn04().map(|patch| WireMcn04 {
                    state: patch.state.into(),
                    range: patch.range.into(),
                }),
            };
        }
        if let Some(fan) = function.fan() {
            return Self::Fan {
                id: function.id().to_owned(),
                label: function.label().to_owned(),
                power: fan.power().into(),
                mode: fan.mode().map(|mode| WireFanMode {
                    property: mode.property().into(),
                    format: mode.format(),
                    straight: mode.straight(),
                    values: mode.values().to_vec(),
                }),
                speed: WireFanSpeed {
                    property: fan.speed().property().into(),
                    format: fan.speed().format(),
                    encoding: fan.speed().encoding().clone(),
                },
                swing: fan.swing().map(Into::into),
            };
        }
        if let Some(curtain) = function.curtain() {
            let motor = curtain.motor();
            let status = curtain.status();
            return Self::Curtain {
                id: function.id().to_owned(),
                label: function.label().to_owned(),
                motor: WireCurtainMotor {
                    property: motor.property().into(),
                    format: motor.format(),
                    pause: motor.pause(),
                },
                status: WireCurtainStatus {
                    property: status.property().into(),
                    format: status.format(),
                    opening: status.opening(),
                    closing: status.closing(),
                    stopped: status.stopped(),
                },
                current: WireNumeric::from_curtain_position(curtain.current()),
                target: WireNumeric::from_curtain_position(curtain.target()),
            };
        }
        if let Some(state) = function.state() {
            let id = function.id().to_owned();
            let label = function.label().to_owned();
            let state = WireState::from_binding(state);
            return match function.kind() {
                crate::device::FunctionKind::Contact => Self::Contact { id, label, state },
                crate::device::FunctionKind::Occupancy => Self::Occupancy { id, label, state },
                crate::device::FunctionKind::Motion => {
                    let motion = function.motion().unwrap();
                    let binding = motion.event();
                    Self::Motion {
                        id,
                        label,
                        state,
                        event: WireEvent {
                            siid: binding.siid,
                            eiid: binding.eiid,
                        },
                        arguments: motion
                            .arguments()
                            .iter()
                            .map(|argument| WireEventArgument {
                                property: argument.property.into(),
                                format: argument.format.clone(),
                                unit: argument.unit.clone(),
                            })
                            .collect(),
                    }
                }
                _ => unreachable!("state functions have state kinds"),
            };
        }
        if let Some(measurement) = function.measurement() {
            let wire = WireMeasurement::from_binding(measurement);
            return match function.kind() {
                crate::device::FunctionKind::Temperature(_) => Self::Temperature {
                    id: function.id().to_owned(),
                    label: function.label().to_owned(),
                    measurement: wire,
                },
                crate::device::FunctionKind::Humidity(_) => Self::Humidity {
                    id: function.id().to_owned(),
                    label: function.label().to_owned(),
                    measurement: wire,
                },
                crate::device::FunctionKind::CarbonDioxide(_) => Self::CarbonDioxide {
                    id: function.id().to_owned(),
                    label: function.label().to_owned(),
                    measurement: wire,
                },
                crate::device::FunctionKind::Illuminance(_) => Self::Illuminance {
                    id: function.id().to_owned(),
                    label: function.label().to_owned(),
                    measurement: wire,
                },
                _ => unreachable!("measurement functions have measurement kinds"),
            };
        }
        let Some(lighting) = function.lighting() else {
            return Self::Outlet {
                id: function.id().to_owned(),
                label: function.label().to_owned(),
                power: function
                    .power()
                    .expect("outlet functions have power")
                    .into(),
            };
        };
        let lighting = match lighting {
            LightBindings::OnOff => WireLighting::OnOff,
            LightBindings::Dimmable { brightness } => WireLighting::Dimmable {
                brightness: WireNumeric::from_brightness(brightness),
            },
            LightBindings::ColorTemperature {
                brightness,
                temperature,
            } => WireLighting::ColorTemperature {
                brightness: WireNumeric::from_brightness(brightness),
                temperature: WireNumeric::from_temperature(temperature),
            },
            LightBindings::ExtendedColor {
                brightness,
                temperature,
                color,
                mode,
            } => WireLighting::ExtendedColor {
                brightness: WireNumeric::from_brightness(brightness),
                temperature: WireNumeric::from_temperature(temperature),
                color: WireFormat {
                    property: color.property().into(),
                    format: color.format(),
                },
                mode: mode.as_ref().map(|mode| WireMode {
                    property: mode.property().into(),
                    format: mode.format(),
                    temperature_value: mode.temperature_value(),
                    rgb_value: mode.rgb_value(),
                }),
            },
        };
        Self::Light {
            id: function.id().to_owned(),
            label: function.label().to_owned(),
            power: function.power().expect("light functions have power").into(),
            lighting,
        }
    }

    pub(super) fn into_parsed(self) -> Result<ParsedFunction, &'static str> {
        match self {
            Self::AirConditioner {
                id,
                label,
                power,
                mode,
                temperature,
                speed,
                swing,
                mcn04,
            } => {
                let grid = TemperatureGrid::new(temperature.min, temperature.max, temperature.step)
                    .map_err(|_| "Stored air-conditioner binding is invalid")?;
                let mode =
                    AirConditionerModeBinding::new(mode.property.into(), mode.format, mode.codes)
                        .map_err(|_| "Stored air-conditioner binding is invalid")?;
                let temperature = AirConditionerTemperatureBinding::new(
                    temperature.property.into(),
                    temperature.format,
                    grid,
                )
                .map_err(|_| "Stored air-conditioner binding is invalid")?;
                let speed = AirConditionerSpeedBinding::new(
                    speed.property.into(),
                    speed.format,
                    speed.codes,
                )
                .map_err(|_| "Stored air-conditioner binding is invalid")?;
                let mut bindings = AirConditionerBindings::new(
                    power.into(),
                    mode,
                    temperature,
                    speed,
                    swing.map(Into::into),
                )
                .map_err(|_| "Stored air-conditioner binding is invalid")?;
                if let Some(patch) = mcn04 {
                    bindings = bindings
                        .with_mcn04(patch.state.into(), patch.range.into())
                        .map_err(|_| "Stored air-conditioner binding is invalid")?;
                }
                if bindings.function_id() != id {
                    return Err("Stored air-conditioner identity is invalid");
                }
                Ok(ParsedFunction::air_conditioner_control(label, bindings))
            }
            Self::Fan {
                id,
                label,
                power,
                mode,
                speed,
                swing,
            } => {
                let mode = mode
                    .map(|mode| {
                        FanModeBinding::new(
                            mode.property.into(),
                            mode.format,
                            mode.straight,
                            mode.values,
                        )
                    })
                    .transpose()
                    .map_err(|_| "Stored fan binding is invalid")?;
                let speed =
                    FanSpeedBinding::new(speed.property.into(), speed.format, speed.encoding)
                        .map_err(|_| "Stored fan binding is invalid")?;
                Ok(ParsedFunction::fan_control(
                    id,
                    label,
                    FanBindings::new(power.into(), mode, speed, swing.map(Into::into)),
                ))
            }
            Self::Curtain {
                id,
                label,
                motor,
                status,
                current,
                target,
            } => {
                let motor =
                    CurtainMotorBinding::new(motor.property.into(), motor.format, motor.pause)
                        .map_err(|_| "Stored curtain binding is invalid")?;
                let status = CurtainStatusBinding::new(
                    status.property.into(),
                    status.format,
                    status.opening,
                    status.closing,
                    status.stopped,
                )
                .map_err(|_| "Stored curtain binding is invalid")?;
                Ok(ParsedFunction::curtain_control(
                    id,
                    label,
                    CurtainBindings::new(
                        motor,
                        status,
                        current.into_curtain_position()?,
                        target.into_curtain_position()?,
                    ),
                ))
            }
            Self::Contact { id, label, state } => {
                Ok(ParsedFunction::contact(id, label, state.into_binding()?))
            }
            Self::Occupancy { id, label, state } => {
                Ok(ParsedFunction::occupancy(id, label, state.into_binding()?))
            }
            Self::Motion {
                id,
                label,
                state,
                event,
                arguments,
            } => {
                let binding = MotionBinding::new(
                    state.into_binding()?,
                    EventBinding {
                        siid: event.siid,
                        eiid: event.eiid,
                    },
                    arguments
                        .into_iter()
                        .map(|argument| EventArgument {
                            property: argument.property.into(),
                            format: argument.format,
                            unit: argument.unit,
                        })
                        .collect(),
                )
                .map_err(|_| "Stored motion binding is invalid")?;
                Ok(ParsedFunction::motion_sensor(id, label, binding))
            }
            Self::Outlet { id, label, power } => {
                Ok(ParsedFunction::outlet(id, label, power.into()))
            }
            Self::Temperature {
                id,
                label,
                measurement,
            } => Ok(ParsedFunction::temperature(
                id,
                label,
                measurement.into_binding()?,
            )),
            Self::Humidity {
                id,
                label,
                measurement,
            } => Ok(ParsedFunction::humidity(
                id,
                label,
                measurement.into_binding()?,
            )),
            Self::CarbonDioxide {
                id,
                label,
                measurement,
            } => Ok(ParsedFunction::co2(id, label, measurement.into_binding()?)),
            Self::Illuminance {
                id,
                label,
                measurement,
            } => Ok(ParsedFunction::illuminance(
                id,
                label,
                measurement.into_binding()?,
            )),
            Self::Light {
                id,
                label,
                power,
                lighting,
            } => {
                let lighting = match lighting {
                    WireLighting::OnOff => LightBindings::OnOff,
                    WireLighting::Dimmable { brightness } => LightBindings::Dimmable {
                        brightness: brightness.into_brightness()?,
                    },
                    WireLighting::ColorTemperature {
                        brightness,
                        temperature,
                    } => LightBindings::ColorTemperature {
                        brightness: brightness.into_brightness()?,
                        temperature: temperature.into_temperature()?,
                    },
                    WireLighting::ExtendedColor {
                        brightness,
                        temperature,
                        color,
                        mode,
                    } => LightBindings::ExtendedColor {
                        brightness: brightness.into_brightness()?,
                        temperature: temperature.into_temperature()?,
                        color: ColorBinding::new(color.property.into(), color.format)
                            .map_err(|_| "Stored light binding is invalid")?,
                        mode: mode
                            .map(|mode| {
                                ModeBinding::new(
                                    mode.property.into(),
                                    mode.format,
                                    mode.temperature_value,
                                    mode.rgb_value,
                                )
                                .map_err(|_| "Stored light binding is invalid")
                            })
                            .transpose()?,
                    },
                };
                Ok(ParsedFunction::light(id, label, power.into(), lighting))
            }
        }
    }
}
