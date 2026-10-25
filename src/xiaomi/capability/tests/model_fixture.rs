//! Valid native fixtures shared by module tests that construct logical devices.

use crate::{
    device::{FunctionKind, LightProfile},
    xiaomi::capability::NativeDescription,
    xiaomi::capability::{
        BatteryBinding, BrightnessBinding, ColorBinding, LightBindings, MeasurementBinding,
        MeasurementUnit, NumericFormat, ParsedDevice, ParsedFunction, PropertyBinding,
        TemperatureBinding,
    },
};

/// A real native description for a synthetic test device, built through the
/// same source constructors used by the network boundary.
pub(crate) fn native_for(
    description: &crate::device::PhysicalDeviceDescription,
) -> NativeDescription {
    let controls = description
        .functions()
        .iter()
        .filter(|function| !function.is_measurement())
        .count();
    let multiple = controls > 1;
    let sensor = description
        .functions()
        .iter()
        .any(|function| function.kind.is_measurement());
    let functions = description
        .functions()
        .iter()
        .map(|function| {
            let id = function.id.clone();
            let label = function.label.clone();
            match function.kind {
                FunctionKind::Temperature(range) => ParsedFunction::temperature(
                    id,
                    label,
                    MeasurementBinding::new(
                        PropertyBinding {
                            siid: 3,
                            piid: 1001,
                        },
                        NumericFormat::Float,
                        MeasurementUnit::Celsius,
                        range,
                    )
                    .unwrap(),
                ),
                FunctionKind::Humidity(range) => ParsedFunction::humidity(
                    id,
                    label,
                    MeasurementBinding::new(
                        PropertyBinding {
                            siid: 3,
                            piid: 1008,
                        },
                        NumericFormat::Float,
                        MeasurementUnit::Percentage,
                        range,
                    )
                    .unwrap(),
                ),
                FunctionKind::Outlet => {
                    let siid = match (multiple, id.as_str()) {
                        (true, "outlet_1") => 3,
                        (true, "outlet_2") => 4,
                        (true, "usb") => 9,
                        _ => 2,
                    };
                    ParsedFunction::outlet(id, label, PropertyBinding { siid, piid: 1 })
                }
                FunctionKind::Light(profile) => {
                    let power = PropertyBinding { siid: 2, piid: 1 };
                    let lighting = match profile {
                        LightProfile::OnOff => LightBindings::OnOff,
                        LightProfile::Dimmable => {
                            LightBindings::new(Some(brightness_binding()), None, None, None)
                                .unwrap()
                        }
                        LightProfile::ColorTemperature(interval) => LightBindings::new(
                            Some(brightness_binding()),
                            Some(temperature_binding(interval)),
                            None,
                            None,
                        )
                        .unwrap(),
                        LightProfile::ExtendedColor(interval) => LightBindings::new(
                            Some(brightness_binding()),
                            Some(temperature_binding(interval)),
                            Some(
                                ColorBinding::new(
                                    PropertyBinding { siid: 2, piid: 5 },
                                    NumericFormat::Uint32,
                                )
                                .unwrap(),
                            ),
                            None,
                        )
                        .unwrap(),
                    };
                    ParsedFunction::light(id, label, power, lighting)
                }
            }
        })
        .collect::<Vec<_>>();
    let key = description.key().clone();
    if sensor {
        let battery = description.has_battery().then(|| {
            BatteryBinding::new(
                MeasurementBinding::new(
                    PropertyBinding {
                        siid: 2,
                        piid: 1003,
                    },
                    NumericFormat::Uint8,
                    MeasurementUnit::Percentage,
                    crate::device::MeasurementRange::new(0.0, 100.0).unwrap(),
                )
                .unwrap(),
            )
            .unwrap()
        });
        return NativeDescription::new(
            "test.sensor".into(),
            if description
                .functions()
                .iter()
                .any(|function| function.kind.is_light())
            {
                "urn:miot-spec-v2:device:light:0000A001:test-v1:1"
            } else if controls > 0 {
                "urn:miot-spec-v2:device:outlet:0000A002:test-v1:1"
            } else {
                "urn:miot-spec-v2:device:temperature-humidity-sensor:0000A00A:test-v1:1"
            }
            .into(),
            ParsedDevice::new_with_battery(key, functions, None, battery).unwrap(),
        )
        .unwrap();
    }
    if multiple {
        return NativeDescription::new(
            "cuco.plug.cp7pd".into(),
            "urn:miot-spec-v2:device:outlet:0000A002:cuco-cp7pd:1".into(),
            ParsedDevice::new(key, functions, Some(PropertyBinding { siid: 2, piid: 1 })).unwrap(),
        )
        .unwrap();
    }
    let is_light = description
        .functions()
        .iter()
        .any(|function| function.kind.is_light());
    NativeDescription::new(
        if is_light {
            "test.light"
        } else {
            "test.outlet"
        }
        .into(),
        if is_light {
            "urn:miot-spec-v2:device:light:0000A001:test-v1:1"
        } else {
            "urn:miot-spec-v2:device:outlet:0000A002:test-v1:1"
        }
        .into(),
        ParsedDevice::new(key, functions, None).unwrap(),
    )
    .unwrap()
}

fn brightness_binding() -> BrightnessBinding {
    BrightnessBinding::new(
        PropertyBinding { siid: 2, piid: 3 },
        NumericFormat::Uint8,
        crate::device::NumericRange::new(1, 100, 1).unwrap(),
    )
    .unwrap()
}

fn temperature_binding(interval: crate::device::TemperatureInterval) -> TemperatureBinding {
    TemperatureBinding::new(
        PropertyBinding { siid: 2, piid: 4 },
        NumericFormat::Uint16,
        crate::device::NumericRange::new(interval.min(), interval.max(), 1).unwrap(),
    )
    .unwrap()
}
