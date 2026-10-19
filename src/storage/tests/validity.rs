//! Domain-boundary rejection tests. A stored description must re-create the
//! native model through the same constructors used by the network boundary.

use super::fixtures::scope;
use crate::device::DeviceKey;
use crate::storage::Store;
use rusqlite::params;

struct Row<'a> {
    model: &'a str,
    instance_type: &'a str,
    category: &'a str,
    functions: &'a str,
    capabilities: &'a str,
    refresh: Option<(u32, u32)>,
}

fn insert(store: &Store, row: Row<'_>) {
    store
        .connection()
        .execute(
            "INSERT INTO device_cache (
                uid, source, device_id, home_id, group_id, model, instance_type,
                category, capabilities, functions, refresh_siid, refresh_piid
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
            params![
                "account",
                "xiaomi",
                "bad-device",
                "home",
                scope().group_id,
                row.model,
                row.instance_type,
                row.category,
                row.capabilities,
                row.functions,
                row.refresh.map(|(siid, _)| siid),
                row.refresh.map(|(_, piid)| piid),
            ],
        )
        .unwrap();
}

fn load(store: &Store) -> Result<bool, ()> {
    store
        .devices()
        .load_parsed("account", "home", &DeviceKey::new("xiaomi", "bad-device"))
        .map(|value| value.is_some())
        .map_err(|_| ())
}

const OUTLET_TYPE: &str = "urn:miot-spec-v2:device:outlet:0000A002:test:1";
const CP7PD_TYPE: &str = "urn:miot-spec-v2:device:outlet:0000A002:cuco-cp7pd:1";

#[test]
fn stored_descriptions_recreate_supported_source_topologies() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let valid = r#"{"functions":[{"id":"main","label":"Main","power":{"siid":2,"piid":1}}]}"#;
    insert(
        &store,
        Row {
            model: "vendor.plug",
            instance_type: OUTLET_TYPE,
            category: "outlet",
            functions: valid,
            capabilities: r#"[{"capability":"on_off"}]"#,
            refresh: None,
        },
    );
    assert!(load(&store).unwrap(), "a single main outlet loads");
}

#[test]
fn stored_outlets_must_match_a_supported_channel_layout() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    // Two arbitrary outlet functions are outside the supported source range.
    let unsupported = r#"{"functions":[
        {"id":"main","label":"Main","power":{"siid":2,"piid":1}},
        {"id":"outlet_2","label":"Second","power":{"siid":3,"piid":1}}
    ]}"#;
    insert(
        &store,
        Row {
            model: "vendor.plug",
            instance_type: OUTLET_TYPE,
            category: "outlet",
            functions: unsupported,
            capabilities: r#"[{"capability":"on_off"}]"#,
            refresh: None,
        },
    );
    assert!(
        load(&store).is_err(),
        "arbitrary outlet topology is rejected"
    );
}

#[test]
fn stored_cp7pd_needs_its_exact_type_trigger_and_channel_bindings() {
    let functions = r#"{"functions":[
        {"id":"outlet_1","label":"One","power":{"siid":3,"piid":1}},
        {"id":"outlet_2","label":"Two","power":{"siid":4,"piid":1}},
        {"id":"usb","label":"USB","power":{"siid":9,"piid":1}}
    ]}"#;
    // A correct cp7pd row loads.
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    insert(
        &store,
        Row {
            model: "cuco.plug.cp7pd",
            instance_type: CP7PD_TYPE,
            category: "outlet",
            functions,
            capabilities: r#"[{"capability":"on_off"}]"#,
            refresh: Some((2, 1)),
        },
    );
    assert!(load(&store).unwrap(), "the concrete cp7pd layout loads");

    // cp7pd model with a standard instance type is rejected.
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    insert(
        &store,
        Row {
            model: "cuco.plug.cp7pd",
            instance_type: OUTLET_TYPE,
            category: "outlet",
            functions,
            capabilities: r#"[{"capability":"on_off"}]"#,
            refresh: Some((2, 1)),
        },
    );
    assert!(
        load(&store).is_err(),
        "cp7pd needs its vendor instance type"
    );

    // A cp7pd instance type with a different model is rejected.
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    insert(
        &store,
        Row {
            model: "vendor.plug",
            instance_type: CP7PD_TYPE,
            category: "outlet",
            functions,
            capabilities: r#"[{"capability":"on_off"}]"#,
            refresh: Some((2, 1)),
        },
    );
    assert!(load(&store).is_err(), "cp7pd type needs the cp7pd model");

    // A wrong master trigger is rejected.
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    insert(
        &store,
        Row {
            model: "cuco.plug.cp7pd",
            instance_type: CP7PD_TYPE,
            category: "outlet",
            functions,
            capabilities: r#"[{"capability":"on_off"}]"#,
            refresh: Some((2, 2)),
        },
    );
    assert!(load(&store).is_err(), "cp7pd requires its master trigger");
}

#[test]
fn stored_light_kind_cannot_hide_behind_an_outlet_instance_type() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let light = r#"{"functions":[{
        "id":"main","label":"Light","power":{"siid":2,"piid":1},
        "lighting":{}
    }]}"#;
    insert(
        &store,
        Row {
            model: "vendor.plug",
            instance_type: OUTLET_TYPE,
            category: "light",
            functions: light,
            capabilities: r#"[{"capability":"on_off"}]"#,
            refresh: None,
        },
    );
    assert!(
        load(&store).is_err(),
        "outlet recovery must stay outlet-only"
    );
}
