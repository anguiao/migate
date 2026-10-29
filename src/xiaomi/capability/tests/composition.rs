use super::*;

fn light() -> Value {
    let instance: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/xiaomi-light-ceil04.json"
    ))
    .unwrap();
    instance["services"][1].clone()
}

fn fan() -> Value {
    json!({
        "iid": 3,
        "type": "urn:miot-spec-v2:service:fan:00007808:example:1",
        "properties": [
            {"iid": 1, "type": "urn:miot-spec-v2:property:on:00000006:example:1", "format": "bool", "access": ["read", "write", "notify"]},
            {"iid": 2, "type": "urn:miot-spec-v2:property:fan-level:00000016:example:1", "format": "uint8", "access": ["read", "write", "notify"], "value-range": [1, 3, 1]}
        ]
    })
}

fn description(services: Vec<Value>) -> Value {
    json!({
        "type": "urn:miot-spec-v2:device:air-conditioner:0000A004:example:1",
        "services": services
    })
}

#[test]
fn unknown_category_exposes_light_fan_and_independent_lights() {
    let mut second = light();
    second["iid"] = json!(4);
    let parsed = super::super::parse_device_for_model(
        key(),
        "example.composite",
        &description(vec![light(), fan(), second]),
    )
    .unwrap();
    assert_eq!(
        parsed
            .functions()
            .iter()
            .map(|f| f.id())
            .collect::<Vec<_>>(),
        ["light:2", "fan:3", "light:4"]
    );
}

#[test]
fn invalid_control_does_not_exclude_an_independent_light() {
    let mut invalid = fan();
    invalid["properties"][1]["access"] = json!(["read"]);
    let parsed = super::super::parse_device_for_model(
        key(),
        "example.composite",
        &description(vec![light(), invalid]),
    )
    .unwrap();
    assert_eq!(
        parsed
            .functions()
            .iter()
            .map(|f| f.id())
            .collect::<Vec<_>>(),
        ["light:2"]
    );
}

#[test]
fn controls_can_share_the_physical_battery() {
    let source: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/miaomiaoce-t6.json"
    ))
    .unwrap();
    let mut battery = source["services"]
        .as_array()
        .unwrap()
        .iter()
        .find(|service| {
            service["type"]
                .as_str()
                .unwrap()
                .contains(":service:battery:")
        })
        .unwrap()
        .clone();
    battery["iid"] = json!(8);
    let parsed = super::super::parse_device_for_model(
        key(),
        "example.composite",
        &description(vec![light(), battery]),
    )
    .unwrap();
    assert_eq!(parsed.functions().len(), 1);
    assert_eq!(parsed.battery().unwrap().property().siid, 8);
}

#[test]
fn independent_measurement_contact_and_motion_services_are_all_kept() {
    let fixture = |text: &str| serde_json::from_str::<Value>(text).unwrap();
    let sensor = fixture(include_str!(
        "../../../../tests/fixtures/miot/miaomiaoce-t6.json"
    ));
    let contact = fixture(include_str!(
        "../../../../tests/fixtures/miot/isa-dw2hl.json"
    ));
    let motion = fixture(include_str!(
        "../../../../tests/fixtures/miot/xiaomi-pir1.json"
    ));
    let mut services = Vec::new();
    for (fixture, index, first, second) in [
        (sensor, 2, 10, 11),
        (contact, 1, 12, 13),
        (motion, 1, 14, 15),
    ] {
        let mut service = fixture["services"][index].clone();
        service["iid"] = json!(first);
        services.push(service.clone());
        service["iid"] = json!(second);
        services.push(service);
    }
    let parsed =
        super::super::parse_device_for_model(key(), "example.composite", &description(services))
            .unwrap();
    let ids = parsed
        .functions()
        .iter()
        .map(|f| f.id())
        .collect::<Vec<_>>();
    for id in [
        "temperature:10:1001",
        "temperature:11:1001",
        "humidity:10:1008",
        "humidity:11:1008",
        "contact:12:2",
        "contact:13:2",
        "motion:14:1024",
        "motion:15:1024",
    ] {
        assert!(ids.contains(&id), "missing {id}: {ids:?}");
    }
}

#[test]
fn occupancy_uses_only_the_first_standard_service_even_if_it_is_invalid() {
    let mut source: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/xiaomi-occupy-p1.json"
    ))
    .unwrap();
    let parsed =
        super::super::parse_device_for_model(key(), "xiaomi.sensor_occupy.p1", &source).unwrap();
    assert_eq!(
        parsed
            .functions()
            .iter()
            .filter(|f| matches!(f.kind(), FunctionKind::Occupancy))
            .map(|f| f.id())
            .collect::<Vec<_>>(),
        ["occupancy:2:1"]
    );
    source["services"][1]["properties"][0]["access"] = json!(["read"]);
    let parsed =
        super::super::parse_device_for_model(key(), "xiaomi.sensor_occupy.p1", &source).unwrap();
    assert!(
        !parsed
            .functions()
            .iter()
            .any(|f| matches!(f.kind(), FunctionKind::Occupancy))
    );
}

#[test]
fn recognition_metadata_identifies_category_core_and_failed_candidate() {
    let mut invalid = fan();
    invalid["properties"][1]["access"] = json!(["read"]);
    let mut input = description(vec![light(), invalid]);
    input["type"] = json!("urn:miot-spec-v2:device:light:0000A001:example:1");
    let parsed = super::super::parse_device_for_model(key(), "example", &input).unwrap();
    let debug = format!("{parsed:?}");
    assert!(debug.contains("category: \"light\""), "{debug}");
    assert!(debug.contains("core: [\"light:2\"]"), "{debug}");
    assert!(
        debug.contains("fan:3") && debug.contains("write"),
        "{debug}"
    );
}

#[test]
fn invalid_optional_rgb_preserves_the_independent_temperature_light() {
    let mut service = light();
    service["properties"].as_array_mut().unwrap().push(json!({
        "iid": 4, "type": "urn:miot-spec-v2:property:color:0000000E:example:1",
        "format": "uint32", "access": ["read", "notify"], "unit": "rgb",
        "value-range": [0, 16777215, 1]
    }));
    let parsed =
        super::super::parse_device_for_model(key(), "example", &description(vec![service]))
            .unwrap();
    assert!(
        parsed.functions()[0]
            .lighting()
            .unwrap()
            .temperature()
            .is_some()
    );
    assert!(parsed.functions()[0].lighting().unwrap().color().is_none());
    assert!(format!("{parsed:?}").contains("write"));
}

#[test]
fn cp7pd_channel_failure_does_not_turn_master_into_an_outlet() {
    let mut input = model_description("cuco.plug.cp7pd");
    cp7pd_service_mut(&mut input, 3)["properties"][0]["access"] = json!(["read"]);
    let parsed = super::super::parse_device_for_model(key(), "cuco.plug.cp7pd", &input).unwrap();
    assert_eq!(
        parsed
            .functions()
            .iter()
            .map(|f| f.id())
            .collect::<Vec<_>>(),
        ["outlet:4", "outlet:9"]
    );
    assert_eq!(
        parsed.refresh_trigger(),
        Some(PropertyBinding { siid: 2, piid: 1 })
    );
    assert!(format!("{parsed:?}").contains("outlet:3"));
    super::super::NativeDescription::new(
        "cuco.plug.cp7pd".into(),
        input["type"].as_str().unwrap().into(),
        parsed,
    )
    .unwrap();
}

#[test]
fn shared_control_binding_conflict_removes_both_claimants_only() {
    let mut input: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/dmaker-p5c.json"
    ))
    .unwrap();
    let mut second = input["services"]
        .as_array()
        .unwrap()
        .iter()
        .find(|service| service["iid"] == 2)
        .unwrap()
        .clone();
    second["iid"] = json!(10);
    let mut independent = light();
    independent["iid"] = json!(11);
    input["services"]
        .as_array_mut()
        .unwrap()
        .extend([second, independent]);
    let parsed = super::super::parse_device_for_model(key(), "dmaker.fan.p5c", &input).unwrap();
    assert_eq!(
        parsed
            .functions()
            .iter()
            .map(|f| f.id())
            .collect::<Vec<_>>(),
        ["light:11"]
    );
    let issues = parsed.recognition().unwrap().diagnostics();
    for id in ["fan:2", "fan:10"] {
        assert!(
            issues.iter().any(|issue| issue.target() == id
                && issue.reason().contains("8.1")
                && issue.reason().contains("ambiguous")),
            "{issues:?}"
        );
    }
    assert!(parsed.recognition().unwrap().patches().is_empty());
}

#[test]
fn invalid_optional_swing_and_motion_report_specific_access_failures() {
    let mut fan = fan();
    fan["properties"].as_array_mut().unwrap().push(json!({
        "iid": 3, "type": "urn:miot-spec-v2:property:horizontal-swing:00000017:example:1",
        "format": "bool", "access": ["read", "write"]
    }));
    let source: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/xiaomi-pir1.json"
    ))
    .unwrap();
    let mut motion = source["services"][1].clone();
    motion["iid"] = json!(9);
    motion["properties"][0]["access"] = json!(["read"]);
    let parsed =
        super::super::parse_device_for_model(key(), "example", &description(vec![fan, motion]))
            .unwrap();
    assert!(
        parsed
            .functions()
            .iter()
            .any(|f| f.id() == "fan:3" && f.fan().unwrap().swing().is_none())
    );
    let issues = parsed.recognition().unwrap().diagnostics();
    for id in ["fan:3", "motion:9"] {
        assert!(
            issues
                .iter()
                .any(|issue| issue.target() == id && issue.reason().contains("notify")),
            "{issues:?}"
        );
    }
}

#[test]
fn a_dmaker_identity_with_an_unrelated_generic_fan_does_not_claim_the_wind_patch() {
    let mut invalid = fan();
    invalid["properties"][0]["access"] = json!(["read"]);
    let mut input = description(vec![light(), invalid]);
    input["type"] = json!("urn:miot-spec-v2:device:fan:0000A005:dmaker-other:1");
    let parsed = super::super::parse_device_for_model(key(), "dmaker.fan.other", &input).unwrap();
    let issue = parsed
        .recognition()
        .unwrap()
        .diagnostics()
        .iter()
        .find(|issue| issue.target() == "fan:3")
        .unwrap();
    assert!(issue.reason().contains("write"));
    assert!(!issue.reason().contains("patch"), "{}", issue.reason());
}

#[test]
fn invalid_source_identity_rejects_the_complete_description() {
    for key in [
        DeviceKey::new("", "device"),
        DeviceKey::new("xiaomi", ""),
        DeviceKey::new("xiaomi", "bad\nidentity"),
    ] {
        assert!(
            super::super::parse_device_for_model(key, "example", &description(vec![light()]))
                .is_err()
        );
    }
}

#[test]
fn zero_valid_candidates_reports_the_specific_candidate_without_creating_a_device() {
    let mut invalid = light();
    invalid["properties"][0]["access"] = json!(["read"]);
    let error = super::super::parse_device_for_model(key(), "example", &description(vec![invalid]))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("light:2") && error.contains("write"),
        "{error}"
    );
}

#[test]
fn cp7pd_refresh_patch_is_reported_only_when_master_binding_is_valid() {
    let mut input = model_description("cuco.plug.cp7pd");
    let parsed = super::super::parse_device_for_model(key(), "cuco.plug.cp7pd", &input).unwrap();
    assert!(
        parsed
            .recognition()
            .unwrap()
            .patches()
            .iter()
            .any(|patch| patch.id() == "cuco.cp7pd.refresh")
    );
    cp7pd_service_mut(&mut input, 2)["properties"][0]["access"] = json!(["read"]);
    let parsed = super::super::parse_device_for_model(key(), "cuco.plug.cp7pd", &input).unwrap();
    assert!(
        parsed
            .recognition()
            .unwrap()
            .patches()
            .iter()
            .any(|patch| patch.id() == "cuco.cp7pd.channels")
    );
    assert!(
        !parsed
            .recognition()
            .unwrap()
            .patches()
            .iter()
            .any(|patch| patch.id() == "cuco.cp7pd.refresh")
    );
    assert!(
        parsed
            .recognition()
            .unwrap()
            .diagnostics()
            .iter()
            .any(|issue| issue.target() == "cp7pd master" && issue.reason().contains("write"))
    );
}

#[test]
fn invalid_alternative_speed_is_diagnosed_while_the_valid_input_is_used() {
    let mut fan = fan();
    let mut invalid = fan["properties"][1].clone();
    invalid["iid"] = json!(4);
    invalid["access"] = json!(["read"]);
    fan["properties"].as_array_mut().unwrap().push(invalid);
    let parsed =
        super::super::parse_device_for_model(key(), "example", &description(vec![fan])).unwrap();
    assert_eq!(
        parsed.functions()[0].fan().unwrap().speed().property(),
        PropertyBinding { siid: 3, piid: 2 }
    );
    assert!(
        parsed
            .recognition()
            .unwrap()
            .diagnostics()
            .iter()
            .any(|issue| issue.target() == "fan:3"
                && issue.reason().contains("3.4")
                && issue.reason().contains("write"))
    );
}

#[test]
fn incompatible_ls2307_patch_identity_does_not_block_the_standard_light() {
    let mut input: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/devcea-light-ls2307.json"
    ))
    .unwrap();
    input["type"] = json!("urn:miot-spec-v2:device:air-conditioner:0000A004:devcea-ls2307:1");
    let parsed =
        super::super::parse_device_for_model(key(), "devcea.light.ls2307", &input).unwrap();
    assert!(parsed.functions()[0].lighting().unwrap().color().is_none());
    assert!(
        parsed
            .recognition()
            .unwrap()
            .diagnostics()
            .iter()
            .any(|issue| issue.reason().contains("devcea.ls2307.rgb"))
    );
    super::super::NativeDescription::new(
        "devcea.light.ls2307".into(),
        input["type"].as_str().unwrap().into(),
        parsed,
    )
    .unwrap();
}

#[test]
fn cp7pd_patch_preserves_an_additional_standard_outlet_channel() {
    let mut input = model_description("cuco.plug.cp7pd");
    input["services"].as_array_mut().unwrap().push(json!({
        "iid": 42, "type": "urn:miot-spec-v2:service:switch:0000780C:example:1", "properties": [
            {"iid": 1, "type": "urn:miot-spec-v2:property:on:00000006:example:1", "format": "bool", "access": ["read", "write", "notify"]}
        ]
    }));
    let parsed = super::super::parse_device_for_model(key(), "cuco.plug.cp7pd", &input).unwrap();
    assert_eq!(
        parsed
            .functions()
            .iter()
            .map(|f| f.id())
            .collect::<Vec<_>>(),
        ["outlet:3", "outlet:4", "outlet:9", "outlet:42"]
    );
    assert!(
        parsed
            .recognition()
            .unwrap()
            .patches()
            .iter()
            .all(|patch| patch.target() != "outlet:42")
    );
}

#[test]
fn cp7pd_instance_alone_does_not_apply_the_model_specific_patch() {
    let instance = model_description("cuco.plug.cp7pd");
    assert!(super::super::parse_device_for_model(key(), "", &instance).is_err());
}
