//! Configuration persistence through the private codec and real transaction.
//! Historical endpoint reservations are independent from current native facts.

use crate::xiaomi::capability::NativeDescription;
use crate::{
    configuration::{
        self, CommittedConfiguration, Plan, PlanInput, Revocation, Scope, SourceDevice, SourceFacts,
    },
    device::{DeviceKey, FunctionKind, LightProfile},
    matter,
    storage::Store,
    xiaomi::capability::parse_device_for_model,
};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::collections::BTreeMap;

const OUTLET: &str = include_str!("../../../tests/fixtures/miot/outlet.json");
const MULTI: &str = include_str!("../../../tests/fixtures/miot/cuco-cp7pd.json");
const CEILING: &str = include_str!("../../../tests/fixtures/miot/xiaomi-light-ceil04.json");
const LS2307: &str = include_str!("../../../tests/fixtures/miot/devcea-light-ls2307.json");

pub(super) fn scope(uid: &str, home: &str) -> Scope {
    Scope {
        uid: uid.into(),
        home_id: home.into(),
    }
}

pub(super) fn native(id: &str, model: &str, instance: Value) -> NativeDescription {
    NativeDescription::new(
        model.into(),
        instance["type"].as_str().unwrap().into(),
        parse_device_for_model(DeviceKey::new("xiaomi", id), model, &instance).unwrap(),
    )
    .unwrap()
}

pub(super) fn fixture(id: &str, model: &str, data: &str) -> NativeDescription {
    native(id, model, serde_json::from_str(data).unwrap())
}

pub(super) fn next(
    current: &CommittedConfiguration,
    scope: Scope,
    natives: Vec<NativeDescription>,
) -> Plan {
    let facts = SourceFacts {
        scope: Some(scope),
        devices: natives
            .into_iter()
            .map(|native| SourceDevice {
                key: native.device().key().clone(),
                name: Some(native.model().to_owned()),
                native,
                fresh: true,
            })
            .collect(),
        relations: Vec::new(),
    };
    let shapes = |derived: &configuration::DerivedConfiguration| {
        derived
            .served
            .iter()
            .map(|(key, served)| {
                (
                    key.clone(),
                    matter::fixed_shape(served.description.functions()),
                )
            })
            .collect()
    };
    let current_derived = current.derive();
    let new_identities = facts
        .devices
        .iter()
        .filter(|device| current.device(&device.key).is_none())
        .map(|device| {
            (
                device.key.clone(),
                uuid::Uuid::new_v4().simple().to_string(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    configuration::plan(PlanInput {
        current: current.clone(),
        current_shapes: shapes(&current_derived),
        current_derived,
        facts_shapes: shapes(&facts.derive()),
        facts,
        revocations: Vec::new(),
        new_identities,
    })
    .unwrap()
}

pub(super) fn save(store: &Store, config: &CommittedConfiguration) {
    store.devices().commit_configuration(config, &[]).unwrap();
}

pub(super) fn load(store: &Store) -> CommittedConfiguration {
    store.devices().load_configuration().unwrap()
}

pub(super) fn raw(store: &Store) -> String {
    store
        .connection()
        .query_row(
            "SELECT json FROM bridge_configuration WHERE id=1",
            [],
            |row| row.get(0),
        )
        .unwrap()
}

pub(super) fn corrupt(store: &Store, value: &Value) {
    store
        .connection()
        .execute(
            "UPDATE bridge_configuration SET json=?1 WHERE id=1",
            [value.to_string()],
        )
        .unwrap();
}

pub(super) fn seed(store: &Store, devices: Vec<NativeDescription>) -> CommittedConfiguration {
    let config = next(
        &CommittedConfiguration::empty(),
        scope("account", "home"),
        devices,
    )
    .configuration;
    save(store, &config);
    config
}

#[test]
fn caches_are_account_scoped_and_identity_survives_restart_and_routing_changes() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let outlet = fixture("did", "cuco.plug.v3", OUTLET);
    let mut config = seed(&store, vec![outlet.clone()]);
    config.devices[0].label = "Home name".into();
    save(&store, &config);
    let identity = config.devices[0].clone();
    for target in [
        scope("account-two", "home"),
        scope("account-two", "different-home"),
    ] {
        config = next(&config, target.clone(), vec![outlet.clone()]).configuration;
        save(&store, &config);
        assert_eq!(load(&store).scope, Some(target));
        assert_eq!(load(&store).devices, std::slice::from_ref(&identity));
    }
    drop(store);
    let store = Store::open(dir.path()).unwrap();
    assert_eq!(load(&store), config);
    let encoded: Value = serde_json::from_str(&raw(&store)).unwrap();
    let native = &encoded["devices"][0]["native"];
    for dynamic in [
        "online",
        "authenticated",
        "pending",
        "current_power",
        "group_id",
        "uid",
        "home_id",
    ] {
        assert!(native.get(dynamic).is_none(), "{dynamic}");
    }
    assert_eq!(store.load_identity().unwrap().bridge_id.len(), 32);
}

#[test]
fn damaged_cache_and_identity_fail_without_replacing_stored_data() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    seed(&store, vec![fixture("did", "cuco.plug.v3", OUTLET)]);
    let valid: Value = serde_json::from_str(&raw(&store)).unwrap();
    let bridge = store.load_identity().unwrap().bridge_id;
    for (pointer, value) in [
        ("/devices/0/native/functions/0/kind", json!("unknown")),
        ("/devices/0/unique_id", json!("broken")),
        ("/devices/0/configuration_version", json!(0)),
        ("/devices/0/endpoint", json!(3)),
        ("/devices/0/functions/0/endpoint", json!(3)),
        ("/root_version", json!(0)),
        ("/scope", Value::Null),
    ] {
        let mut changed = valid.clone();
        *changed.pointer_mut(pointer).unwrap() = value;
        corrupt(&store, &changed);
        assert!(store.devices().load_configuration().is_err(), "{pointer}");
        assert_eq!(raw(&store), changed.to_string());
        assert_eq!(store.load_identity().unwrap().bridge_id, bridge);
    }
}

#[test]
fn late_write_failure_preserves_configuration_and_revocations() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let original = seed(&store, vec![fixture("did", "cuco.plug.v3", OUTLET)]);
    let revocation = Revocation {
        key: original.devices[0].key.clone(),
        reason: "confirmed removal".into(),
    };
    store
        .devices()
        .record_revocations(std::slice::from_ref(&revocation))
        .unwrap();
    let db = Connection::open(store.path()).unwrap();
    db.execute_batch("CREATE TRIGGER reject_clear AFTER DELETE ON publication_revocation BEGIN SELECT RAISE(ABORT, 'private details'); END;").unwrap();
    let mut changed = original.clone();
    changed.devices[0].label = "Must roll back".into();
    let error = store
        .devices()
        .commit_configuration(&changed, std::slice::from_ref(&revocation.key))
        .unwrap_err();
    assert!(!format!("{error:?} {error}").contains("private details"));
    assert_eq!(load(&store), original);
    assert_eq!(
        store.devices().load_revocations().unwrap(),
        std::slice::from_ref(&revocation)
    );
    db.execute_batch("DROP TRIGGER reject_clear;").unwrap();
    store
        .devices()
        .commit_configuration(&changed, &[revocation.key])
        .unwrap();
    assert_eq!(load(&store), changed);
    assert!(store.devices().load_revocations().unwrap().is_empty());
}

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
            include_str!("../../../tests/fixtures/miot/yeelink-light-light3.json"),
        ),
        (
            "yeelink.light.ml9",
            include_str!("../../../tests/fixtures/miot/yeelink-light-ml9.json"),
        ),
        (
            "yeelink.light.spot2",
            include_str!("../../../tests/fixtures/miot/yeelink-light-spot2.json"),
        ),
        ("xiaomi.light.ceil04", CEILING),
        (
            "xiaomi.light.bar2",
            include_str!("../../../tests/fixtures/miot/xiaomi-light-bar2.json"),
        ),
        (
            "lemesh.light.wy0c15",
            include_str!("../../../tests/fixtures/miot/lemesh-light-wy0c15.json"),
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
        assert_eq!(device.functions[0].id, "main");
        assert_eq!(device.functions[0].endpoint, device.endpoint);
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
fn ls2307_cannot_publish_an_outlet_description() {
    let instance: Value = serde_json::from_str(OUTLET).unwrap();
    assert!(
        parse_device_for_model(
            DeviceKey::new("xiaomi", "wrong"),
            "devcea.light.ls2307",
            &instance
        )
        .is_err()
    );
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    seed(&store, vec![fixture("wrong", "cuco.plug.v3", OUTLET)]);
    let mut encoded: Value = serde_json::from_str(&raw(&store)).unwrap();
    encoded["devices"][0]["native"]["model"] = json!("devcea.light.ls2307");
    corrupt(&store, &encoded);
    assert!(store.devices().load_configuration().is_err());
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
        assert!(
            parse_device_for_model(
                DeviceKey::new("xiaomi", "bad-range"),
                "xiaomi.light.ceil04",
                &instance
            )
            .is_err()
        );
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
fn stored_identity_names_and_endpoints_survive_withdrawal_and_republication() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let multi = fixture("multi", "cuco.plug.cp7pd", MULTI);
    let mut original = seed(&store, vec![multi.clone()]);
    original.devices[0].label = "Home name".into();
    save(&store, &original);
    let other = fixture("other", "cuco.plug.cp7pd", MULTI);
    let withdrawn = next(&original, scope("account", "home"), vec![other.clone()]).configuration;
    save(&store, &withdrawn);
    assert!(withdrawn.devices[0].native.is_none());
    assert!(!withdrawn.derive().published.contains(multi.device().key()));
    drop(store);
    let store = Store::open(dir.path()).unwrap();
    assert_eq!(load(&store), withdrawn);
    let restored = next(&withdrawn, scope("account", "home"), vec![multi, other]).configuration;
    save(&store, &restored);
    assert_eq!(load(&store), restored);
    let saved = &restored.devices[0];
    assert_eq!(saved.unique_id, original.devices[0].unique_id);
    assert_eq!(saved.endpoint, 2);
    assert_eq!(
        saved
            .functions
            .iter()
            .map(|function| function.endpoint)
            .collect::<Vec<_>>(),
        [3, 4, 5]
    );
    assert_eq!(saved.label, "Home name");
    assert_eq!(
        saved.configuration_version,
        original.devices[0].configuration_version + 2
    );
    assert_eq!(restored.devices[1].endpoint, 6);
}

#[test]
fn malformed_stored_functions_are_rejected_when_loading_a_publication() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    seed(&store, vec![fixture("multi", "cuco.plug.cp7pd", MULTI)]);
    let valid: Value = serde_json::from_str(&raw(&store)).unwrap();
    let bridge = store.load_identity().unwrap().bridge_id;
    for (pointer, replacement) in [
        ("/devices/0/functions/2/id", json!("")),
        ("/devices/0/functions/2/id", json!("outlet_1")),
        ("/devices/0/functions/2/endpoint", json!(3)),
        ("/devices/0/functions/2/endpoint", json!(65535)),
        ("/devices/0/endpoint", json!(3)),
        ("/devices/0/endpoint", json!(4)),
        ("/devices/0/endpoint", json!(5)),
        ("/devices/0/native/functions/2/kind", json!("unknown")),
        ("/devices/0/label", json!("x".repeat(33))),
        ("/devices/0/functions", json!([])),
    ] {
        let mut encoded = valid.clone();
        *encoded.pointer_mut(pointer).unwrap() = replacement;
        corrupt(&store, &encoded);
        assert!(store.devices().load_configuration().is_err(), "{pointer}");
        assert_eq!(raw(&store), encoded.to_string());
        assert_eq!(store.load_identity().unwrap().bridge_id, bridge);
    }
    let mut missing = valid;
    missing["devices"][0]["functions"]
        .as_array_mut()
        .unwrap()
        .pop();
    corrupt(&store, &missing);
    assert!(
        store.devices().load_configuration().is_err(),
        "a current native function must have an endpoint reservation"
    );
}

#[test]
fn failed_configuration_write_does_not_allocate_endpoints() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let before = load(&store);
    let multi = fixture("multi", "cuco.plug.cp7pd", MULTI);
    let planned = next(&before, scope("account", "home"), vec![multi.clone()]).configuration;
    store.connection().execute_batch("CREATE TRIGGER reject_configuration AFTER UPDATE ON bridge_configuration BEGIN SELECT RAISE(ABORT, 'secret'); END;").unwrap();
    assert!(store.devices().commit_configuration(&planned, &[]).is_err());
    assert_eq!(load(&store), before);
    store
        .connection()
        .execute_batch("DROP TRIGGER reject_configuration;")
        .unwrap();
    let retry = next(&load(&store), scope("account", "home"), vec![multi]).configuration;
    assert_eq!(retry.devices[0].endpoint, 2);
    assert_eq!(retry.devices[0].functions, planned.devices[0].functions);
    save(&store, &retry);
    assert_eq!(load(&store), retry);
}

#[test]
fn cache_and_selected_scope_are_account_and_home_scoped() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let multi = fixture("multi", "cuco.plug.cp7pd", MULTI);
    let first = seed(&store, vec![multi]);
    let second = next(
        &first,
        scope("account", "home-two"),
        vec![fixture("other", "cuco.plug.v3", OUTLET)],
    )
    .configuration;
    save(&store, &second);
    let third = next(
        &second,
        scope("other-account", "home-three"),
        vec![fixture("third", "cuco.plug.v3", OUTLET)],
    )
    .configuration;
    save(&store, &third);
    drop(store);
    let restored = load(&Store::open(dir.path()).unwrap());
    assert_eq!(restored.scope, Some(scope("other-account", "home-three")));
    assert_eq!(
        restored
            .devices
            .iter()
            .filter(|device| device.native.is_some())
            .map(|device| device.key.id.as_str())
            .collect::<Vec<_>>(),
        ["third"]
    );
    let history = restored.device(&first.devices[0].key).unwrap();
    assert_eq!(history.unique_id, first.devices[0].unique_id);
    assert_eq!(history.functions, first.devices[0].functions);
    assert!(history.native.is_none());
}

#[test]
fn confirmed_relations_and_historical_reservations_commit_as_one_configuration() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let original = seed(
        &store,
        vec![
            fixture("first", "xiaomi.light.ceil04", CEILING),
            fixture("second", "xiaomi.light.ceil04", CEILING),
        ],
    );
    let mut planned = next(
        &original,
        scope("account", "home"),
        vec![fixture("first", "xiaomi.light.ceil04", CEILING)],
    )
    .configuration;
    planned.devices[0].label = "First renamed".into();
    planned.groups = vec![
        configuration::GroupRelation {
            group_did: "group-one".into(),
            record_did: "record-one".into(),
            model: "xiaomi.group".into(),
            instance_type: Some("urn:miot-spec-v2:device:light:0000A001:vendor:1".into()),
            is_physical: false,
            active: true,
            error: None,
            members: vec!["first".into(), "second".into()],
        },
        configuration::GroupRelation {
            group_did: "group-two".into(),
            record_did: "record-two".into(),
            model: "xiaomi.group".into(),
            instance_type: None,
            is_physical: true,
            active: false,
            error: Some("native group description has no instance type".into()),
            members: Vec::new(),
        },
    ];
    let revoke = Revocation {
        key: original.devices[1].key.clone(),
        reason: "deleted from the account".into(),
    };
    store
        .devices()
        .record_revocations(std::slice::from_ref(&revoke))
        .unwrap();
    save(&store, &planned);
    drop(store);
    let store = Store::open(dir.path()).unwrap();
    assert_eq!(load(&store), planned);
    assert_eq!(
        store.devices().load_revocations().unwrap(),
        std::slice::from_ref(&revoke)
    );
    let retained = &planned.devices[1];
    assert!(retained.native.is_none());
    assert_eq!(retained.unique_id, original.devices[1].unique_id);
    assert_eq!(retained.functions, original.devices[1].functions);

    let mut changed = planned.clone();
    changed.groups.clear();
    changed.devices[0].label = "Must not persist".into();
    store.connection().execute_batch("CREATE TRIGGER reject_clear AFTER DELETE ON publication_revocation BEGIN SELECT RAISE(ABORT, 'injected failure'); END;").unwrap();
    assert!(
        store
            .devices()
            .commit_configuration(&changed, &[revoke.key])
            .is_err()
    );
    assert_eq!(
        load(&store),
        planned,
        "native facts, names, versions, reservations and both active/inactive relations roll back together"
    );
}

#[test]
fn late_transaction_failure_rolls_back_the_whole_first_publication() {
    use std::error::Error as _;
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let empty = load(&store);
    let planned = next(
        &empty,
        scope("account", "home"),
        vec![fixture("new-light", "xiaomi.light.ceil04", CEILING)],
    )
    .configuration;
    let key = planned.devices[0].key.clone();
    let revocation = Revocation {
        key: key.clone(),
        reason: "old membership removed".into(),
    };
    store
        .devices()
        .record_revocations(std::slice::from_ref(&revocation))
        .unwrap();
    store.connection().execute_batch("CREATE TRIGGER injected_failure AFTER DELETE ON publication_revocation BEGIN SELECT RAISE(ABORT, 'injected failure'); END;").unwrap();
    let error = store
        .devices()
        .commit_configuration(&planned, std::slice::from_ref(&key))
        .unwrap_err();
    let source = error
        .source()
        .unwrap()
        .downcast_ref::<rusqlite::Error>()
        .unwrap();
    let rusqlite::Error::SqliteFailure(code, _) = source else {
        panic!("expected trigger failure: {source}");
    };
    assert_eq!(code.extended_code, rusqlite::ffi::SQLITE_CONSTRAINT_TRIGGER);
    assert_eq!(load(&store), empty);
    assert_eq!(store.devices().load_revocations().unwrap(), [revocation]);

    store
        .connection()
        .execute_batch("DROP TRIGGER injected_failure;")
        .unwrap();
    store
        .devices()
        .commit_configuration(&planned, &[key])
        .unwrap();
    drop(store);
    let restored = load(&Store::open(dir.path()).unwrap());
    assert_eq!(restored, planned);
    assert_eq!(restored.devices[0].endpoint, 2);
    assert_eq!(restored.devices[0].functions[0].endpoint, 2);
}
