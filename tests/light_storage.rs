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
fn binding_or_range_updates_keep_identity_but_changed_light_features_are_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let mut cached = light(
        "xiaomi.light.ceil04",
        include_str!("fixtures/miot/xiaomi-light-ceil04.json"),
    );
    let original = store.devices().publish_cached(&cached, "Original").unwrap();
    let lighting = cached.device.functions[0].lighting.as_mut().unwrap();
    lighting.brightness.property.piid = 8;
    lighting.temperature.range.max = 5600;
    cached.device.functions[0].description.capabilities[2] =
        Capability::ColorTemperature(lighting.temperature.range);
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
        5600
    );
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
    let rejoined = store.devices().publish_cached(&cached, "New").unwrap();
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
