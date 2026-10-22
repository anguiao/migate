//! Native model and private JSON boundary rejection tests. Values must pass
//! the same constructors as the network boundary; corrupted records fail the
//! load without being repaired.

use super::fixtures::{native, planned, scope, unique};
use crate::{
    configuration::{CommittedConfiguration, CommittedDevice, CommittedFunction},
    device::DeviceKey,
    storage::Store,
};
use serde_json::{Value, json};

const OUTLET: &str = include_str!("../../../tests/fixtures/miot/outlet.json");
const MULTI: &str = include_str!("../../../tests/fixtures/miot/cuco-cp7pd.json");
const CEILING: &str = include_str!("../../../tests/fixtures/miot/xiaomi-light-ceil04.json");

fn stored(store: &Store) -> Value {
    let json: String = store
        .connection()
        .query_row(
            "SELECT json FROM bridge_configuration WHERE id=1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    serde_json::from_str(&json).unwrap()
}

fn overwrite(store: &Store, value: &Value) {
    store
        .connection()
        .execute(
            "UPDATE bridge_configuration SET json=?1 WHERE id=1",
            [value.to_string()],
        )
        .unwrap();
}

fn index_of(value: &Value, id: &str) -> usize {
    value["devices"]
        .as_array()
        .unwrap()
        .iter()
        .position(|device| device["device_id"] == id)
        .unwrap()
}

fn outlet(store: &Store) {
    planned(
        store,
        vec![native(
            "outlet",
            "cuco.plug.v3",
            serde_json::from_str(OUTLET).unwrap(),
        )],
    );
}

fn outlet_and_light(store: &Store) {
    planned(
        store,
        vec![
            native(
                "light",
                "xiaomi.light.ceil04",
                serde_json::from_str(CEILING).unwrap(),
            ),
            native(
                "outlet",
                "cuco.plug.v3",
                serde_json::from_str(OUTLET).unwrap(),
            ),
        ],
    );
}

#[test]
fn stored_descriptions_recreate_supported_source_topologies() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    outlet(&store);
    assert!(store.devices().load_configuration().is_ok());
}

#[test]
fn stored_outlets_must_match_a_supported_channel_layout() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    outlet(&store);
    let mut changed = stored(&store);
    let index = index_of(&changed, "outlet");
    changed["devices"][index]["native"]["functions"] = json!([
        {"kind":"outlet","id":"main","label":"Main power","power":{"siid":2,"piid":1}},
        {"kind":"outlet","id":"outlet_2","label":"Second","power":{"siid":3,"piid":1}}
    ]);
    overwrite(&store, &changed);
    assert!(store.devices().load_configuration().is_err());
}

#[test]
fn stored_outlets_cannot_hide_a_light_function() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    outlet(&store);
    let mut changed = stored(&store);
    let index = index_of(&changed, "outlet");
    changed["devices"][index]["native"]["functions"] = json!([
        {"kind":"light","id":"main","label":"Light","power":{"siid":2,"piid":1},
         "lighting":{"profile":"on_off"}}
    ]);
    overwrite(&store, &changed);
    assert!(store.devices().load_configuration().is_err());
}

#[test]
fn stored_cp7pd_needs_its_exact_type_trigger_and_channel_bindings() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    planned(
        &store,
        vec![native(
            "multi",
            "cuco.plug.cp7pd",
            serde_json::from_str(MULTI).unwrap(),
        )],
    );
    let valid = stored(&store);
    assert!(store.devices().load_configuration().is_ok());
    for (pointer, value) in [
        ("/devices/0/native/model", json!("vendor.plug")),
        (
            "/devices/0/native/instance_type",
            json!("urn:miot-spec-v2:device:outlet:0000A002:test-v1:1"),
        ),
        ("/devices/0/native/refresh_trigger", Value::Null),
        ("/devices/0/native/functions/0/power/siid", json!(7)),
    ] {
        let mut changed = valid.clone();
        *changed.pointer_mut(pointer).unwrap() = value;
        overwrite(&store, &changed);
        assert!(
            store.devices().load_configuration().is_err(),
            "{pointer} must fail"
        );
    }
    let mut changed = valid.clone();
    changed["devices"][0]["native"]["functions"]
        .as_array_mut()
        .unwrap()
        .pop();
    overwrite(&store, &changed);
    assert!(store.devices().load_configuration().is_err());
}

#[test]
fn missing_required_fields_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    outlet_and_light(&store);
    let valid = stored(&store);
    for pointer in [
        "/scope",
        "/root_version",
        "/devices",
        "/groups",
        "/devices/0/native",
        "/devices/0/functions",
        "/devices/0/unique_id",
    ] {
        let mut changed = valid.clone();
        let (parent, field) = pointer.rsplit_once('/').unwrap();
        let parent = if parent.is_empty() {
            &mut changed
        } else {
            changed.pointer_mut(parent).unwrap()
        };
        parent.as_object_mut().unwrap().remove(field);
        overwrite(&store, &changed);
        assert!(
            store.devices().load_configuration().is_err(),
            "missing {pointer} must fail"
        );
    }
}

#[test]
fn unknown_fields_are_rejected_at_every_layer() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    outlet_and_light(&store);
    let valid = stored(&store);
    for pointer in [
        "",
        "/scope",
        "/devices/0",
        "/devices/0/native",
        "/devices/0/native/functions/0",
    ] {
        let mut changed = valid.clone();
        changed
            .pointer_mut(pointer)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert("unexpected".into(), Value::Bool(true));
        overwrite(&store, &changed);
        assert!(
            store.devices().load_configuration().is_err(),
            "unknown field at {pointer} must fail"
        );
    }
}

#[test]
fn duplicated_identity_and_endpoint_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    outlet_and_light(&store);
    let valid = stored(&store);
    let mut changed = valid.clone();
    changed["devices"]
        .as_array_mut()
        .unwrap()
        .push(valid["devices"][0].clone());
    overwrite(&store, &changed);
    assert!(store.devices().load_configuration().is_err());

    let mut changed = valid.clone();
    let endpoint = changed["devices"][0]["endpoint"].clone();
    changed["devices"][1]["endpoint"] = endpoint.clone();
    changed["devices"][1]["functions"][0]["endpoint"] = endpoint;
    overwrite(&store, &changed);
    assert!(store.devices().load_configuration().is_err());

    let mut changed = valid.clone();
    let unique_id = changed["devices"][0]["unique_id"].clone();
    changed["devices"][1]["unique_id"] = unique_id;
    overwrite(&store, &changed);
    assert!(store.devices().load_configuration().is_err());
}

#[test]
fn duplicated_group_members_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    outlet(&store);
    let mut configuration = store.devices().load_configuration().unwrap();
    configuration
        .groups
        .push(crate::configuration::GroupRelation {
            group_did: "group-1".into(),
            record_did: "record-1".into(),
            model: "vendor.group".into(),
            instance_type: None,
            is_physical: true,
            active: false,
            error: None,
            members: vec!["outlet".into()],
        });
    store
        .devices()
        .commit_configuration(&configuration, &[])
        .unwrap();
    assert!(store.devices().load_configuration().is_ok());
    // A stored duplicate member is rejected at the load boundary rather than
    // silently collapsed.
    let mut encoded = stored(&store);
    encoded["groups"][0]["members"] = serde_json::json!(["outlet", "outlet"]);
    overwrite(&store, &encoded);
    assert!(store.devices().load_configuration().is_err());
}

#[test]
fn a_native_function_without_a_reserved_identity_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    outlet_and_light(&store);
    let mut changed = stored(&store);
    let index = index_of(&changed, "light");
    changed["devices"][index]["functions"] = json!([]);
    overwrite(&store, &changed);
    assert!(store.devices().load_configuration().is_err());
}

#[test]
fn invalid_tagged_profiles_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    outlet_and_light(&store);
    let valid = stored(&store);
    let index = index_of(&valid, "light");
    for (pointer, value) in [
        (
            format!("/devices/{index}/native/functions/0/lighting/profile"),
            json!("unknown"),
        ),
        (
            format!("/devices/{index}/native/functions/0/lighting/temperature"),
            Value::Null,
        ),
        (
            format!("/devices/{index}/native/functions/0/lighting/brightness/step"),
            json!(0),
        ),
    ] {
        let mut changed = valid.clone();
        *changed.pointer_mut(&pointer).unwrap() = value;
        overwrite(&store, &changed);
        assert!(
            store.devices().load_configuration().is_err(),
            "{pointer} must fail"
        );
    }
}

#[test]
fn native_facts_without_a_home_scope_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let mut configuration = planned(
        &store,
        vec![native(
            "outlet",
            "cuco.plug.v3",
            serde_json::from_str(OUTLET).unwrap(),
        )],
    );
    configuration.scope = None;
    assert!(configuration.validate().is_err());
}

#[test]
fn in_memory_native_key_mismatch_is_rejected() {
    let native = native(
        "outlet",
        "cuco.plug.v3",
        serde_json::from_str(OUTLET).unwrap(),
    );
    let mut other = native.clone();
    other = crate::xiaomi::capability::NativeDescription::new(
        other.model().to_owned(),
        other.instance_type().to_owned(),
        crate::xiaomi::capability::parse_device_for_model(
            DeviceKey::new("xiaomi", "other"),
            "cuco.plug.v3",
            &serde_json::from_str(OUTLET).unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    let configuration = CommittedConfiguration {
        scope: Some(scope()),
        root_version: 1,
        devices: vec![CommittedDevice {
            key: DeviceKey::new("xiaomi", "outlet"),
            unique_id: unique("outlet"),
            endpoint: 2,
            label: "Outlet".into(),
            configuration_version: 1,
            functions: vec![CommittedFunction {
                id: "main".into(),
                endpoint: 2,
            }],
            native: Some(other),
        }],
        groups: Vec::new(),
    };
    assert!(configuration.validate().is_err());
}
