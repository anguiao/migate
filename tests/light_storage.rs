use migate::{
    device::{Capability, DeviceKey, LightMode, LightPatch, PhysicalDevice},
    storage::{AdmissionError, CachedPhysicalDevice, Store},
    xiaomi::{capability::parse_device_for_model, cloud::group_id},
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

fn profile_light(id: &str, brightness: bool, temperature: bool) -> CachedPhysicalDevice {
    let mut instance: Value =
        serde_json::from_str(include_str!("fixtures/miot/xiaomi-light-ceil04.json")).unwrap();
    instance["services"][1]["properties"]
        .as_array_mut()
        .unwrap()
        .retain(|property| {
            let kind = property["type"].as_str().unwrap_or_default();
            kind.contains(":on:")
                || (brightness && kind.contains(":brightness:"))
                || (temperature && kind.contains(":color-temperature:"))
        });
    CachedPhysicalDevice {
        uid: "account".into(),
        home_id: "home".into(),
        group_id: group_id("account", "home"),
        model: "xiaomi.light.ceil04".into(),
        instance_type: instance["type"].as_str().unwrap().into(),
        device: parse_device_for_model(
            DeviceKey::new("xiaomi", id),
            "xiaomi.light.ceil04",
            &instance,
        )
        .unwrap(),
    }
}

#[test]
fn on_off_and_dimmable_profiles_persist_and_restore_static_capabilities() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    for (id, brightness, expected) in [
        ("profile-switch", false, vec![Capability::OnOff]),
        (
            "profile-dimmer",
            true,
            vec![Capability::OnOff, Capability::Brightness],
        ),
    ] {
        let cached = profile_light(id, brightness, false);
        let identity = store.devices().publish_cached(&cached, id).unwrap();
        assert_eq!(identity.functions[0].capabilities, expected);
    }
    let restored = store.devices().restore_cached("account", "home").unwrap();
    assert_eq!(restored.len(), 2);
    for (cached, stored) in &restored {
        assert_eq!(
            cached.device.functions[0].description.capabilities,
            stored.functions[0].capabilities
        );
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
fn binding_updates_keep_identity_but_range_or_capability_changes_are_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let mut cached = light(
        "xiaomi.light.ceil04",
        include_str!("fixtures/miot/xiaomi-light-ceil04.json"),
    );
    let original_cached = cached.clone();
    let original = store.devices().publish_cached(&cached, "Original").unwrap();
    cached.device.functions[0]
        .lighting
        .as_mut()
        .unwrap()
        .brightness
        .as_mut()
        .unwrap()
        .property
        .piid = 8;
    let changed = store
        .devices()
        .publish_cached(&cached, "Changed cloud name")
        .unwrap();
    assert_eq!(changed.unique_id, original.unique_id);
    assert_eq!(changed.endpoint, original.endpoint);
    assert_eq!(changed.label, original.label);
    assert_eq!(
        changed.description().functions[0]
            .temperature_range()
            .unwrap()
            .max,
        5700
    );
    assert_eq!(
        store
            .devices()
            .load_parsed("account", "home", &cached.device.key)
            .unwrap()
            .unwrap()
            .device
            .functions[0]
            .lighting
            .as_ref()
            .unwrap()
            .brightness
            .as_ref()
            .unwrap()
            .property
            .piid,
        8
    );
    let mut range_changed = cached.clone();
    let range = &mut range_changed.device.functions[0]
        .lighting
        .as_mut()
        .unwrap()
        .temperature
        .as_mut()
        .unwrap()
        .range;
    range.max = 5600;
    range_changed.device.functions[0].description.capabilities[2] =
        Capability::ColorTemperature(*range);
    assert!(matches!(
        store.devices().publish_cached(&range_changed, "Range"),
        Err(AdmissionError::IncompatibleFunctions)
    ));
    let mut incompatible = changed.description();
    incompatible.functions[0]
        .capabilities
        .push(Capability::ColorRgb);
    assert!(matches!(
        store.devices().publish(&incompatible),
        Err(AdmissionError::IncompatibleFunctions)
    ));
    store.devices().remove(&cached.device.key).unwrap();
    assert!(
        store
            .devices()
            .restore_cached("account", "home")
            .unwrap()
            .is_empty()
    );
    let rejoined = store
        .devices()
        .publish_cached(&original_cached, "New")
        .unwrap();
    assert_eq!(rejoined.unique_id, original.unique_id);
    assert_eq!(rejoined.endpoint, original.endpoint);
}

#[test]
fn corrupted_ls2307_binding_or_mode_conversion_cannot_be_restored() {
    use rusqlite::Connection;
    for mutation in ["color-binding", "mode-values", "instance-type", "range"] {
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
        if mutation == "instance-type" {
            db.execute("UPDATE device_cache SET instance_type='urn:miot-spec-v2:device:light:0000A001:unknown-v1:1'", []).unwrap();
        } else {
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
                "mode-values" => {
                    functions[0]["lighting"]["mode"]["rgb_value"] = serde_json::json!(9)
                }
                "range" => {
                    functions[0]["lighting"]["brightness"]["range"]["step"] = serde_json::json!(0)
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
fn binding_update_restores_identical_cache_and_identity_without_reallocating() {
    let directory = tempfile::tempdir().unwrap();
    let mut cached = light(
        "xiaomi.light.ceil04",
        include_str!("fixtures/miot/xiaomi-light-ceil04.json"),
    );
    let original;
    {
        let store = Store::open(directory.path()).unwrap();
        original = store.devices().publish_cached(&cached, "Light").unwrap();
        cached.device.functions[0]
            .lighting
            .as_mut()
            .unwrap()
            .brightness
            .as_mut()
            .unwrap()
            .property
            .piid = 8;
        store.devices().publish_cached(&cached, "New name").unwrap();
    }
    let store = Store::open(directory.path()).unwrap();
    let restored = store.devices().restore_cached("account", "home").unwrap();
    assert_eq!(restored[0].1.unique_id, original.unique_id);
    assert_eq!(restored[0].1.endpoint, original.endpoint);
    assert_eq!(restored[0].1.label, original.label);
    assert_eq!(
        restored[0].0.device.functions[0]
            .lighting
            .as_ref()
            .unwrap()
            .brightness
            .as_ref()
            .unwrap()
            .property
            .piid,
        8
    );
    assert_eq!(
        restored[0].0.device.functions[0].description.capabilities,
        restored[0].1.functions[0].capabilities
    );
}

#[test]
fn ls2307_cannot_publish_an_outlet_description() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let instance: Value = serde_json::from_str(include_str!("fixtures/miot/outlet.json")).unwrap();
    assert!(
        parse_device_for_model(
            DeviceKey::new("xiaomi", "wrong"),
            "devcea.light.ls2307",
            &instance
        )
        .is_err()
    );
    let cached = CachedPhysicalDevice {
        uid: "account".into(),
        home_id: "home".into(),
        group_id: group_id("account", "home"),
        model: "devcea.light.ls2307".into(),
        instance_type: instance["type"].as_str().unwrap().into(),
        device: migate::xiaomi::capability::parse_device(
            DeviceKey::new("xiaomi", "wrong"),
            &instance,
        )
        .unwrap(),
    };
    assert!(store.devices().publish_cached(&cached, "Wrong").is_err());
}

#[test]
fn unrepresentable_mired_ranges_are_rejected_at_publication_and_cache_restore() {
    use migate::device::NumericRange;
    for range in [
        NumericRange {
            min: 4500,
            max: 4501,
            step: 1,
        },
        NumericRange {
            min: 10,
            max: 6500,
            step: 1,
        },
    ] {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let mut cached = light(
            "xiaomi.light.ceil04",
            include_str!("fixtures/miot/xiaomi-light-ceil04.json"),
        );
        let mut description = cached.device.functions[0].description.clone();
        description.capabilities[2] = Capability::ColorTemperature(range);
        cached.device.functions[0].description = description;
        cached.device.functions[0]
            .lighting
            .as_mut()
            .unwrap()
            .temperature
            .as_mut()
            .unwrap()
            .range = range;
        assert!(
            store
                .devices()
                .publish_cached(&cached, "Bad range")
                .is_err()
        );
        let description = migate::device::PhysicalDeviceDescription {
            key: cached.device.key.clone(),
            label: "Bad range".into(),
            functions: vec![cached.device.functions[0].description.clone()],
        };
        assert!(store.devices().publish(&description).is_err());
    }
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let cached = light(
        "xiaomi.light.ceil04",
        include_str!("fixtures/miot/xiaomi-light-ceil04.json"),
    );
    store.devices().publish_cached(&cached, "Light").unwrap();
    let db = rusqlite::Connection::open(store.path()).unwrap();
    let mut functions: Value = serde_json::from_str(
        &db.query_row("SELECT functions FROM device_cache", [], |row| {
            row.get::<_, String>(0)
        })
        .unwrap(),
    )
    .unwrap();
    functions[0]["description"]["capabilities"][2]["color_temperature"]["min"] =
        serde_json::json!(10);
    functions[0]["lighting"]["temperature"]["range"]["min"] = serde_json::json!(10);
    db.execute(
        "UPDATE device_cache SET functions=?1",
        [functions.to_string()],
    )
    .unwrap();
    let mut capabilities: Value = serde_json::from_str(
        &db.query_row("SELECT capabilities FROM device_function", [], |row| {
            row.get::<_, String>(0)
        })
        .unwrap(),
    )
    .unwrap();
    capabilities[2]["color_temperature"]["min"] = serde_json::json!(10);
    db.execute(
        "UPDATE device_function SET capabilities=?1",
        [capabilities.to_string()],
    )
    .unwrap();
    assert!(store.devices().restore_cached("account", "home").is_err());
}
