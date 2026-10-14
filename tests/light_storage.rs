use migate::{
    device::{
        Capability, Category, DeviceKey, LightMode, LightPatch, NumericRange, PhysicalDevice,
    },
    storage::{AdmissionError, CachedPhysicalDevice, Store},
    xiaomi::{
        capability::{
            NumericBinding, NumericFormat, NumericUnit, PropertyBinding, parse_device_for_model,
        },
        cloud::group_id,
    },
};
use serde_json::Value;

fn light(model: &str, fixture: &str) -> CachedPhysicalDevice {
    let instance: Value = serde_json::from_str(fixture).unwrap();
    CachedPhysicalDevice {
        uid: "account".into(),
        home_id: "home".into(),
        group_id: group_id("account", "home"),
        model: model.into(),
        instance_type: instance["type"].as_str().unwrap().into(),
        device: parse_device_for_model(DeviceKey::new("xiaomi", model), model, &instance).unwrap(),
    }
}

#[test]
fn all_seven_lights_restore_complete_static_bindings_one_endpoint_and_unknown_runtime() {
    let directory = tempfile::tempdir().unwrap();
    let models = [
        (
            "devcea.light.ls2307",
            include_str!("fixtures/miot/devcea-light-ls2307.json"),
        ),
        (
            "yeelink.light.light3",
            include_str!("fixtures/miot/yeelink-light-light3.json"),
        ),
        (
            "yeelink.light.ml9",
            include_str!("fixtures/miot/yeelink-light-ml9.json"),
        ),
        (
            "yeelink.light.spot2",
            include_str!("fixtures/miot/yeelink-light-spot2.json"),
        ),
        (
            "xiaomi.light.ceil04",
            include_str!("fixtures/miot/xiaomi-light-ceil04.json"),
        ),
        (
            "xiaomi.light.bar2",
            include_str!("fixtures/miot/xiaomi-light-bar2.json"),
        ),
        (
            "lemesh.light.wy0c15",
            include_str!("fixtures/miot/lemesh-light-wy0c15.json"),
        ),
    ];
    let mut saved = Vec::new();
    {
        let store = Store::open(directory.path()).unwrap();
        for (model, fixture) in models {
            let cached = light(model, fixture);
            let identity = store.devices().publish_cached(&cached, model).unwrap();
            assert_eq!(identity.functions.len(), 1);
            assert_eq!(identity.functions[0].key.id, "main");
            assert_eq!(identity.endpoint, identity.functions[0].endpoint);
            let (device, _) = PhysicalDevice::new(identity.description());
            device.functions()[0]
                .observe_light(LightPatch {
                    power: Some(true),
                    brightness_percent: Some(72.5),
                    temperature_kelvin: Some(4000),
                    ..LightPatch::default()
                })
                .unwrap();
            saved.push((cached, identity));
        }
    }
    let store = Store::open(directory.path()).unwrap();
    let restored = store.devices().restore_cached("account", "home").unwrap();
    assert_eq!(restored, saved);
    for (cached, identity) in restored {
        let (device, _) = PhysicalDevice::new(identity.description());
        assert_eq!(
            device.functions()[0].snapshot().power,
            migate::device::Power::Unknown
        );
        let snapshot = device.functions()[0].snapshot().lighting.unwrap();
        assert_eq!(snapshot.brightness_percent, None);
        assert_eq!(snapshot.temperature_kelvin, None);
        assert_eq!(snapshot.color, None);
        assert_eq!(
            snapshot.mode,
            if cached.model == "devcea.light.ls2307" {
                None
            } else {
                Some(LightMode::Temperature)
            }
        );
    }
}

#[test]
fn compatible_binding_updates_keep_identity_and_static_capabilities() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let mut cached = light(
        "xiaomi.light.ceil04",
        include_str!("fixtures/miot/xiaomi-light-ceil04.json"),
    );
    let original = store.devices().publish_cached(&cached, "Original").unwrap();
    let brightness = cached.device.functions[0]
        .lighting
        .as_mut()
        .unwrap()
        .brightness
        .as_mut()
        .unwrap();
    brightness.property.piid = 8;
    // A different raw encoding still describes the same normalized brightness capability.
    brightness.range.max = 200;
    let changed = store
        .devices()
        .publish_cached(&cached, "Changed cloud name")
        .unwrap();
    assert_eq!(changed, original);
    let restored = store.devices().restore_cached("account", "home").unwrap();
    assert_eq!(restored, vec![(cached.clone(), original.clone())]);

    let mut incompatible = original.description();
    incompatible.functions[0].category = Category::Outlet;
    incompatible.functions[0].capabilities = vec![Capability::OnOff];
    assert!(matches!(
        store.devices().publish(&incompatible),
        Err(AdmissionError::IncompatibleFunctions)
    ));
}

#[test]
fn capability_reordering_is_not_an_identity_or_cache_change() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let mut cached = light(
        "xiaomi.light.ceil04",
        include_str!("fixtures/miot/xiaomi-light-ceil04.json"),
    );
    let original = store.devices().publish_cached(&cached, "Original").unwrap();
    cached.device.functions[0]
        .description
        .capabilities
        .reverse();
    let reordered = store.devices().publish_cached(&cached, "Reorder").unwrap();
    assert_eq!(reordered.unique_id, original.unique_id);
    assert_eq!(reordered.endpoint, original.endpoint);
    assert_eq!(reordered.label, original.label);
    let restored = store.devices().restore_cached("account", "home").unwrap();
    assert_eq!(restored.len(), 1);
    assert_eq!(restored[0].1.functions[0].capabilities.len(), 3);
    for capability in &original.functions[0].capabilities {
        assert!(
            restored[0].1.functions[0].capabilities.contains(capability),
            "{capability:?}"
        );
    }
}

#[test]
fn structurally_invalid_stored_light_bindings_cannot_be_restored() {
    use rusqlite::Connection;
    for mutation in [
        "brightness-range",
        "brightness-conversion",
        "color-range",
        "duplicate-binding",
        "outlet-instance",
        "capability-mismatch",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let cached = light(
            "devcea.light.ls2307",
            include_str!("fixtures/miot/devcea-light-ls2307.json"),
        );
        store
            .devices()
            .publish_cached(&cached, "RGB light")
            .unwrap();
        let db = Connection::open(store.path()).unwrap();
        if mutation == "outlet-instance" {
            db.execute("UPDATE device_cache SET instance_type='urn:miot-spec-v2:device:outlet:0000A002:unknown-v1:1'", []).unwrap();
        } else {
            let mut functions: Value = serde_json::from_str(
                &db.query_row("SELECT functions FROM device_cache", [], |row| {
                    row.get::<_, String>(0)
                })
                .unwrap(),
            )
            .unwrap();
            match mutation {
                "brightness-range" => {
                    functions[0]["lighting"]["brightness"]["range"]["step"] = serde_json::json!(0)
                }
                "brightness-conversion" => {
                    functions[0]["lighting"]["brightness_conversion"] = serde_json::Value::Null
                }
                "color-range" => {
                    functions[0]["lighting"]["color"]["range"]["max"] = serde_json::json!(100)
                }
                "duplicate-binding" => {
                    functions[0]["lighting"]["temperature"]["property"]["piid"] =
                        serde_json::json!(2)
                }
                "capability-mismatch" => {
                    functions[0]["description"]["capabilities"] = serde_json::json!(["on_off", "brightness", {"color_temperature":{"min":2700,"max":6500,"step":1}}])
                }
                _ => unreachable!(),
            }
            db.execute(
                "UPDATE device_cache SET functions=?1",
                [functions.to_string()],
            )
            .unwrap();
        }
        assert!(
            store.devices().restore_cached("account", "home").is_err(),
            "{mutation}"
        );
    }
}

#[test]
fn adapter_specific_ls2307_constants_are_not_re_admitted_by_storage() {
    use rusqlite::Connection;
    for mutation in ["color-binding", "mode-values"] {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let cached = light(
            "devcea.light.ls2307",
            include_str!("fixtures/miot/devcea-light-ls2307.json"),
        );
        store
            .devices()
            .publish_cached(&cached, "RGB light")
            .unwrap();
        let db = Connection::open(store.path()).unwrap();
        let mut functions: Value = serde_json::from_str(
            &db.query_row("SELECT functions FROM device_cache", [], |row| {
                row.get::<_, String>(0)
            })
            .unwrap(),
        )
        .unwrap();
        match mutation {
            "color-binding" => {
                functions[0]["lighting"]["color"]["property"]["piid"] = serde_json::json!(8)
            }
            "mode-values" => functions[0]["lighting"]["mode"]["rgb_value"] = serde_json::json!(9),
            _ => unreachable!(),
        }
        db.execute(
            "UPDATE device_cache SET functions=?1",
            [functions.to_string()],
        )
        .unwrap();
        let restored = store.devices().restore_cached("account", "home").unwrap();
        assert_eq!(restored.len(), 1, "{mutation}");
    }
}

#[test]
fn cached_and_identity_temperature_ranges_must_match_on_restore() {
    use rusqlite::Connection;
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let cached = light(
        "xiaomi.light.ceil04",
        include_str!("fixtures/miot/xiaomi-light-ceil04.json"),
    );
    store.devices().publish_cached(&cached, "Light").unwrap();
    let db = Connection::open(store.path()).unwrap();
    let mut capabilities: Value = serde_json::from_str(
        &db.query_row("SELECT capabilities FROM device_function", [], |row| {
            row.get::<_, String>(0)
        })
        .unwrap(),
    )
    .unwrap();
    capabilities[2]["color_temperature"]["max"] = serde_json::json!(5600);
    db.execute(
        "UPDATE device_function SET capabilities=?1",
        [capabilities.to_string()],
    )
    .unwrap();
    assert!(store.devices().restore_cached("account", "home").is_err());
}

#[test]
fn incompatible_capabilities_and_physical_ranges_preserve_cache_and_identity() {
    for change in ["add", "remove", "range"] {
        let directory = tempfile::tempdir().unwrap();
        let cached = light(
            "xiaomi.light.ceil04",
            include_str!("fixtures/miot/xiaomi-light-ceil04.json"),
        );
        let original;
        {
            let store = Store::open(directory.path()).unwrap();
            original = store.devices().publish_cached(&cached, "Light").unwrap();
            let mut changed = cached.clone();
            let function = &mut changed.device.functions[0];
            let lighting = function.lighting.as_mut().unwrap();
            match change {
                "add" => {
                    lighting.color = Some(NumericBinding {
                        property: PropertyBinding { siid: 2, piid: 9 },
                        format: NumericFormat::Uint32,
                        unit: NumericUnit::Rgb,
                        range: NumericRange {
                            min: 0,
                            max: 0xffffff,
                            step: 1,
                        },
                    });
                    function.description.capabilities.push(Capability::ColorRgb);
                }
                "remove" => {
                    function.power = None;
                    function
                        .description
                        .capabilities
                        .retain(|capability| *capability != Capability::OnOff);
                }
                "range" => {
                    let range = &mut lighting.temperature.as_mut().unwrap().range;
                    range.max = 5600;
                    function.description.capabilities[2] = Capability::ColorTemperature(*range);
                }
                _ => unreachable!(),
            }
            assert!(
                matches!(
                    store.devices().publish_cached(&changed, "Changed name"),
                    Err(AdmissionError::IncompatibleFunctions)
                ),
                "{change}"
            );
        }
        let store = Store::open(directory.path()).unwrap();
        assert_eq!(
            store.devices().restore_cached("account", "home").unwrap(),
            vec![(cached, original)],
            "{change}"
        );
    }
}

#[test]
fn ls2307_model_stays_a_parser_adapter_and_storage_rejects_real_conflicts() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let outlet: Value = serde_json::from_str(include_str!("fixtures/miot/outlet.json")).unwrap();
    assert!(
        parse_device_for_model(
            DeviceKey::new("xiaomi", "wrong"),
            "devcea.light.ls2307",
            &outlet
        )
        .is_err()
    );
    // Storage no longer keys on the model string; a real category conflict is
    // rejected while valid normalized bindings are trusted.
    let light_instance: Value =
        serde_json::from_str(include_str!("fixtures/miot/xiaomi-light-ceil04.json")).unwrap();
    let mut light_cached = CachedPhysicalDevice {
        uid: "account".into(),
        home_id: "home".into(),
        group_id: group_id("account", "home"),
        model: "devcea.light.ls2307".into(),
        instance_type: outlet["type"].as_str().unwrap().into(),
        device: migate::xiaomi::capability::parse_device(
            DeviceKey::new("xiaomi", "wrong"),
            &light_instance,
        )
        .unwrap(),
    };
    assert!(
        store
            .devices()
            .publish_cached(&light_cached, "Wrong")
            .is_err()
    );
    light_cached.instance_type = light_instance["type"].as_str().unwrap().into();
    light_cached.device.functions[0].description = outlet_function(&outlet);
    assert!(
        store
            .devices()
            .publish_cached(&light_cached, "Wrong")
            .is_err()
    );

    let mut outlet_cached = CachedPhysicalDevice {
        uid: "account".into(),
        home_id: "home".into(),
        group_id: group_id("account", "home"),
        model: "devcea.light.ls2307".into(),
        instance_type: outlet["type"].as_str().unwrap().into(),
        device: migate::xiaomi::capability::parse_device(
            DeviceKey::new("xiaomi", "wrong"),
            &outlet,
        )
        .unwrap(),
    };
    outlet_cached.device.functions[0].description.category = Category::Light;
    assert!(
        store
            .devices()
            .publish_cached(&outlet_cached, "Wrong")
            .is_err()
    );
}

fn outlet_function(instance: &Value) -> migate::device::FunctionDescription {
    migate::xiaomi::capability::parse_device(DeviceKey::new("xiaomi", "wrong"), instance)
        .unwrap()
        .functions[0]
        .description
        .clone()
}

#[test]
fn valid_kelvin_ranges_publish_restore_and_map_without_matter_limits() {
    use migate::device::NumericRange;
    for (min, max) in [(4500, 4501), (10, 6500), (1, 6500)] {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let mut cached = light(
            "xiaomi.light.ceil04",
            include_str!("fixtures/miot/xiaomi-light-ceil04.json"),
        );
        let range = NumericRange { min, max, step: 1 };
        cached.device.functions[0]
            .lighting
            .as_mut()
            .unwrap()
            .temperature
            .as_mut()
            .unwrap()
            .range = range;
        cached.device.functions[0].description.capabilities[2] =
            Capability::ColorTemperature(range);
        assert!(range.mired_bounds().is_err(), "{min}..{max}");
        let description = migate::device::PhysicalDeviceDescription {
            key: cached.device.key.clone(),
            label: "Valid range".into(),
            functions: vec![cached.device.functions[0].description.clone()],
        };
        store
            .devices()
            .publish_cached(&cached, "Valid range")
            .unwrap();
        assert!(
            store.devices().publish(&description).is_ok(),
            "{min}..{max}"
        );
        assert_eq!(
            store
                .devices()
                .load_parsed("account", "home", &cached.device.key)
                .unwrap(),
            Some(cached)
        );
    }
}

#[test]
fn all_fifteen_optional_light_binding_sets_roundtrip_through_storage() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let complete = light(
        "devcea.light.ls2307",
        include_str!("fixtures/miot/devcea-light-ls2307.json"),
    );
    let mut expected = Vec::new();
    // Storage consumes normalized bindings; MIoT layout variants belong to parser tests.
    for mask in 1..16 {
        let enabled = |bit| mask & bit != 0;
        let mut cached = complete.clone();
        cached.device.key = DeviceKey::new("xiaomi", format!("did-{mask}"));
        let function = &mut cached.device.functions[0];
        function
            .description
            .capabilities
            .retain(|capability| match capability {
                Capability::OnOff => enabled(1),
                Capability::Brightness => enabled(2),
                Capability::ColorTemperature(_) => enabled(4),
                Capability::ColorRgb => enabled(8),
            });
        function.power = function.power.filter(|_| enabled(1));
        let lighting = function.lighting.as_mut().unwrap();
        lighting.brightness = lighting.brightness.filter(|_| enabled(2));
        lighting.brightness_conversion = lighting.brightness_conversion.filter(|_| enabled(2));
        lighting.temperature = lighting.temperature.filter(|_| enabled(4));
        lighting.color = lighting.color.filter(|_| enabled(8));
        lighting.mode = None;
        if mask == 1 {
            function.lighting = None;
        }
        let identity = store
            .devices()
            .publish_cached(&cached, "Subset light")
            .unwrap();
        assert_eq!(
            store
                .devices()
                .load_parsed("account", "home", &cached.device.key)
                .unwrap(),
            Some(cached.clone())
        );
        expected.push((cached, identity));
    }
    assert_eq!(expected.len(), 15);
    drop(store);
    let store = Store::open(directory.path()).unwrap();
    let restored = store.devices().restore_cached("account", "home").unwrap();
    assert_eq!(restored.len(), 15);
    for (cached, stored) in restored {
        let (expected, identity) = expected
            .iter()
            .find(|(expected, _)| expected.device.key == cached.device.key)
            .unwrap();
        assert_eq!(&cached, expected);
        assert_eq!(stored.unique_id, identity.unique_id);
        assert_eq!(stored.endpoint, identity.endpoint);
        assert_eq!(stored.label, identity.label);
        assert_eq!(
            stored.functions[0].capabilities,
            cached.device.functions[0].description.capabilities
        );
    }
}

const CT_CACHE_FUNCTIONS: &str = r#"[{"description":{"id":"main","label":"Light","category":"light","capabilities":["on_off","brightness",{"color_temperature":{"min":2700,"max":6500,"step":1}}]},"power":{"siid":2,"piid":1},"lighting":{"brightness":{"property":{"siid":2,"piid":2},"format":"uint8","unit":"percentage","range":{"min":1,"max":100,"step":1}},"temperature":{"property":{"siid":2,"piid":3},"format":"uint16","unit":"kelvin","range":{"min":2700,"max":6500,"step":1}},"color":null,"mode":null,"brightness_conversion":"nonzero_linear"}}]"#;
const RGB_CACHE_FUNCTIONS: &str = r#"[{"description":{"id":"main","label":"Light","category":"light","capabilities":["on_off","brightness",{"color_temperature":{"min":2700,"max":6500,"step":1}},"color_rgb"]},"power":{"siid":2,"piid":1},"lighting":{"brightness":{"property":{"siid":2,"piid":2},"format":"uint8","unit":"percentage","range":{"min":1,"max":100,"step":1}},"temperature":{"property":{"siid":2,"piid":3},"format":"uint32","unit":"kelvin","range":{"min":2700,"max":6500,"step":1}},"color":{"property":{"siid":4,"piid":1},"format":"uint32","unit":"rgb","range":{"min":0,"max":16777215,"step":1}},"mode":{"property":{"siid":4,"piid":2},"format":"uint8","temperature_value":0,"rgb_value":1},"brightness_conversion":"nonzero_linear"}}]"#;
const ONOFF_CACHE_FUNCTIONS: &str = r#"[{"description":{"id":"main","label":"Light","category":"light","capabilities":["on_off"]},"power":{"siid":2,"piid":1},"lighting":null}]"#;
const LIGHT_RGB_CACHE_FUNCTIONS: &str = r#"[{"description":{"id":"main","label":"Light","category":"light","capabilities":["on_off","color_rgb"]},"power":{"siid":2,"piid":1},"lighting":{"brightness":null,"temperature":null,"color":{"property":{"siid":2,"piid":5},"format":"uint32","unit":"rgb","range":{"min":0,"max":16777215,"step":1}},"mode":null,"brightness_conversion":null}}]"#;

#[test]
fn old_ct_rgb_and_optional_caches_restore_without_model_specific_admission() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    {
        let connection = rusqlite::Connection::open(store.path()).unwrap();
        for (did, model, instance_type, capabilities, functions) in [
            (
                "ct",
                "xiaomi.light.ceil04",
                "urn:miot-spec-v2:device:light:0000A001:xiaomi-ceil04:1:0000C802",
                r#"["on_off","brightness",{"color_temperature":{"min":2700,"max":6500,"step":1}}]"#,
                CT_CACHE_FUNCTIONS,
            ),
            (
                "rgb",
                "vendor.light.rgb",
                "urn:miot-spec-v2:device:light:0000A001:vendor-rgb:1",
                r#"["on_off","brightness",{"color_temperature":{"min":2700,"max":6500,"step":1}},"color_rgb"]"#,
                RGB_CACHE_FUNCTIONS,
            ),
            (
                "onoff",
                "vendor.light.onoff",
                "urn:miot-spec-v2:device:light:0000A001:vendor-onoff:1",
                r#"["on_off"]"#,
                ONOFF_CACHE_FUNCTIONS,
            ),
            (
                "lightrgb",
                "vendor.light.lightrgb",
                "urn:miot-spec-v2:device:light:0000A001:vendor-lightrgb:1",
                r#"["on_off","color_rgb"]"#,
                LIGHT_RGB_CACHE_FUNCTIONS,
            ),
        ] {
            connection
                .execute(
                    "INSERT INTO device_cache VALUES (?1,'xiaomi',?2,'home',?3,?4,?5,'light',?6,?7,NULL,NULL)",
                    rusqlite::params![
                        "account",
                        did,
                        group_id("account", "home"),
                        model,
                        instance_type,
                        capabilities,
                        functions
                    ],
                )
                .unwrap();
        }
    }
    for (did, has_color, has_lighting) in [
        ("ct", false, true),
        ("rgb", true, true),
        ("onoff", false, false),
        ("lightrgb", true, true),
    ] {
        let cached = store
            .devices()
            .load_parsed("account", "home", &DeviceKey::new("xiaomi", did))
            .unwrap()
            .unwrap_or_else(|| panic!("{did}"));
        let json = serde_json::to_value(&cached.device.functions).unwrap();
        assert!(json[0]["power"].is_object(), "{did}");
        assert_eq!(json[0]["lighting"].is_object(), has_lighting, "{did}");
        assert_eq!(json[0]["lighting"]["color"].is_object(), has_color, "{did}");
    }
}

#[test]
fn inconsistent_optional_bindings_are_rejected_on_publish() {
    use migate::xiaomi::capability::{
        BrightnessConversion, ModeBinding, NumericFormat, PropertyBinding,
    };
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let rgb = CachedPhysicalDevice {
        uid: "account".into(),
        home_id: "home".into(),
        group_id: group_id("account", "home"),
        model: "synthetic.light.rgbonly".into(),
        instance_type: "urn:miot-spec-v2:device:light:0000A001:synthetic-rgbonly:1".into(),
        device: parse_device_for_model(
            DeviceKey::new("xiaomi", "rgb"),
            "synthetic.light.rgbonly",
            &serde_json::from_str(include_str!("fixtures/miot/synthetic-rgbonly-light.json"))
                .unwrap(),
        )
        .unwrap(),
    };
    let mut fabricated_conversion = rgb.clone();
    fabricated_conversion.device.functions[0]
        .lighting
        .as_mut()
        .unwrap()
        .brightness_conversion = Some(BrightnessConversion::NonzeroLinear);
    assert!(
        store
            .devices()
            .publish_cached(&fabricated_conversion, "Fabricated")
            .is_err()
    );
    let mut fabricated_mode = rgb;
    fabricated_mode.device.functions[0]
        .lighting
        .as_mut()
        .unwrap()
        .mode = Some(ModeBinding {
        property: PropertyBinding { siid: 4, piid: 2 },
        format: NumericFormat::Uint8,
        temperature_value: 0,
        rgb_value: 1,
    });
    assert!(
        store
            .devices()
            .publish_cached(&fabricated_mode, "Fabricated")
            .is_err()
    );
}
