//! Valid native fixtures shared by module tests that construct logical devices.

use crate::{
    device::{FunctionKind, LightProfile},
    xiaomi::capability::NativeDescription,
    xiaomi::capability::{
        BatteryBinding, BrightnessBinding, ColorBinding, EventBinding, LightBindings,
        MeasurementBinding, MeasurementUnit, MotionBinding, NumericFormat, ParsedDevice,
        ParsedFunction, PropertyBinding, StateBinding, StateMapping, StateValue,
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
        .filter(|function| !function.kind.is_read_only())
        .count();
    let multiple = controls > 1;
    let sensor = description
        .functions()
        .iter()
        .any(|function| function.kind.is_read_only());
    let functions = description
        .functions()
        .iter()
        .map(|function| {
            let label = function.label.clone();
            let id = match function.kind {
                FunctionKind::Temperature(_) => "temperature:3:1001",
                FunctionKind::Humidity(_) => "humidity:3:1008",
                FunctionKind::CarbonDioxide(_) => "co2:8:1029",
                FunctionKind::Illuminance(_) => "illuminance:42:1",
                FunctionKind::Contact => "contact:50:1",
                FunctionKind::Motion => "motion:51:1",
                FunctionKind::Occupancy => "occupancy:52:1",
                FunctionKind::Fan { .. } => "fan:2",
                FunctionKind::Curtain => "curtain:2",
                FunctionKind::Light(_) => "light:2",
                FunctionKind::Outlet => match (multiple, function.id.as_str()) {
                    (true, "outlet_1" | "outlet:3") => "outlet:3",
                    (true, "outlet_2" | "outlet:4") => "outlet:4",
                    (true, "usb" | "outlet:9") => "outlet:9",
                    _ => "outlet:2",
                },
            }
            .to_owned();
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
                FunctionKind::CarbonDioxide(range) => ParsedFunction::co2(
                    id,
                    label,
                    MeasurementBinding::new(
                        PropertyBinding {
                            siid: 8,
                            piid: 1029,
                        },
                        NumericFormat::Float,
                        MeasurementUnit::Ppm,
                        range,
                    )
                    .unwrap(),
                ),
                FunctionKind::Illuminance(range) => ParsedFunction::illuminance(
                    id,
                    label,
                    MeasurementBinding::new(
                        PropertyBinding { siid: 42, piid: 1 },
                        NumericFormat::Float,
                        MeasurementUnit::Lux,
                        range,
                    )
                    .unwrap(),
                ),
                FunctionKind::Contact => ParsedFunction::contact(
                    id,
                    label,
                    StateBinding::new(
                        PropertyBinding { siid: 50, piid: 1 },
                        StateMapping::ContactBoolean,
                    )
                    .unwrap(),
                ),
                FunctionKind::Motion => ParsedFunction::motion_sensor(
                    id,
                    label,
                    MotionBinding::new(
                        StateBinding::new(
                            PropertyBinding { siid: 51, piid: 1 },
                            StateMapping::NoMotionDuration {
                                format: NumericFormat::Uint16,
                            },
                        )
                        .unwrap(),
                        EventBinding { siid: 51, eiid: 1 },
                        vec![],
                    )
                    .unwrap(),
                ),
                FunctionKind::Occupancy => ParsedFunction::occupancy(
                    id,
                    label,
                    StateBinding::new(
                        PropertyBinding { siid: 52, piid: 1 },
                        StateMapping::Enumerated {
                            format: NumericFormat::Uint8,
                            values: vec![
                                StateValue {
                                    native: 0,
                                    active: Some(false),
                                },
                                StateValue {
                                    native: 1,
                                    active: Some(true),
                                },
                                StateValue {
                                    native: 2,
                                    active: None,
                                },
                            ],
                        },
                    )
                    .unwrap(),
                ),
                FunctionKind::Outlet => {
                    let siid = match (multiple, id.as_str()) {
                        (true, "outlet:3") => 3,
                        (true, "outlet:4") => 4,
                        (true, "outlet:9") => 9,
                        _ => 2,
                    };
                    ParsedFunction::outlet(id, label, PropertyBinding { siid, piid: 1 })
                }
                FunctionKind::Fan {
                    horizontal_swing, ..
                } => {
                    let mut instance: serde_json::Value = serde_json::from_str(include_str!(
                        "../../../../tests/fixtures/miot/dmaker-p5c.json"
                    ))
                    .unwrap();
                    if !horizontal_swing {
                        instance["services"][1]["properties"][3]["access"] =
                            serde_json::json!(["read"]);
                    }
                    let parsed = crate::xiaomi::capability::parse_device_for_model(
                        description.key().clone(),
                        "dmaker.fan.p5c",
                        &instance,
                    )
                    .unwrap();
                    ParsedFunction::fan_control(
                        id,
                        label,
                        parsed.functions()[0].fan().unwrap().clone(),
                    )
                }
                FunctionKind::Curtain => {
                    let instance = serde_json::from_str(include_str!(
                        "../../../../tests/fixtures/miot/xiaomi-acn010.json"
                    ))
                    .unwrap();
                    let parsed = crate::xiaomi::capability::parse_device_for_model(
                        description.key().clone(),
                        "xiaomi.curtain.acn010",
                        &instance,
                    )
                    .unwrap();
                    ParsedFunction::curtain_control(
                        id,
                        label,
                        parsed.functions()[0].curtain().unwrap().clone(),
                    )
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
                .any(|function| function.kind == FunctionKind::Curtain)
            {
                "urn:miot-spec-v2:device:curtain:0000A00C:xiaomi-acn010:1:0000D031"
            } else if description
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
    let is_curtain = description
        .functions()
        .iter()
        .any(|function| function.kind == FunctionKind::Curtain);
    let is_fan = description
        .functions()
        .iter()
        .any(|function| matches!(function.kind, FunctionKind::Fan { .. }));
    NativeDescription::new(
        if is_fan {
            "dmaker.fan.p5c"
        } else if is_curtain {
            "xiaomi.curtain.acn010"
        } else if is_light {
            "test.light"
        } else {
            "test.outlet"
        }
        .into(),
        if is_fan {
            "urn:miot-spec-v2:device:fan:0000A005:dmaker-p5c:1"
        } else if is_curtain {
            "urn:miot-spec-v2:device:curtain:0000A00C:xiaomi-acn010:1:0000D031"
        } else if is_light {
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
