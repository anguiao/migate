use migate::{
    device::{self, DeviceKey, FunctionKind, LightProfile},
    storage::{self, CachedPhysicalDevice, Store},
    xiaomi::{self, capability::parse_device_for_model, cloud::group_id},
};
use serde_json::Value;
#[path = "../src/storage/tests/fixtures.rs"]
mod publication_support;
use publication_support::{commit, record, scope};

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
fn on_off_and_dimmable_profiles_persist_static_capabilities() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let cached = vec![
        profile_light("profile-switch", false, false),
        profile_light("profile-dimmer", true, false),
    ];
    let records = cached
        .iter()
        .enumerate()
        .map(|(index, cached)| {
            record(
                &cached.physical_description(&cached.device.key().id),
                2 + index as u16,
            )
        })
        .collect::<Vec<_>>();
    store
        .devices()
        .apply_publication(&commit(scope(), cached.clone(), records.clone()))
        .unwrap();
    let state = store.devices().load_publication(Some(&scope())).unwrap();
    assert_eq!(state.records, records);
    assert_eq!(state.cached.len(), 2);
    assert_eq!(
        state.records[0].functions[0].kind,
        FunctionKind::Light(LightProfile::OnOff)
    );
    assert_eq!(
        state.records[1].functions[0].kind,
        FunctionKind::Light(LightProfile::Dimmable)
    );
}

#[test]
fn all_seven_lights_restore_complete_static_bindings_and_identities() {
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
        for (index, (model, fixture)) in models.into_iter().enumerate() {
            let cached = light(model, fixture);
            let identity = record(&cached.physical_description(model), 2 + index as u16);
            assert_eq!(identity.functions.len(), 1);
            assert_eq!(identity.functions[0].key.id, "main");
            assert_eq!(identity.endpoint, identity.functions[0].endpoint);
            saved.push((cached, identity));
        }
        store
            .devices()
            .apply_publication(&commit(
                scope(),
                saved.iter().map(|(cached, _)| cached.clone()).collect(),
                saved.iter().map(|(_, identity)| identity.clone()).collect(),
            ))
            .unwrap();
    }
    let store = Store::open(directory.path()).unwrap();
    let state = store.devices().load_publication(Some(&scope())).unwrap();
    assert_eq!(
        state.records,
        saved
            .iter()
            .map(|(_, identity)| identity.clone())
            .collect::<Vec<_>>()
    );
    for (cached, _) in saved {
        assert!(state.cached.contains(&cached));
    }
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
        let identity = record(&cached.physical_description("RGB light"), 2);
        store
            .devices()
            .apply_publication(&commit(scope(), vec![cached], vec![identity]))
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
                    functions["functions"][0]["lighting"]["color"]["property"]["piid"] =
                        serde_json::json!(8)
                }
                "mode-values" => {
                    functions["functions"][0]["lighting"]["mode"]["rgb_value"] =
                        serde_json::json!(9)
                }
                "range" => {
                    functions["functions"][0]["lighting"]["brightness"]["step"] =
                        serde_json::json!(0)
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
            store.devices().load_publication(Some(&scope())).is_err(),
            "{mutation}"
        );
    }
}

#[test]
fn new_description_and_version_keep_identity_names_and_endpoints() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let mut cached = light(
        "xiaomi.light.ceil04",
        include_str!("fixtures/miot/xiaomi-light-ceil04.json"),
    );
    let original = record(&cached.physical_description("Light"), 2);
    store
        .devices()
        .apply_publication(&commit(
            scope(),
            vec![cached.clone()],
            vec![original.clone()],
        ))
        .unwrap();
    let mut instance: Value =
        serde_json::from_str(include_str!("fixtures/miot/xiaomi-light-ceil04.json")).unwrap();
    let properties = instance["services"][1]["properties"]
        .as_array_mut()
        .unwrap();
    for property in properties {
        let kind = property["type"].as_str().unwrap();
        if kind.contains(":brightness:") {
            property["iid"] = serde_json::json!(8);
        } else if kind.contains(":color-temperature:") {
            property["value-range"][1] = serde_json::json!(5600);
        }
    }
    cached.device =
        parse_device_for_model(cached.device.key().clone(), &cached.model, &instance).unwrap();
    let mut updated = original.clone();
    updated.configuration_version = 2;
    updated.functions[0].kind = cached.device.functions()[0].kind();
    let mut publication = commit(scope(), vec![cached.clone()], vec![updated.clone()]);
    publication.root_version = 2;
    store.devices().apply_publication(&publication).unwrap();
    drop(store);
    let store = Store::open(directory.path()).unwrap();
    let state = store.devices().load_publication(Some(&scope())).unwrap();
    assert_eq!(state.root_version, 2);
    assert_eq!(state.cached, [cached]);
    assert_eq!(state.records, [updated]);
    assert_eq!(state.records[0].unique_id, original.unique_id);
    assert_eq!(state.records[0].endpoint, original.endpoint);
    assert_eq!(state.records[0].label, original.label);
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
    let identity = record(&cached.physical_description("Wrong"), 2);
    assert!(
        store
            .devices()
            .apply_publication(&commit(scope(), vec![cached], vec![identity]))
            .is_err()
    );
}

#[test]
fn unrepresentable_mired_ranges_are_rejected_at_construction_and_cache_restore() {
    for range in [[4500, 4501, 1], [10, 6500, 1]] {
        let mut instance: Value =
            serde_json::from_str(include_str!("fixtures/miot/xiaomi-light-ceil04.json")).unwrap();
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
        temperature["value-range"] = serde_json::json!(range);
        assert!(
            parse_device_for_model(
                DeviceKey::new("xiaomi", "bad-range"),
                "xiaomi.light.ceil04",
                &instance,
            )
            .is_err()
        );
    }
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let cached = light(
        "xiaomi.light.ceil04",
        include_str!("fixtures/miot/xiaomi-light-ceil04.json"),
    );
    let identity = record(&cached.physical_description("Light"), 2);
    store
        .devices()
        .apply_publication(&commit(scope(), vec![cached], vec![identity]))
        .unwrap();
    let db = rusqlite::Connection::open(store.path()).unwrap();
    let mut functions: Value = serde_json::from_str(
        &db.query_row("SELECT functions FROM device_cache", [], |row| {
            row.get::<_, String>(0)
        })
        .unwrap(),
    )
    .unwrap();
    functions["functions"][0]["lighting"]["temperature"]["min"] = serde_json::json!(10);
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
    capabilities["min"] = serde_json::json!(10);
    db.execute(
        "UPDATE device_function SET capabilities=?1",
        [capabilities.to_string()],
    )
    .unwrap();
    assert!(store.devices().load_publication(Some(&scope())).is_err());
}
