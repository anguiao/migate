use super::*;

#[test]
fn on_off_and_dimmable_profiles_persist_static_capabilities() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let lights = [("switch", false), ("dimmer", true)]
        .into_iter()
        .map(|(id, brightness)| {
            let mut instance: Value = serde_json::from_str(CEILING).unwrap();
            instance["services"][1]["properties"]
                .as_array_mut()
                .unwrap()
                .retain(|property| {
                    let kind = property["type"].as_str().unwrap();
                    kind.contains(":on:") || (brightness && kind.contains(":brightness:"))
                });
            native(id, "xiaomi.light.ceil04", instance)
        })
        .collect();
    let expected = seed(&store, lights);
    drop(store);
    let restored = load(&Store::open(dir.path()).unwrap());
    assert_eq!(restored, expected);
    for (id, profile) in [
        ("switch", LightProfile::OnOff),
        ("dimmer", LightProfile::Dimmable),
    ] {
        assert_eq!(
            restored
                .device(&DeviceKey::new("xiaomi", id))
                .unwrap()
                .native
                .as_ref()
                .unwrap()
                .device()
                .functions()[0]
                .kind(),
            FunctionKind::Light(profile)
        );
    }
}

#[test]
fn all_seven_lights_restore_complete_static_bindings_and_identities() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let fixtures = [
        ("devcea.light.ls2307", LS2307),
        (
            "yeelink.light.light3",
            include_str!("../../../../tests/fixtures/miot/yeelink-light-light3.json"),
        ),
        (
            "yeelink.light.ml9",
            include_str!("../../../../tests/fixtures/miot/yeelink-light-ml9.json"),
        ),
        (
            "yeelink.light.spot2",
            include_str!("../../../../tests/fixtures/miot/yeelink-light-spot2.json"),
        ),
        ("xiaomi.light.ceil04", CEILING),
        (
            "xiaomi.light.bar2",
            include_str!("../../../../tests/fixtures/miot/xiaomi-light-bar2.json"),
        ),
        (
            "lemesh.light.wy0c15",
            include_str!("../../../../tests/fixtures/miot/lemesh-light-wy0c15.json"),
        ),
    ];
    let expected = seed(
        &store,
        fixtures
            .into_iter()
            .map(|(model, data)| fixture(model, model, data))
            .collect(),
    );
    for device in &expected.devices {
        assert_eq!(device.functions.len(), 1);
        assert_eq!(device.functions[0].id, "light:2");
        assert_ne!(device.functions[0].endpoint, device.endpoint);
    }
    drop(store);
    assert_eq!(load(&Store::open(dir.path()).unwrap()), expected);
}

#[test]
fn corrupted_ls2307_binding_or_mode_conversion_cannot_be_restored() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    seed(
        &store,
        vec![fixture("light", "devcea.light.ls2307", LS2307)],
    );
    let valid: Value = serde_json::from_str(&raw(&store)).unwrap();
    for (pointer, replacement) in [
        (
            "/devices/0/native/functions/0/lighting/color/property/piid",
            json!(8),
        ),
        (
            "/devices/0/native/functions/0/lighting/mode/rgb_value",
            json!(9),
        ),
        (
            "/devices/0/native/instance_type",
            json!("urn:miot-spec-v2:device:light:0000A001:unknown-v1:1"),
        ),
        (
            "/devices/0/native/functions/0/lighting/brightness/step",
            json!(0),
        ),
    ] {
        let mut changed = valid.clone();
        *changed.pointer_mut(pointer).unwrap() = replacement;
        corrupt(&store, &changed);
        assert!(store.devices().load_configuration().is_err(), "{pointer}");
    }
}

#[test]
fn generic_light_cannot_restore_an_unrelated_service_as_brightness() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    seed(
        &store,
        vec![fixture("light", "xiaomi.light.ceil04", CEILING)],
    );
    let mut encoded: Value = serde_json::from_str(&raw(&store)).unwrap();
    encoded["devices"][0]["native"]["functions"][0]["lighting"]["brightness"]["property"]["siid"] =
        json!(40);
    corrupt(&store, &encoded);
    assert!(store.devices().load_configuration().is_err());
}

#[test]
fn new_description_and_version_keep_identity_names_and_endpoints() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let mut original = seed(
        &store,
        vec![fixture("light", "xiaomi.light.ceil04", CEILING)],
    );
    original.devices[0].label = "Saved light".into();
    save(&store, &original);
    let mut instance: Value = serde_json::from_str(CEILING).unwrap();
    instance["type"] = json!("urn:miot-spec-v2:device:light:0000A001:xiaomi-ceil04:2");
    for property in instance["services"][1]["properties"]
        .as_array_mut()
        .unwrap()
    {
        if property["type"]
            .as_str()
            .unwrap()
            .contains(":color-temperature:")
        {
            property["value-range"][1] = json!(5600);
        }
    }
    let plan = next(
        &original,
        scope("account", "home"),
        vec![native("light", "xiaomi.light.ceil04", instance)],
    );
    assert!(plan.rebuild);
    assert_eq!(
        plan.configuration.devices[0].configuration_version,
        original.devices[0].configuration_version + 1
    );
    assert_eq!(plan.configuration.root_version, original.root_version + 1);
    save(&store, &plan.configuration);
    drop(store);
    let restored = load(&Store::open(dir.path()).unwrap());
    assert_eq!(restored, plan.configuration);
    let (before, after) = (&original.devices[0], &restored.devices[0]);
    assert_eq!(after.unique_id, before.unique_id);
    assert_eq!(after.label, before.label);
    assert_eq!(after.endpoint, before.endpoint);
    assert_eq!(after.functions, before.functions);
}

#[test]
fn ls2307_identity_does_not_discard_an_independent_outlet() {
    let instance: Value = serde_json::from_str(OUTLET).unwrap();
    let parsed = parse_device_for_model(
        DeviceKey::new("xiaomi", "wrong"),
        "devcea.light.ls2307",
        &instance,
    )
    .unwrap();
    assert!(
        parsed
            .functions()
            .iter()
            .any(|function| function.id() == "outlet:2")
    );
    assert!(
        parsed
            .recognition()
            .unwrap()
            .diagnostics()
            .iter()
            .any(|issue| issue.reason().contains("devcea.ls2307.rgb"))
    );
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    seed(&store, vec![fixture("wrong", "cuco.plug.v3", OUTLET)]);
    let mut encoded: Value = serde_json::from_str(&raw(&store)).unwrap();
    encoded["devices"][0]["native"]["model"] = json!("devcea.light.ls2307");
    encoded["descriptions"][0]["model"] = json!("devcea.light.ls2307");
    corrupt(&store, &encoded);
    assert!(store.devices().load_configuration().is_ok());
}

#[test]
fn unrepresentable_mired_ranges_are_rejected_at_construction_and_cache_restore() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    seed(
        &store,
        vec![fixture("light", "xiaomi.light.ceil04", CEILING)],
    );
    let valid: Value = serde_json::from_str(&raw(&store)).unwrap();
    for range in [[4500, 4501, 1], [10, 6500, 1]] {
        let mut instance: Value = serde_json::from_str(CEILING).unwrap();
        let temperature = instance["services"][1]["properties"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|property| {
                property["type"]
                    .as_str()
                    .unwrap()
                    .contains(":color-temperature:")
            })
            .unwrap();
        temperature["value-range"] = json!(range);
        let parsed = parse_device_for_model(
            DeviceKey::new("xiaomi", "bad-range"),
            "xiaomi.light.ceil04",
            &instance,
        )
        .unwrap();
        assert!(
            parsed.functions()[0]
                .lighting()
                .unwrap()
                .temperature()
                .is_none()
        );
        assert!(!parsed.recognition().unwrap().diagnostics().is_empty());
        let mut encoded = valid.clone();
        let temperature =
            &mut encoded["devices"][0]["native"]["functions"][0]["lighting"]["temperature"];
        temperature["min"] = json!(range[0]);
        temperature["max"] = json!(range[1]);
        temperature["step"] = json!(range[2]);
        corrupt(&store, &encoded);
        assert!(store.devices().load_configuration().is_err());
    }
}

#[test]
fn adding_light_capabilities_keeps_its_function_and_endpoint_after_restart() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let expanded: Value = serde_json::from_str(CEILING).unwrap();
    let mut basic = expanded.clone();
    basic["services"][1]["properties"]
        .as_array_mut()
        .unwrap()
        .retain(|property| property["type"].as_str().unwrap().contains(":on:"));
    let initial = seed(&store, vec![native("light", "xiaomi.light.ceil04", basic)]);
    let before = &initial.devices[0];
    assert_eq!(before.functions[0].id, "light:2");
    assert_ne!(before.endpoint, before.functions[0].endpoint);
    let plan = next(
        &initial,
        scope("account", "home"),
        vec![native("light", "xiaomi.light.ceil04", expanded)],
    );
    assert!(plan.rebuild);
    assert_eq!(plan.configuration.devices[0].functions, before.functions);
    assert_eq!(plan.configuration.devices[0].endpoint, before.endpoint);
    save(&store, &plan.configuration);
    drop(store);
    let restored = load(&Store::open(dir.path()).unwrap());
    assert_eq!(restored.devices[0].functions, before.functions);
    assert_eq!(restored.devices[0].endpoint, before.endpoint);
    let unchanged = next(
        &restored,
        scope("account", "home"),
        vec![SourceFixture::committed(
            &restored,
            &restored.devices[0].key.id,
        )],
    );
    assert!(!unchanged.changed);
}
