//! Native capability records and their checked decoding boundary.

mod function;

use crate::xiaomi::capability::NativeDescription;
use crate::{
    device::{DeviceKey, MeasurementRange, NumericRange, TemperatureGrid},
    xiaomi::capability::{
        AirConditionerBindings, AirConditionerModeBinding, AirConditionerSpeedBinding,
        AirConditionerTemperatureBinding, BatteryBinding, BrightnessBinding, ColorBinding,
        CurtainBindings, CurtainMotorBinding, CurtainPositionBinding, CurtainStatusBinding,
        EventArgument, EventBinding, FanBindings, FanModeBinding, FanSpeedBinding,
        FanSpeedEncoding, LightBindings, MeasurementBinding, MeasurementUnit, ModeBinding,
        MotionBinding, NumericFormat, ParsedDevice, ParsedFunction, PropertyBinding,
        RecognitionMetadata, StateBinding, StateMapping, StateValue, TemperatureBinding,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    recognition: Option<RecognitionMetadata>,
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
    Fan {
        id: String,
        label: String,
        power: WireBinding,
        mode: Option<WireFanMode>,
        speed: WireFanSpeed,
        swing: Option<WireBinding>,
    },
    AirConditioner {
        id: String,
        label: String,
        power: WireBinding,
        mode: WireAirConditionerEnum<5>,
        temperature: WireAirConditionerTemperature,
        speed: WireAirConditionerEnum<4>,
        swing: Option<WireBinding>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mcn04: Option<WireMcn04>,
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
struct WireMcn04 {
    state: WireBinding,
    range: WireBinding,
}

#[derive(Serialize, Deserialize)]
#[serde(
    deny_unknown_fields,
    bound(
        serialize = "[i64; N]: Serialize",
        deserialize = "[i64; N]: Deserialize<'de>"
    )
)]
struct WireAirConditionerEnum<const N: usize> {
    property: WireBinding,
    format: NumericFormat,
    codes: [i64; N],
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireAirConditionerTemperature {
    property: WireBinding,
    format: NumericFormat,
    min: i16,
    max: i16,
    step: i16,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireFanSpeed {
    property: WireBinding,
    format: NumericFormat,
    encoding: FanSpeedEncoding,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireFanMode {
    property: WireBinding,
    format: NumericFormat,
    straight: u32,
    values: Vec<u32>,
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
            recognition: native.device().recognition().cloned(),
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
        let mut device = ParsedDevice::new_with_battery_diagnostic(
            key.clone(),
            functions,
            refresh_trigger,
            battery,
            self.battery_issue,
        )
        .map_err(|_| "Stored native description is invalid")?;
        if let Some(recognition) = self.recognition {
            device = device
                .with_recognition(recognition)
                .map_err(|_| "Stored native description is invalid")?;
        }
        NativeDescription::new(self.model, self.instance_type, device)
            .map_err(|_| "Stored native description is invalid")
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
