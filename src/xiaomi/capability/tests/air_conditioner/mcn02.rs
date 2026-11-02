use crate::xiaomi::capability::{PropertyBinding, tests::source::SourceFixture};
use serde_json::{Value, json};

const BINDINGS: [PropertyBinding; 5] = [
    PropertyBinding { siid: 2, piid: 1 },
    PropertyBinding { siid: 2, piid: 2 },
    PropertyBinding { siid: 2, piid: 3 },
    PropertyBinding { siid: 3, piid: 1 },
    PropertyBinding { siid: 3, piid: 2 },
];

fn instance() -> Value {
    serde_json::from_str(include_str!(
        "../../../../../tests/fixtures/miot/lumi-mcn02-synthetic.json"
    ))
    .unwrap()
}

#[test]
fn mcn02_profile_is_derived_from_the_native_description_and_reported_as_core() {
    let instance = instance();
    let source = SourceFixture::new("one", "lumi.acpartner.mcn02", instance.clone());
    let profile = source.legacy_profile().unwrap();
    assert_eq!(profile.patch().id(), "lumi.mcn02.legacy-miio");
    assert_eq!(profile.patch().target(), "air-conditioner:2");
    assert_eq!(profile.patch().stage(), "direct transport");
    assert_eq!(
        source.device().recognition().unwrap().core(),
        ["air-conditioner:2"]
    );
    assert!(source.device().recognition().unwrap().patches().is_empty());

    let generic = SourceFixture::new("one", "other.air-conditioner", instance);
    assert!(generic.legacy_profile().is_none());
    assert_eq!(
        generic.device().recognition().unwrap().core(),
        ["air-conditioner:2"]
    );
}

#[test]
fn valid_device_urn_does_not_narrow_the_existing_mcn02_mapping() {
    let mut instance = instance();
    instance["type"] = json!("urn:miot-spec-v2:device:air-condition-outlet:0000A045:lumi-mcn02:1");
    let source = SourceFixture::new("one", "lumi.acpartner.mcn02", instance);
    assert!(source.legacy_profile().is_some());
}

#[test]
fn mcn02_mapping_converts_all_fields_both_directions() {
    let source = SourceFixture::new("one", "lumi.acpartner.mcn02", instance());
    let mapping = source.legacy_profile().unwrap().mapping();
    assert_eq!(
        mapping.fields(),
        ["power", "mode", "tar_temp", "fan_level", "ver_swing"]
    );
    for (binding, value, method, native) in [
        (BINDINGS[0], json!(true), "set_power", json!("on")),
        (BINDINGS[0], json!(false), "set_power", json!("off")),
        (BINDINGS[1], json!(0), "set_mode", json!("auto")),
        (BINDINGS[1], json!(1), "set_mode", json!("cool")),
        (BINDINGS[1], json!(2), "set_mode", json!("dry")),
        (BINDINGS[1], json!(3), "set_mode", json!("heat")),
        (BINDINGS[1], json!(4), "set_mode", json!("wind")),
        (BINDINGS[2], json!(16.0), "set_tar_temp", json!(16)),
        (BINDINGS[2], json!(30), "set_tar_temp", json!(30)),
        (BINDINGS[3], json!(0), "set_fan_level", json!("auto_fan")),
        (BINDINGS[3], json!(1), "set_fan_level", json!("small_fan")),
        (BINDINGS[3], json!(2), "set_fan_level", json!("medium_fan")),
        (BINDINGS[3], json!(3), "set_fan_level", json!("large_fan")),
        (BINDINGS[4], json!(true), "set_ver_swing", json!("on")),
        (BINDINGS[4], json!(false), "set_ver_swing", json!("off")),
    ] {
        assert_eq!(mapping.encode(binding, &value), Some((method, native)));
    }
    for (index, pairs) in [
        vec![(json!("off"), json!(false)), (json!("on"), json!(true))],
        vec![
            (json!("auto"), json!(0)),
            (json!("cool"), json!(1)),
            (json!("dry"), json!(2)),
            (json!("heat"), json!(3)),
            (json!("wind"), json!(4)),
        ],
        vec![(json!(16), json!(16)), (json!(30), json!(30))],
        vec![
            (json!("auto_fan"), json!(0)),
            (json!("small_fan"), json!(1)),
            (json!("medium_fan"), json!(2)),
            (json!("large_fan"), json!(3)),
        ],
        vec![(json!("off"), json!(false)), (json!("on"), json!(true))],
    ]
    .into_iter()
    .enumerate()
    {
        for (native, expected) in pairs {
            let mut input = json!(["on", "cool", 24, "small_fan", "off"]);
            input[index] = native;
            assert_eq!(mapping.decode(&input).unwrap()[index], Ok(expected));
        }
    }
}

#[test]
fn invalid_shapes_types_enums_and_temperatures_are_not_defaulted() {
    let source = SourceFixture::new("one", "lumi.acpartner.mcn02", instance());
    let mapping = source.legacy_profile().unwrap().mapping();
    for input in [
        json!(null),
        json!({}),
        json!([]),
        json!(["on", "cool", 24, "small_fan"]),
        json!(["on", "cool", 24, "small_fan", "off", 1]),
    ] {
        assert!(mapping.decode(&input).is_err());
    }
    for (index, bad) in [
        (0, json!("unknown")),
        (0, json!(false)),
        (1, json!("mystery")),
        (1, json!(0)),
        (2, json!(15)),
        (2, json!(31)),
        (2, json!(24.5)),
        (2, json!("24")),
        (3, json!("mystery")),
        (4, json!("unknown")),
    ] {
        let mut input = json!(["on", "cool", 24, "small_fan", "off"]);
        input[index] = bad;
        let values = mapping.decode(&input).unwrap();
        assert_eq!(values[index], Err(()));
        assert_eq!(values.iter().filter(|value| value.is_err()).count(), 1);
    }
    for (binding, value) in [
        (BINDINGS[0], json!(0)),
        (BINDINGS[1], json!(5)),
        (BINDINGS[2], json!(24.5)),
        (BINDINGS[2], json!(15)),
        (BINDINGS[3], json!(-1)),
        (BINDINGS[4], json!("off")),
    ] {
        assert!(mapping.encode(binding, &value).is_none());
    }
}
