//! MIoT temperature/humidity admission at the source boundary.

use super::*;
use crate::device::{FunctionKind, MeasurementRange};
use crate::xiaomi::capability::ParsedDevice;
use crate::xiaomi::capability::{MeasurementUnit, NumericFormat};
use serde_json::{Value, json};

fn sensor_description() -> Value {
    // Source: miot-spec.org public instance for
    // urn:miot-spec-v2:device:temperature-humidity-sensor:0000A00A:miaomiaoce-t6:1
    serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/miaomiaoce-t6.json"
    ))
    .unwrap()
}

fn sensor_key() -> crate::device::DeviceKey {
    crate::device::DeviceKey::new("xiaomi", "sensor")
}

fn measurement_service_mut(description: &mut Value) -> &mut Value {
    description["services"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|service| {
            service["properties"]
                .as_array()
                .unwrap()
                .iter()
                .any(|property| property["iid"] == 1001)
        })
        .unwrap()
}

fn measurement_property_mut(description: &mut Value, piid: u32) -> &mut Value {
    measurement_service_mut(description)["properties"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|property| property["iid"] == piid)
        .unwrap()
}

#[test]
fn measurements_are_independent_of_each_other_and_the_host_category() {
    for host in [
        "urn:miot-spec-v2:device:temperature-humidity-sensor:0000A00A:sample:1",
        "urn:miot-spec-v2:device:night-light:0000A0AB:sample:1",
        "urn:miot-spec-v2:device:air-conditioner:0000A004:sample:1",
    ] {
        for wanted in [[true, false], [false, true], [true, true]] {
            let mut description = sensor_description();
            description["type"] = json!(host);
            measurement_service_mut(&mut description)["properties"]
                .as_array_mut()
                .unwrap()
                .retain(|property| match property["iid"].as_u64().unwrap() {
                    1001 => wanted[0],
                    1008 => wanted[1],
                    _ => true,
                });
            let device = parse_device(sensor_key(), &description).unwrap();
            assert_eq!(
                device.functions().len(),
                wanted.into_iter().filter(|value| *value).count()
            );
            assert!(device.is_read_only());
        }
    }
}

#[test]
fn environment_temperature_is_selected_by_service_semantics() {
    let mut description = sensor_description();
    measurement_service_mut(&mut description)["type"] =
        json!("urn:miot-spec-v2:service:environment:0000780A:sample:1");
    let device = parse_device(sensor_key(), &description).unwrap();
    assert_eq!(device.functions()[0].id(), "temperature");

    measurement_service_mut(&mut description)["type"] =
        json!("urn:miot-spec-v2:service:outlet:00007802:sample:1");
    let device = parse_device(sensor_key(), &description).unwrap();
    assert_eq!(device.functions().len(), 1);
    assert_eq!(device.functions()[0].id(), "humidity");
}

#[test]
fn supported_controls_and_measurements_share_one_physical_device() {
    let mut description = public_description();
    let mut environment = measurement_service_mut(&mut sensor_description()).clone();
    environment["iid"] = json!(42);
    description["services"]
        .as_array_mut()
        .unwrap()
        .push(environment);
    let device = parse_device(sensor_key(), &description).unwrap();
    assert_eq!(
        device
            .functions()
            .iter()
            .map(|function| function.id())
            .collect::<Vec<_>>(),
        ["main", "temperature", "humidity"]
    );
    assert!(!device.is_read_only());
}

#[test]
fn first_qualified_measurement_wins_and_measurement_steps_are_ignored() {
    let mut description = sensor_description();
    measurement_property_mut(&mut description, 1001)["access"] = json!(["read"]);
    let mut second = measurement_service_mut(&mut sensor_description()).clone();
    second["iid"] = json!(42);
    description["services"].as_array_mut().unwrap().push(second);
    let device = parse_device(sensor_key(), &description).unwrap();
    assert_eq!(
        device.functions()[0].measurement().unwrap().property().siid,
        42
    );

    let mut other = description.clone();
    for service in other["services"].as_array_mut().unwrap() {
        for property in service["properties"].as_array_mut().unwrap() {
            if property["value-range"].is_array() {
                property["value-range"][2] = json!(0);
            }
        }
    }
    assert_eq!(parse_device(sensor_key(), &other).unwrap(), device);
}

#[test]
fn standard_sensor_admits_independent_temperature_and_humidity_bindings() {
    let device = parse_device(sensor_key(), &sensor_description()).unwrap();
    assert_eq!(*device.key(), sensor_key());
    assert!(device.is_read_only());
    assert_eq!(device.refresh_trigger(), None);
    assert_eq!(
        device
            .functions()
            .iter()
            .map(|function| function.id())
            .collect::<Vec<_>>(),
        ["temperature", "humidity"]
    );

    let temperature = &device.functions()[0];
    assert_eq!(temperature.power(), None);
    let measurement = temperature.measurement().unwrap();
    assert_eq!(
        measurement.property(),
        PropertyBinding {
            siid: 3,
            piid: 1001
        }
    );
    assert_eq!(measurement.format(), NumericFormat::Float);
    assert_eq!(measurement.unit(), MeasurementUnit::Celsius);
    assert_eq!(measurement.range().min(), -30.0);
    assert_eq!(measurement.range().max(), 100.0);
    assert_eq!(
        temperature.kind(),
        FunctionKind::Temperature(MeasurementRange::new(-30.0, 100.0).unwrap())
    );

    let humidity = &device.functions()[1];
    assert_eq!(humidity.power(), None);
    let measurement = humidity.measurement().unwrap();
    assert_eq!(
        measurement.property(),
        PropertyBinding {
            siid: 3,
            piid: 1008
        }
    );
    assert_eq!(measurement.format(), NumericFormat::Float);
    assert_eq!(measurement.unit(), MeasurementUnit::Percentage);
    assert_eq!(measurement.range().min(), 0.0);
    assert_eq!(measurement.range().max(), 100.0);
    assert_eq!(
        humidity.kind(),
        FunctionKind::Humidity(MeasurementRange::new(0.0, 100.0).unwrap())
    );
}

#[test]
fn official_t6_uses_the_standard_temperature_property_semantics() {
    // The fixture is the fetched official static description, not a
    // hand-written candidate. Its service is temperature-humidity-sensor with
    // property temperature:00000020 and relative-humidity:0000000C.
    let description = sensor_description();
    assert!(
        description["services"]
            .as_array()
            .unwrap()
            .iter()
            .any(|service| service["type"]
                .as_str()
                .is_some_and(|urn| urn.contains("service:temperature-humidity-sensor:00007814")))
    );
    let device = parse_device(sensor_key(), &description).unwrap();
    assert_eq!(device.functions().len(), 2);
    assert_eq!(
        device.functions()[0].measurement().unwrap().property(),
        PropertyBinding {
            siid: 3,
            piid: 1001
        }
    );
}

#[test]
fn optional_battery_is_ignored_and_does_not_create_a_function() {
    let device = parse_device(sensor_key(), &sensor_description()).unwrap();
    assert_eq!(device.functions().len(), 2);
    assert!(
        device
            .functions()
            .iter()
            .all(|function| function.measurement().is_some())
    );
    assert!(
        device
            .functions()
            .iter()
            .all(|function| function.lighting().is_none())
    );
}

#[test]
fn both_measurements_need_read_and_notify_access_but_not_write() {
    for piid in [1001, 1008] {
        for permission in ["read", "notify"] {
            let mut description = sensor_description();
            measurement_property_mut(&mut description, piid)["access"] = json!(
                ["read", "notify", "write"]
                    .into_iter()
                    .filter(|value| *value != permission)
                    .collect::<Vec<_>>()
            );
            let device = parse_device(sensor_key(), &description).unwrap();
            assert_eq!(device.functions().len(), 1, "{piid} without {permission}");
            assert_ne!(
                device.functions()[0].measurement().unwrap().property().piid,
                piid
            );
        }
        let mut description = sensor_description();
        measurement_property_mut(&mut description, piid)["access"] = json!(["read", "notify"]);
        assert!(parse_device(sensor_key(), &description).is_ok());
    }
    let mut description = sensor_description();
    description["services"]
        .as_array_mut()
        .unwrap()
        .retain(|service| service["iid"] != 2);
    assert!(parse_device(sensor_key(), &description).is_ok());
}

#[test]
fn measured_values_at_the_native_boundaries_are_strict() {
    let mut description = sensor_description();
    measurement_property_mut(&mut description, 1001)["value-range"] = json!([-273.15, 327.67, 0.1]);
    let device = parse_device(sensor_key(), &description).unwrap();
    for (index, min, max, below, above) in [
        (0, -273.15, 327.67, -273.1500000001, 327.6700000001),
        (1, 0.0, 100.0, -1e-12, 100.0000000001),
    ] {
        let measurement = device.functions()[index].measurement().unwrap();
        for valid in [min, max] {
            assert_eq!(measurement.read_value(&json!(valid)), Ok(valid));
        }
        for invalid in [below, above] {
            assert_eq!(
                measurement.read_value(&json!(invalid)),
                Err(CapabilityError::InvalidValue("measurement"))
            );
        }
    }
}

#[test]
fn source_values_are_validated_by_their_declared_format_and_range() {
    let device = parse_device(sensor_key(), &sensor_description()).unwrap();
    let temperature = device.functions()[0].measurement().unwrap();
    assert_eq!(temperature.read_value(&json!(21.5)).unwrap(), 21.5);
    assert_eq!(temperature.read_value(&json!(-30)).unwrap(), -30.0);
    assert_eq!(temperature.read_value(&json!(100)).unwrap(), 100.0);
    for invalid in [
        json!(null),
        json!(true),
        json!("21.5"),
        json!(100.0001),
        json!(-30.0001),
        json!([]),
    ] {
        assert_eq!(
            temperature.read_value(&invalid),
            Err(CapabilityError::InvalidValue("measurement")),
            "{invalid}"
        );
    }

    let mut description = sensor_description();
    measurement_property_mut(&mut description, 1001)["format"] = json!("int16");
    measurement_property_mut(&mut description, 1001)["value-range"] = json!([-30, 100, 1]);
    let device = parse_device(sensor_key(), &description).unwrap();
    let temperature = device.functions()[0].measurement().unwrap();
    assert_eq!(temperature.read_value(&json!(23)).unwrap(), 23.0);
    assert_eq!(
        temperature.read_value(&json!(23.5)),
        Err(CapabilityError::InvalidValue("measurement"))
    );
}

fn battery_service_mut(description: &mut Value) -> &mut Value {
    description["services"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|service| {
            service["type"]
                .as_str()
                .is_some_and(|urn| urn.contains("service:battery:00007805"))
        })
        .unwrap()
}

fn battery_property_mut(description: &mut Value) -> &mut Value {
    battery_service_mut(description)["properties"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|property| property["iid"] == 1003)
        .unwrap()
}

#[test]
fn standard_battery_is_a_physical_binding_not_a_third_function() {
    let device = parse_device(sensor_key(), &sensor_description()).unwrap();
    assert_eq!(device.functions().len(), 2);
    assert!(device.battery_issue().is_none());
    let battery = device.battery().unwrap();
    assert_eq!(
        battery.property(),
        PropertyBinding {
            siid: 2,
            piid: 1003
        }
    );
    assert_eq!(battery.format(), NumericFormat::Uint8);
    assert_eq!(battery.range().min(), 0.0);
    assert_eq!(battery.range().max(), 100.0);
    assert!(device.is_read_only());
}

#[test]
fn another_sensor_model_uses_standard_battery_semantics_with_other_instance_ids() {
    let mut description = sensor_description();
    description["type"] =
        json!("urn:miot-spec-v2:device:temperature-humidity-sensor:0000A00A:vendor-t7:1");
    measurement_service_mut(&mut description)["iid"] = json!(7);
    measurement_property_mut(&mut description, 1008)["iid"] = json!(2008);
    measurement_property_mut(&mut description, 1001)["iid"] = json!(2001);
    let battery = battery_service_mut(&mut description);
    battery["iid"] = json!(9);
    battery["properties"][0]["iid"] = json!(3003);

    let device = parse_device(sensor_key(), &description).expect("second compatible sensor model");
    assert!(device.battery_issue().is_none());
    assert_eq!(
        device.functions()[0].measurement().unwrap().property().siid,
        7
    );
    assert_eq!(
        device.functions()[1].measurement().unwrap().property(),
        PropertyBinding {
            siid: 7,
            piid: 2008
        }
    );
    assert_eq!(
        device.battery().unwrap().property(),
        PropertyBinding {
            siid: 9,
            piid: 3003
        }
    );
}

#[test]
fn absent_invalid_or_non_notifying_battery_is_diagnosed_and_omitted() {
    let mut missing = sensor_description();
    missing["services"]
        .as_array_mut()
        .unwrap()
        .retain(|service| {
            !service["type"]
                .as_str()
                .is_some_and(|urn| urn.contains("service:battery:"))
        });
    for (description, expected) in [
        (missing, "battery"),
        {
            let mut value = sensor_description();
            battery_property_mut(&mut value)["access"] = json!(["read"]);
            (value, "notify")
        },
        {
            let mut value = sensor_description();
            battery_property_mut(&mut value)["access"] = json!(["notify"]);
            (value, "read")
        },
        {
            let mut value = sensor_description();
            battery_property_mut(&mut value)["unit"] = json!("celsius");
            (value, "unit")
        },
        {
            let mut value = sensor_description();
            battery_service_mut(&mut value)["properties"] = json!([]);
            (value, "battery-level")
        },
    ] {
        let device = parse_device(sensor_key(), &description).unwrap();
        assert!(device.battery().is_none());
        let issue = device.battery_issue().expect("battery diagnosis");
        assert!(
            issue.to_ascii_lowercase().contains(expected),
            "{expected}: {issue}"
        );
        assert_eq!(device.functions().len(), 2);
        assert!(device.is_read_only());
    }
}

#[test]
fn invalid_battery_format_unit_range_and_duplicates_are_not_published() {
    for (field, value) in [
        ("format", json!("bool")),
        ("unit", json!("percentage-points")),
        ("value-range", json!([0, 101, 1])),
        ("value-range", json!([-1, 100, 1])),
    ] {
        let mut description = sensor_description();
        battery_property_mut(&mut description)[field] = value.clone();
        let device = parse_device(sensor_key(), &description).unwrap();
        assert!(device.battery().is_none(), "{field} {value}");
        assert!(device.battery_issue().is_some(), "{field} {value}");
        assert_eq!(device.functions().len(), 2);
    }

    let mut description = sensor_description();
    let duplicate = battery_property_mut(&mut description).clone();
    let mut duplicate = duplicate;
    duplicate["iid"] = json!(1004);
    battery_service_mut(&mut description)["properties"]
        .as_array_mut()
        .unwrap()
        .push(duplicate);
    let device = parse_device(sensor_key(), &description).unwrap();
    assert!(device.battery().is_none());
    assert!(device.battery_issue().is_some());

    let mut description = sensor_description();
    let mut duplicate = battery_service_mut(&mut description).clone();
    duplicate["iid"] = json!(42);
    description["services"]
        .as_array_mut()
        .unwrap()
        .push(duplicate);
    let device = parse_device(sensor_key(), &description).unwrap();
    assert!(device.battery().is_none());
    assert!(device.battery_issue().is_some());
}

#[test]
fn battery_values_follow_the_declared_integer_percentage_range() {
    let device = parse_device(sensor_key(), &sensor_description()).unwrap();
    let battery = device.battery().unwrap();
    assert_eq!(battery.read_value(&json!(0)).unwrap(), 0.0);
    assert_eq!(battery.read_value(&json!(5)).unwrap(), 5.0);
    assert_eq!(battery.read_value(&json!(10)).unwrap(), 10.0);
    assert_eq!(battery.read_value(&json!(100)).unwrap(), 100.0);
    for invalid in [
        json!(101),
        json!(-1),
        json!(5.5),
        json!(null),
        json!(true),
        json!("5"),
    ] {
        assert_eq!(
            battery.read_value(&invalid),
            Err(CapabilityError::InvalidValue("battery")),
            "{invalid}"
        );
    }
}

#[test]
fn a_battery_binding_alone_cannot_create_a_device() {
    use crate::xiaomi::capability::ParsedFunction;
    let battery = parse_device(sensor_key(), &sensor_description())
        .unwrap()
        .battery()
        .unwrap()
        .clone();
    let outlet = ParsedFunction::outlet("main", "Main", PropertyBinding { siid: 2, piid: 1 });
    assert!(
        ParsedDevice::new_with_battery(sensor_key(), vec![outlet], None, Some(battery),).is_err()
    );
}

#[test]
fn battery_diagnostics_do_not_change_native_capability_equality() {
    let mut missing = sensor_description();
    missing["services"]
        .as_array_mut()
        .unwrap()
        .retain(|service| {
            !service["type"]
                .as_str()
                .is_some_and(|urn| urn.contains("service:battery:"))
        });
    let diagnosed = parse_device(sensor_key(), &missing).unwrap();
    let stored = ParsedDevice::new(
        diagnosed.key().clone(),
        diagnosed.functions().to_vec(),
        diagnosed.refresh_trigger(),
    )
    .unwrap();
    assert_eq!(diagnosed, stored);
    assert!(diagnosed.battery_issue().is_some());
    assert!(stored.battery_issue().is_none());
}

#[test]
fn public_new3pr_extracts_co2_independently_in_standard_measurement_order() {
    // Source: https://miot-spec.org/miot-spec-v2/instance?type=urn:miot-spec-v2:device:temperature-humidity-sensor:0000A00A:xiaomi-new3pr:1:0000D063
    let description: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/xiaomi-new3pr.json"
    ))
    .unwrap();
    for mask in 1..8 {
        let mut instance = description.clone();
        instance["type"] = json!("urn:miot-spec-v2:device:air-conditioner:0000A004:sample:1");
        for service in instance["services"].as_array_mut().unwrap() {
            if let Some(properties) = service["properties"].as_array_mut() {
                properties.retain(|property| {
                    match property["type"]
                        .as_str()
                        .unwrap()
                        .split(':')
                        .nth(3)
                        .unwrap()
                    {
                        "temperature" => mask & 1 != 0,
                        "relative-humidity" => mask & 2 != 0,
                        "co2-density" => mask & 4 != 0,
                        _ => true,
                    }
                });
            }
        }
        let device = parse_device(sensor_key(), &instance).unwrap();
        let expected = ["temperature", "humidity", "co2"]
            .into_iter()
            .enumerate()
            .filter_map(|(index, id)| (mask & (1 << index) != 0).then_some(id))
            .collect::<Vec<_>>();
        assert_eq!(
            device
                .functions()
                .iter()
                .map(|function| function.id())
                .collect::<Vec<_>>(),
            expected,
            "mask {mask}"
        );
        assert!(device.is_read_only());
        assert!(device.battery().is_some());
        if let Some(co2) = device
            .functions()
            .iter()
            .find(|function| function.id() == "co2")
        {
            let measurement = co2.measurement().unwrap();
            assert_eq!(
                measurement.property(),
                PropertyBinding {
                    siid: 8,
                    piid: 1029
                }
            );
            assert_eq!(measurement.format(), NumericFormat::Uint16);
            assert_eq!(
                measurement.range(),
                MeasurementRange::new(400.0, 5000.0).unwrap()
            );
        }
    }
}

#[test]
fn co2_requires_its_declared_ppm_read_and_notify_capabilities_and_combines_with_controls() {
    let description: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/xiaomi-new3pr.json"
    ))
    .unwrap();
    let mut co2_service = description["services"]
        .as_array()
        .unwrap()
        .iter()
        .find(|service| service["iid"] == 8)
        .unwrap()
        .clone();
    co2_service["iid"] = json!(42);
    let mut instance = public_description();
    instance["services"]
        .as_array_mut()
        .unwrap()
        .push(co2_service.clone());
    let parsed = parse_device(sensor_key(), &instance).unwrap();
    assert_eq!(
        parsed
            .functions()
            .iter()
            .map(|function| function.id())
            .collect::<Vec<_>>(),
        ["main", "co2"]
    );
    assert_eq!(
        parsed.functions()[0].power(),
        Some(PropertyBinding { siid: 2, piid: 1 })
    );
    assert_eq!(
        parsed.functions()[1].measurement().unwrap().unit(),
        MeasurementUnit::Ppm
    );
    for (field, value) in [
        ("access", json!(["read"])),
        ("access", json!(["notify"])),
        ("unit", json!("percentage")),
    ] {
        let mut instance = instance.clone();
        let service = instance["services"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|service| service["iid"] == 42)
            .unwrap();
        let property = service["properties"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|property| property["iid"] == 1029)
            .unwrap();
        property[field] = value;
        let parsed = parse_device(sensor_key(), &instance).unwrap();
        assert_eq!(
            parsed
                .functions()
                .iter()
                .map(|function| function.id())
                .collect::<Vec<_>>(),
            ["main"]
        );
    }
}
