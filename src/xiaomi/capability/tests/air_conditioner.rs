use crate::{
    device::air_conditioner::{AirConditionerFanSpeed, AirConditionerMode},
    xiaomi::capability::{
        PropertyBinding, air_conditioner::parse_air_conditioner, description::Description,
    },
};
use serde_json::{Value, json};

fn instance() -> Value {
    // Public MIoT instance: urn:miot-spec-v2:device:air-condition-outlet:0000A045:lumi-mcn04:1
    serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/lumi-mcn04.json"
    ))
    .unwrap()
}

fn parse(value: &Value) -> super::super::model::AirConditionerBindings {
    let description = Description::parse(value).unwrap();
    parse_air_conditioner(&description, &mut Vec::new()).unwrap()
}

#[test]
fn standard_air_conditioner_binds_the_real_services_and_native_semantics() {
    let bindings = parse(&instance());
    assert_eq!(bindings.function_id(), "air-conditioner:3");
    assert_eq!(bindings.power(), PropertyBinding { siid: 3, piid: 1 });
    assert_eq!(
        bindings.mode().property(),
        PropertyBinding { siid: 3, piid: 2 }
    );
    assert_eq!(
        bindings.temperature().property(),
        PropertyBinding { siid: 3, piid: 4 }
    );
    assert_eq!(
        bindings.speed().property(),
        PropertyBinding { siid: 4, piid: 2 }
    );
    assert_eq!(bindings.swing(), Some(PropertyBinding { siid: 4, piid: 4 }));
    assert_eq!(
        bindings.mode().read_value(&json!(0)).unwrap(),
        AirConditionerMode::Cool
    );
    assert_eq!(bindings.mode().encode(AirConditionerMode::Dry), 4);
    assert_eq!(
        bindings.speed().read_value(&json!(0)).unwrap(),
        AirConditionerFanSpeed::Auto
    );
    assert_eq!(bindings.speed().encode(AirConditionerFanSpeed::High), 3);
    assert_eq!(bindings.temperature().read_value(&json!(26.0)).unwrap(), 26);
    assert_eq!(bindings.temperature().encode(26), json!(26.0));
    assert_eq!(bindings.bindings().len(), 5);
    assert!(bindings.temperature().read_value(&json!(26.5)).is_err());
}

fn parse_result(
    value: &Value,
) -> Result<super::super::model::AirConditionerBindings, super::super::CapabilityError> {
    let description = Description::parse(value).unwrap();
    parse_air_conditioner(&description, &mut Vec::new())
}

fn property(value: &mut Value, siid: u32, piid: u32) -> &mut Value {
    value["services"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|service| service["iid"] == siid)
        .unwrap()["properties"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|property| property["iid"] == piid)
        .unwrap()
}

#[test]
fn unrecognized_or_duplicate_enum_capabilities_are_not_trimmed() {
    for (siid, piid) in [(3, 2), (4, 2)] {
        let mut extra = instance();
        property(&mut extra, siid, piid)["value-list"]
            .as_array_mut()
            .unwrap()
            .push(json!({"value": 99, "description": "Unexplained"}));
        assert!(
            parse_result(&extra).is_err(),
            "extra enumeration at {siid}.{piid}"
        );
        let mut duplicate = instance();
        let first = property(&mut duplicate, siid, piid)["value-list"][0].clone();
        property(&mut duplicate, siid, piid)["value-list"]
            .as_array_mut()
            .unwrap()
            .push(first);
        assert!(
            parse_result(&duplicate).is_err(),
            "duplicate enumeration at {siid}.{piid}"
        );
        let mut missing = instance();
        property(&mut missing, siid, piid)["value-list"]
            .as_array_mut()
            .unwrap()
            .pop();
        assert!(
            parse_result(&missing).is_err(),
            "missing enumeration at {siid}.{piid}"
        );
        let mut conflicting_shapes = instance();
        property(&mut conflicting_shapes, siid, piid)["value-range"] = json!([0, 4, 1]);
        assert!(
            parse_result(&conflicting_shapes).is_err(),
            "ambiguous enumeration at {siid}.{piid}"
        );
    }
}

#[test]
fn required_bindings_need_read_write_notify_and_valid_formats() {
    for (siid, piid) in [(3, 1), (3, 2), (3, 4), (4, 2)] {
        for permission in ["read", "write", "notify"] {
            let mut value = instance();
            property(&mut value, siid, piid)["access"] = json!(
                ["read", "write", "notify"]
                    .into_iter()
                    .filter(|item| *item != permission)
                    .collect::<Vec<_>>()
            );
            assert!(parse_result(&value).is_err(), "{siid}.{piid} {permission}");
        }
    }
    for (siid, piid, format) in [
        (3, 1, "uint8"),
        (3, 2, "float"),
        (4, 2, "float"),
        (3, 4, "bool"),
    ] {
        let mut value = instance();
        property(&mut value, siid, piid)["format"] = json!(format);
        assert!(parse_result(&value).is_err(), "{siid}.{piid} {format}");
    }
}

#[test]
fn temperature_requires_a_celsius_whole_degree_grid_and_one_declared_shape() {
    for bad in [
        json!([16, 30, 0.5]),
        json!([16.5, 30, 1]),
        json!([30, 16, 1]),
        json!([16, 30, 0]),
        json!([-274, 30, 1]),
        json!([16, 328, 1]),
    ] {
        let mut value = instance();
        property(&mut value, 3, 4)["value-range"] = bad.clone();
        assert!(parse_result(&value).is_err(), "{bad}");
    }
    let mut unit = instance();
    property(&mut unit, 3, 4)["unit"] = json!("fahrenheit");
    assert!(parse_result(&unit).is_err());
    let mut dual_shape = instance();
    property(&mut dual_shape, 3, 4)["value-list"] = json!([{ "value": 16, "description": "16" }]);
    assert!(parse_result(&dual_shape).is_err());
}

#[test]
fn optional_swing_requires_one_valid_boolean_binding() {
    let mut value = instance();
    property(&mut value, 4, 4)["access"] = json!(["read"]);
    let description = Description::parse(&value).unwrap();
    let mut diagnostics = Vec::new();
    let bindings = parse_air_conditioner(&description, &mut diagnostics).unwrap();
    assert_eq!(bindings.swing(), None);
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].target, "air-conditioner:3");
    let mut value = instance();
    let fan = value["services"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|s| s["iid"] == 4)
        .unwrap();
    fan["properties"]
        .as_array_mut()
        .unwrap()
        .retain(|p| p["iid"] != 4);
    let description = Description::parse(&value).unwrap();
    let mut diagnostics = Vec::new();
    let bindings = parse_air_conditioner(&description, &mut diagnostics).unwrap();
    assert_eq!(bindings.swing(), None);
    assert!(diagnostics.is_empty());
}

#[test]
fn native_codes_are_mapped_by_semantics_and_unknown_observations_are_rejected() {
    let mut value = instance();
    let mode = property(&mut value, 3, 2)["value-list"]
        .as_array_mut()
        .unwrap();
    for (item, code) in mode.iter_mut().zip([7, 20, 1, 6, 2]) {
        item["value"] = json!(code);
    }
    let speed = property(&mut value, 4, 2)["value-list"]
        .as_array_mut()
        .unwrap();
    for (item, code) in speed.iter_mut().zip([8, 1, 11, 2]) {
        item["value"] = json!(code);
    }
    let bindings = parse(&value);
    assert_eq!(bindings.mode().encode(AirConditionerMode::Cool), 7);
    assert_eq!(
        bindings.mode().read_value(&json!(6)).unwrap(),
        AirConditionerMode::Fan
    );
    assert_eq!(bindings.speed().encode(AirConditionerFanSpeed::Auto), 8);
    assert_eq!(
        bindings.speed().read_value(&json!(11)).unwrap(),
        AirConditionerFanSpeed::Medium
    );
    assert!(bindings.mode().read_value(&json!(99)).is_err());
    assert!(bindings.speed().read_value(&json!(0)).is_err());
}

#[test]
fn ambiguous_cross_service_ownership_is_rejected() {
    for siid in [3, 4] {
        let mut value = instance();
        let services = value["services"].as_array_mut().unwrap();
        let mut duplicate = services.iter().find(|s| s["iid"] == siid).unwrap().clone();
        duplicate["iid"] = json!(20);
        services.push(duplicate);
        assert!(parse_result(&value).is_err(), "duplicated service {siid}");
    }
}

#[test]
fn stored_bindings_must_keep_their_service_ownership_and_nonoverlapping_paths() {
    use super::super::model::{
        AirConditionerBindings, AirConditionerModeBinding, AirConditionerSpeedBinding,
        AirConditionerTemperatureBinding,
    };
    let valid = parse(&instance());
    let wrong_mode = AirConditionerModeBinding::new(
        PropertyBinding { siid: 9, piid: 2 },
        valid.mode().format(),
        valid.mode().codes(),
    )
    .unwrap();
    assert!(
        AirConditionerBindings::new(
            valid.power(),
            wrong_mode,
            *valid.temperature(),
            valid.speed().clone(),
            valid.swing()
        )
        .is_err()
    );
    let wrong_temperature = AirConditionerTemperatureBinding::new(
        PropertyBinding { siid: 9, piid: 4 },
        valid.temperature().format(),
        valid.temperature().grid(),
    )
    .unwrap();
    assert!(
        AirConditionerBindings::new(
            valid.power(),
            valid.mode().clone(),
            wrong_temperature,
            valid.speed().clone(),
            valid.swing()
        )
        .is_err()
    );
    let wrong_speed = AirConditionerSpeedBinding::new(
        PropertyBinding { siid: 3, piid: 5 },
        valid.speed().format(),
        valid.speed().codes(),
    )
    .unwrap();
    assert!(
        AirConditionerBindings::new(
            valid.power(),
            valid.mode().clone(),
            *valid.temperature(),
            wrong_speed,
            None
        )
        .is_err()
    );
    assert!(
        AirConditionerBindings::new(
            valid.power(),
            valid.mode().clone(),
            *valid.temperature(),
            valid.speed().clone(),
            Some(PropertyBinding { siid: 9, piid: 4 })
        )
        .is_err()
    );
    assert!(
        AirConditionerBindings::new(
            valid.power(),
            valid.mode().clone(),
            *valid.temperature(),
            valid.speed().clone(),
            Some(valid.speed().property())
        )
        .is_err()
    );
}

#[test]
fn binding_constructors_reject_invalid_native_codes_and_formats() {
    use super::super::{
        NumericFormat,
        model::{
            AirConditionerModeBinding, AirConditionerSpeedBinding, AirConditionerTemperatureBinding,
        },
    };
    use crate::device::air_conditioner::TemperatureGrid;
    let mode = PropertyBinding { siid: 3, piid: 2 };
    let speed = PropertyBinding { siid: 4, piid: 2 };
    assert!(AirConditionerModeBinding::new(mode, NumericFormat::Uint8, [0, 1, 2, 3, 256]).is_err());
    assert!(AirConditionerModeBinding::new(mode, NumericFormat::Uint8, [0, 1, 2, 3, 3]).is_err());
    assert!(AirConditionerModeBinding::new(mode, NumericFormat::Float, [0, 1, 2, 3, 4]).is_err());
    assert!(AirConditionerSpeedBinding::new(speed, NumericFormat::Uint8, [-1, 1, 2, 3]).is_err());
    assert!(AirConditionerSpeedBinding::new(speed, NumericFormat::Int8, [0, 1, 2, 128]).is_err());
    let temperature = PropertyBinding { siid: 3, piid: 4 };
    let grid = TemperatureGrid::new(-1, 30, 1).unwrap();
    assert!(
        AirConditionerTemperatureBinding::new(temperature, NumericFormat::Uint8, grid).is_err()
    );
    let integer =
        AirConditionerTemperatureBinding::new(temperature, NumericFormat::Int8, grid).unwrap();
    assert_eq!(integer.encode(26), json!(26));
    assert_eq!(integer.read_value(&json!(-1)).unwrap(), -1);
}
