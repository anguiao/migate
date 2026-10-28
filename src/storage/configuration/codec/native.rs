//! Native capability records and their checked decoding boundary.

use crate::xiaomi::capability::NativeDescription;
use crate::{
    device::{DeviceKey, MeasurementRange, NumericRange},
    xiaomi::capability::{
        BatteryBinding, BrightnessBinding, ColorBinding, CurtainBindings, CurtainMotorBinding,
        CurtainPositionBinding, CurtainStatusBinding, EventArgument, EventBinding, LightBindings,
        MeasurementBinding, MeasurementUnit, ModeBinding, MotionBinding, NumericFormat,
        ParsedDevice, ParsedFunction, PropertyBinding, StateBinding, StateMapping, StateValue,
        TemperatureBinding,
    },
};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct WireNative {
    model: String,
    instance_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    refresh_trigger: Option<WireBinding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    battery: Option<WireMeasurement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    battery_issue: Option<String>,
    functions: Vec<WireFunction>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum WireFunction {
    Outlet {
        id: String,
        label: String,
        power: WireBinding,
    },
    Light {
        id: String,
        label: String,
        power: WireBinding,
        lighting: WireLighting,
    },
    Curtain {
        id: String,
        label: String,
        motor: WireCurtainMotor,
        status: WireCurtainStatus,
        current: WireNumeric,
        target: WireNumeric,
    },
    Temperature {
        id: String,
        label: String,
        measurement: WireMeasurement,
    },
    Humidity {
        id: String,
        label: String,
        measurement: WireMeasurement,
    },
    CarbonDioxide {
        id: String,
        label: String,
        measurement: WireMeasurement,
    },
    Illuminance {
        id: String,
        label: String,
        measurement: WireMeasurement,
    },
    Contact {
        id: String,
        label: String,
        state: WireState,
    },
    Motion {
        id: String,
        label: String,
        state: WireState,
        event: WireEvent,
        arguments: Vec<WireEventArgument>,
    },
    Occupancy {
        id: String,
        label: String,
        state: WireState,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireCurtainMotor {
    property: WireBinding,
    format: NumericFormat,
    pause: u32,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireCurtainStatus {
    property: WireBinding,
    format: NumericFormat,
    opening: u32,
    closing: u32,
    stopped: u32,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireState {
    property: WireBinding,
    mapping: WireStateMapping,
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum WireStateMapping {
    ContactBoolean,
    Enumerated {
        format: NumericFormat,
        values: Vec<WireStateValue>,
    },
    NoMotionDuration {
        format: NumericFormat,
    },
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireStateValue {
    native: i64,
    #[serde(deserialize_with = "Option::deserialize")]
    active: Option<bool>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireEvent {
    siid: u32,
    eiid: u32,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireEventArgument {
    property: WireBinding,
    format: String,
    unit: Option<String>,
}
impl WireState {
    fn from_binding(state: &StateBinding) -> Self {
        let mapping = match state.mapping() {
            StateMapping::ContactBoolean => WireStateMapping::ContactBoolean,
            StateMapping::NoMotionDuration { format } => {
                WireStateMapping::NoMotionDuration { format: *format }
            }
            StateMapping::Enumerated { format, values } => WireStateMapping::Enumerated {
                format: *format,
                values: values
                    .iter()
                    .map(|value| WireStateValue {
                        native: value.native,
                        active: value.active,
                    })
                    .collect(),
            },
        };
        Self {
            property: state.property().into(),
            mapping,
        }
    }
    fn into_binding(self) -> Result<StateBinding, &'static str> {
        let mapping = match self.mapping {
            WireStateMapping::ContactBoolean => StateMapping::ContactBoolean,
            WireStateMapping::NoMotionDuration { format } => {
                StateMapping::NoMotionDuration { format }
            }
            WireStateMapping::Enumerated { format, values } => StateMapping::Enumerated {
                format,
                values: values
                    .into_iter()
                    .map(|value| StateValue {
                        native: value.native,
                        active: value.active,
                    })
                    .collect(),
            },
        };
        StateBinding::new(self.property.into(), mapping)
            .map_err(|_| "Stored state binding is invalid")
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireMeasurement {
    property: WireBinding,
    format: NumericFormat,
    unit: MeasurementUnit,
    min: f64,
    max: f64,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "profile", rename_all = "snake_case", deny_unknown_fields)]
enum WireLighting {
    OnOff,
    Dimmable {
        brightness: WireNumeric,
    },
    ColorTemperature {
        brightness: WireNumeric,
        temperature: WireNumeric,
    },
    ExtendedColor {
        brightness: WireNumeric,
        temperature: WireNumeric,
        color: WireFormat,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mode: Option<WireMode>,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireNumeric {
    property: WireBinding,
    format: NumericFormat,
    min: u32,
    max: u32,
    step: u32,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireFormat {
    property: WireBinding,
    format: NumericFormat,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireMode {
    property: WireBinding,
    format: NumericFormat,
    temperature_value: u32,
    rgb_value: u32,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireBinding {
    siid: u32,
    piid: u32,
}
impl From<PropertyBinding> for WireBinding {
    fn from(binding: PropertyBinding) -> Self {
        Self {
            siid: binding.siid,
            piid: binding.piid,
        }
    }
}
impl From<WireBinding> for PropertyBinding {
    fn from(binding: WireBinding) -> Self {
        Self {
            siid: binding.siid,
            piid: binding.piid,
        }
    }
}

impl WireNative {
    pub(super) fn from_native(native: &NativeDescription) -> Self {
        Self {
            model: native.model().to_owned(),
            instance_type: native.instance_type().to_owned(),
            refresh_trigger: native.device().refresh_trigger().map(WireBinding::from),
            battery: native.device().battery().map(WireMeasurement::from_battery),
            battery_issue: native.device().battery_issue().map(str::to_owned),
            functions: native
                .device()
                .functions()
                .iter()
                .map(WireFunction::from_parsed)
                .collect(),
        }
    }

    pub(super) fn into_native(self, key: &DeviceKey) -> Result<NativeDescription, &'static str> {
        let refresh_trigger = self.refresh_trigger.map(PropertyBinding::from);
        let battery = self
            .battery
            .map(WireMeasurement::into_battery)
            .transpose()?;
        let functions = self
            .functions
            .into_iter()
            .map(WireFunction::into_parsed)
            .collect::<Result<Vec<_>, _>>()?;
        let battery_issue = self.battery_issue.or_else(|| {
            (battery.is_none()
                && functions
                    .iter()
                    .any(|function| function.kind().is_read_only()))
            .then(|| "battery capability not declared".into())
        });
        let device = ParsedDevice::new_with_battery_diagnostic(
            key.clone(),
            functions,
            refresh_trigger,
            battery,
            battery_issue,
        )
        .map_err(|_| "Stored native description is invalid")?;
        NativeDescription::new(self.model, self.instance_type, device)
            .map_err(|_| "Stored native description is invalid")
    }
}

impl WireFunction {
    fn from_parsed(function: &ParsedFunction) -> Self {
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

    fn into_parsed(self) -> Result<ParsedFunction, &'static str> {
        match self {
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

impl WireMeasurement {
    fn from_binding(binding: &MeasurementBinding) -> Self {
        Self {
            property: binding.property().into(),
            format: binding.format(),
            unit: binding.unit(),
            min: binding.range().min(),
            max: binding.range().max(),
        }
    }

    fn from_battery(binding: &BatteryBinding) -> Self {
        Self {
            property: binding.property().into(),
            format: binding.format(),
            unit: MeasurementUnit::Percentage,
            min: binding.range().min(),
            max: binding.range().max(),
        }
    }

    fn into_binding(self) -> Result<MeasurementBinding, &'static str> {
        MeasurementBinding::new(
            self.property.into(),
            self.format,
            self.unit,
            MeasurementRange::new(self.min, self.max)
                .map_err(|_| "Stored measurement binding is invalid")?,
        )
        .map_err(|_| "Stored measurement binding is invalid")
    }

    fn into_battery(self) -> Result<BatteryBinding, &'static str> {
        self.into_binding().and_then(|binding| {
            BatteryBinding::new(binding).map_err(|_| "Stored battery binding is invalid")
        })
    }
}

impl WireNumeric {
    fn from_curtain_position(binding: &CurtainPositionBinding) -> Self {
        Self {
            property: binding.property().into(),
            format: binding.format(),
            min: binding.range().min(),
            max: binding.range().max(),
            step: binding.range().step(),
        }
    }

    fn into_curtain_position(self) -> Result<CurtainPositionBinding, &'static str> {
        let range = NumericRange::new(self.min, self.max, self.step)
            .map_err(|_| "Stored curtain binding is invalid")?;
        CurtainPositionBinding::new(self.property.into(), self.format, range)
            .map_err(|_| "Stored curtain binding is invalid")
    }
    fn from_brightness(binding: &BrightnessBinding) -> Self {
        Self {
            property: binding.property().into(),
            format: binding.format(),
            min: binding.range().min(),
            max: binding.range().max(),
            step: binding.range().step(),
        }
    }

    fn from_temperature(binding: &TemperatureBinding) -> Self {
        Self {
            property: binding.property().into(),
            format: binding.format(),
            min: binding.range().min(),
            max: binding.range().max(),
            step: binding.range().step(),
        }
    }

    fn into_brightness(self) -> Result<BrightnessBinding, &'static str> {
        BrightnessBinding::new(
            self.property.into(),
            self.format,
            NumericRange::new(self.min, self.max, self.step)
                .map_err(|_| "Stored light binding is invalid")?,
        )
        .map_err(|_| "Stored light binding is invalid")
    }

    fn into_temperature(self) -> Result<TemperatureBinding, &'static str> {
        TemperatureBinding::new(
            self.property.into(),
            self.format,
            NumericRange::new(self.min, self.max, self.step)
                .map_err(|_| "Stored light binding is invalid")?,
        )
        .map_err(|_| "Stored light binding is invalid")
    }
}
