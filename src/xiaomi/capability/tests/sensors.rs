//! MIoT temperature/humidity admission at the source boundary.

use super::*;
use crate::device::{FunctionKind, MeasurementRange};
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
    assert_eq!(measurement.step(), 0.1);
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
    assert_eq!(measurement.step(), 0.1);
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
fn measurement_service_semantics_are_required_but_the_service_iid_is_not_fixed() {
    let mut description = sensor_description();
    measurement_service_mut(&mut description)["type"] =
        json!("urn:miot-spec-v2:service:device-information:00007801:miaomiaoce-t6:1");
    assert_eq!(
        parse_device(sensor_key(), &description),
        Err(CapabilityError::MissingMeasurement("temperature"))
    );

    let mut description = sensor_description();
    measurement_service_mut(&mut description)["iid"] = json!(42);
    assert!(parse_device(sensor_key(), &description).is_ok());

    let mut description = sensor_description();
    let mut duplicate = measurement_service_mut(&mut description).clone();
    duplicate["iid"] = json!(4);
    description["services"]
        .as_array_mut()
        .unwrap()
        .push(duplicate);
    assert_eq!(
        parse_device(sensor_key(), &description),
        Err(CapabilityError::InvalidMeasurement("service"))
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
            assert_eq!(
                parse_device(sensor_key(), &description),
                Err(CapabilityError::MissingAccess(permission)),
                "{piid} without {permission}"
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
fn missing_or_duplicated_required_measurements_are_rejected() {
    let mut description = sensor_description();
    measurement_service_mut(&mut description)["properties"]
        .as_array_mut()
        .unwrap()
        .retain(|property| property["iid"] != 1001);
    assert!(matches!(
        parse_device(sensor_key(), &description),
        Err(CapabilityError::MissingMeasurement("temperature"))
    ));

    let mut description = sensor_description();
    measurement_service_mut(&mut description)["properties"]
        .as_array_mut()
        .unwrap()
        .retain(|property| property["iid"] != 1008);
    assert!(matches!(
        parse_device(sensor_key(), &description),
        Err(CapabilityError::MissingMeasurement("humidity"))
    ));

    let mut description = sensor_description();
    let duplicate = measurement_property_mut(&mut description, 1001).clone();
    measurement_service_mut(&mut description)["properties"]
        .as_array_mut()
        .unwrap()
        .push(duplicate);
    assert!(matches!(
        parse_device(sensor_key(), &description),
        Err(CapabilityError::InvalidDescription(
            "duplicate property iid"
        ))
    ));
}

#[test]
fn format_unit_range_and_matter_representability_are_enforced() {
    let cases = [
        (Some("format"), json!("bool"), "format"),
        (Some("unit"), json!("fahrenheit"), "unit"),
        (Some("value-range"), json!([-30, 100]), "range"),
        (Some("value-range"), json!([100, -30, 0.1]), "range"),
        (Some("value-range"), json!([-30, 100, 0]), "range"),
        (Some("value-range"), json!([-30, "100", 0.1]), "range"),
        (Some("value-range"), json!([-274, 100, 0.1]), "range"),
        (Some("value-range"), json!([-30, 328, 0.1]), "range"),
    ];
    for (field, value, reason) in cases {
        let mut description = sensor_description();
        let property = measurement_property_mut(&mut description, 1001);
        if let Some(field) = field {
            property[field] = value.clone();
        }
        assert_eq!(
            parse_device(sensor_key(), &description),
            Err(CapabilityError::InvalidMeasurement(reason)),
            "{field:?} {value}"
        );
    }

    let mut description = sensor_description();
    measurement_property_mut(&mut description, 1001)["format"] = json!("uint8");
    assert_eq!(
        parse_device(sensor_key(), &description),
        Err(CapabilityError::InvalidMeasurement("range"))
    );

    let cases = [
        (Some("unit"), json!("celsius"), "unit"),
        (Some("value-range"), json!([-0.001, 100, 0.1]), "range"),
        (Some("value-range"), json!([0, 100.1, 0.1]), "range"),
        (Some("value-range"), json!([100, 100, 0.1]), "range"),
    ];
    for (field, value, reason) in cases {
        let mut description = sensor_description();
        let property = measurement_property_mut(&mut description, 1008);
        if let Some(field) = field {
            property[field] = value.clone();
        }
        assert_eq!(
            parse_device(sensor_key(), &description),
            Err(CapabilityError::InvalidMeasurement(reason)),
            "{field:?} {value}"
        );
    }
}

#[test]
fn native_boundaries_are_strict_before_matter_rounding() {
    for (piid, range) in [
        (1001, json!([-273.1500000001, 100, 0.1])),
        (1001, json!([-30, 327.6700000001, 0.1])),
        (1008, json!([-1e-12, 100, 0.1])),
        (1008, json!([0, 100.0000000001, 0.1])),
    ] {
        let mut description = sensor_description();
        measurement_property_mut(&mut description, piid)["value-range"] = range.clone();
        assert_eq!(
            parse_device(sensor_key(), &description),
            Err(CapabilityError::InvalidMeasurement("range")),
            "{piid}: {range}"
        );
    }

    let mut description = sensor_description();
    measurement_property_mut(&mut description, 1001)["value-range"] = json!([-273.15, 327.67, 0.1]);
    assert!(parse_device(sensor_key(), &description).is_ok());
    let mut description = sensor_description();
    measurement_property_mut(&mut description, 1008)["value-range"] = json!([0, 100, 0.1]);
    assert!(parse_device(sensor_key(), &description).is_ok());
}

#[test]
fn measured_values_at_the_native_boundaries_are_strict() {
    let device = parse_device(sensor_key(), &sensor_description()).unwrap();
    let temperature = device.functions()[0].measurement().unwrap();
    assert_eq!(
        temperature.read_value(&json!(-273.1500000001)),
        Err(CapabilityError::InvalidValue("measurement"))
    );
    assert_eq!(
        temperature.read_value(&json!(327.6700000001)),
        Err(CapabilityError::InvalidValue("measurement"))
    );
    let humidity = device.functions()[1].measurement().unwrap();
    assert_eq!(
        humidity.read_value(&json!(-1e-12)),
        Err(CapabilityError::InvalidValue("measurement"))
    );
    assert_eq!(
        humidity.read_value(&json!(100.0000000001)),
        Err(CapabilityError::InvalidValue("measurement"))
    );
}

#[test]
fn full_standard_temperature_upper_bound_is_admitted() {
    let mut description = sensor_description();
    measurement_property_mut(&mut description, 1001)["value-range"] = json!([-30, 327.67, 0.1]);
    assert!(parse_device(sensor_key(), &description).is_ok());
    measurement_property_mut(&mut description, 1001)["value-range"] = json!([-30, 327.68, 0.1]);
    assert_eq!(
        parse_device(sensor_key(), &description),
        Err(CapabilityError::InvalidMeasurement("range"))
    );
}

#[test]
fn attached_measurements_on_other_device_categories_are_not_admitted() {
    let mut description = sensor_description();
    description["type"] = json!("urn:miot-spec-v2:device:light:0000A001:miaomiaoce-t6:1");
    assert_eq!(
        parse_device(sensor_key(), &description),
        Err(CapabilityError::UnsupportedDevice)
    );

    let mut description = sensor_description();
    description["type"] =
        json!("urn:miot-spec-v2:device:environment-sensor:0000A00B:miaomiaoce-t6:1");
    assert_eq!(
        parse_device(sensor_key(), &description),
        Err(CapabilityError::UnsupportedDevice)
    );
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

#[test]
fn projected_bounds_reject_values_that_round_into_the_matter_range() {
    // The raw bound is outside the Matter limit even though rounding would
    // land on the limit; admission must not repair it.
    let mut description = sensor_description();
    measurement_service_mut(&mut description)["properties"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|property| property["iid"] == 1001)
        .unwrap()["value-range"] = json!([-30, 327.671, 0.1]);
    assert_eq!(
        parse_device(sensor_key(), &description),
        Err(CapabilityError::InvalidMeasurement("range"))
    );

    let mut description = sensor_description();
    measurement_service_mut(&mut description)["properties"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|property| property["iid"] == 1008)
        .unwrap()["value-range"] = json!([-0.001, 100, 0.1]);
    assert_eq!(
        parse_device(sensor_key(), &description),
        Err(CapabilityError::InvalidMeasurement("range"))
    );
}
