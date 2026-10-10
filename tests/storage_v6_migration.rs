use migate::{
    device::{Capability, Category, Composition, DeviceKey, FunctionDescription},
    storage::{CachedPhysicalDevice, CachedScope, Store},
    xiaomi::{
        capability::{ParsedFunction, PropertyBinding, parse_device_for_model},
        cloud::group_id,
    },
};
use rusqlite::{Connection, params};
use serde_json::Value;

const BRIDGE: &str = "11111111111111111111111111111111";
const OUTLET_ID: &str = "22222222222222222222222222222222";
const GROUP_ID: &str = "33333333333333333333333333333333";
const LIGHT_ID: &str = "44444444444444444444444444444444";
const NUMERIC_ID: &str = "55555555555555555555555555555555";
const ORDINARY_ID: &str = "66666666666666666666666666666666";

fn outlet_functions() -> String {
    serde_json::to_string(&vec![ParsedFunction {
        description: FunctionDescription {
            id: "main".into(),
            label: "Main power".into(),
            category: Category::Outlet,
            capabilities: vec![Capability::OnOff],
        },
        power: PropertyBinding { siid: 12, piid: 9 },
        lighting: None,
    }])
    .unwrap()
}

fn light_parts() -> (String, String) {
    let instance: Value =
        serde_json::from_str(include_str!("fixtures/miot/xiaomi-light-ceil04.json")).unwrap();
    let device = parse_device_for_model(
        DeviceKey::new("xiaomi", "light"),
        "xiaomi.light.ceil04",
        &instance,
    )
    .unwrap();
    (
        serde_json::to_string(&device.functions).unwrap(),
        serde_json::to_string(&device.functions[0].description.capabilities).unwrap(),
    )
}

fn legacy() -> (tempfile::TempDir, Connection) {
    let dir = tempfile::tempdir().unwrap();
    let db = Connection::open(dir.path().join("state.db")).unwrap();
    db.execute_batch(include_str!("../src/storage/schema.sql"))
        .unwrap();
    db.execute_batch(include_str!("../src/storage/devices-v4.sql"))
        .unwrap();
    db.execute_batch(&format!(
        "PRAGMA user_version=5; INSERT INTO identity VALUES(1,'{BRIDGE}');"
    ))
    .unwrap();
    let home = group_id("account", "home");
    let functions = outlet_functions();
    db.execute(
        "INSERT INTO device_identity VALUES('xiaomi','outlet',?1,2,'Outlet',1)",
        [OUTLET_ID],
    )
    .unwrap();
    db.execute(
        "INSERT INTO device_function VALUES('xiaomi','outlet','main',2,'Main power','outlet','[\"on_off\"]')",
        [],
    )
    .unwrap();
    db.execute(
        "INSERT INTO endpoint_allocation VALUES(2,'xiaomi','outlet','main')",
        [],
    )
    .unwrap();
    db.execute(
        "INSERT INTO device_cache VALUES('account','xiaomi','outlet','home',?1,'vendor.plug','urn:miot-spec-v2:device:outlet:0000A002:vendor-v1:1','outlet','[\"on_off\"]',?2,NULL,NULL)",
        params![home, functions],
    )
    .unwrap();
    db.execute(
        "INSERT INTO device_identity VALUES('xiaomi','group.42',?1,3,'Saved group',0)",
        [GROUP_ID],
    )
    .unwrap();
    db.execute(
        "INSERT INTO device_function VALUES('xiaomi','group.42','main',3,'Main power','outlet','[\"on_off\"]')",
        [],
    )
    .unwrap();
    db.execute(
        "INSERT INTO endpoint_allocation VALUES(3,'xiaomi','group.42','main')",
        [],
    )
    .unwrap();
    db.execute(
        "INSERT INTO device_cache VALUES('account','xiaomi','group.42','home',?1,'mijia.light.group3','urn:miot-spec-v2:device:light:0000A001:mijia-group3:3:0000C802','light','[\"on_off\"]',?2,NULL,NULL)",
        params![home, functions],
    )
    .unwrap();
    let (light_functions, light_capabilities) = light_parts();
    db.execute(
        "INSERT INTO device_identity VALUES('xiaomi','light',?1,4,'Ordinary light',1)",
        [LIGHT_ID],
    )
    .unwrap();
    db.execute(
        "INSERT INTO device_function VALUES('xiaomi','light','main',4,'Main light','light',?1)",
        [&light_capabilities],
    )
    .unwrap();
    db.execute(
        "INSERT INTO endpoint_allocation VALUES(4,'xiaomi','light','main')",
        [],
    )
    .unwrap();
    db.execute(
        "INSERT INTO device_cache VALUES('account','xiaomi','light','home',?1,'xiaomi.light.ceil04','urn:miot-spec-v2:device:light:0000A001:xiaomi:3:0000C802','light',?2,?3,NULL,NULL)",
        params![home, light_capabilities, light_functions],
    )
    .unwrap();
    db.execute(
        "INSERT INTO blobs VALUES(7,?1)",
        [b"pairing-and-subscriptions".as_slice()],
    )
    .unwrap();
    (dir, db)
}

fn light(did: &str) -> CachedPhysicalDevice {
    let instance: Value =
        serde_json::from_str(include_str!("fixtures/miot/xiaomi-light-ceil04.json")).unwrap();
    let model = "xiaomi.light.ceil04";
    CachedPhysicalDevice {
        uid: "account".into(),
        home_id: "home".into(),
        group_id: group_id("account", "home"),
        model: model.into(),
        instance_type: instance["type"].as_str().unwrap().into(),
        device: parse_device_for_model(DeviceKey::new("xiaomi", did), model, &instance).unwrap(),
    }
}

fn scope() -> CachedScope {
    CachedScope {
        uid: "account".into(),
        home_id: "home".into(),
        group_id: group_id("account", "home"),
    }
}

#[test]
fn legacy_group_identity_migrates_to_parent_and_stays_unpublished() {
    let (dir, db) = legacy();
    let store = Store::open(dir.path()).unwrap();
    assert_eq!(
        db.query_row("PRAGMA user_version", [], |row| row.get::<_, u32>(0))
            .unwrap(),
        6
    );
    let restored = store.devices().restore_cached("account", "home").unwrap();
    assert_eq!(restored.len(), 2);
    assert!(
        restored
            .iter()
            .any(|(_, stored)| stored.key == DeviceKey::new("xiaomi", "outlet"))
    );
    assert!(
        restored
            .iter()
            .any(|(_, stored)| stored.key == DeviceKey::new("xiaomi", "light"))
    );
    assert!(
        store
            .devices()
            .load_published(&DeviceKey::new("xiaomi", "group.42"))
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .devices()
            .restore_groups("account", "home")
            .unwrap()
            .groups
            .is_empty()
    );
    let reservation = db
        .query_row(
            "SELECT unique_id, endpoint, label FROM group_identity",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, u16>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(reservation, (GROUP_ID.into(), 3, "Saved group".into()));
    assert_eq!(
        db.query_row("SELECT count(*) FROM group_relation", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        db.query_row(
            "SELECT slot FROM endpoint_allocation WHERE source='xiaomi' AND device_id='group.42'",
            [],
            |row| row.get::<_, String>(0)
        )
        .unwrap(),
        "group"
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM device_function WHERE source='xiaomi' AND device_id='group.42'",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM device_identity WHERE source='xiaomi' AND device_id='group.42'",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    assert_eq!(
        store.matter().get(7).unwrap(),
        Some(b"pairing-and-subscriptions".to_vec())
    );

    store
        .devices()
        .publish_cached(&light("member"), "Member")
        .unwrap();
    let confirmed = store
        .devices()
        .commit_composition(
            &scope(),
            &[Composition {
                key: DeviceKey::new("xiaomi", "group.42"),
                label: "Cloud group".into(),
                members: vec![DeviceKey::new("xiaomi", "member")],
            }],
            &[],
        )
        .unwrap();
    assert_eq!(confirmed.version, 1);
    assert_eq!(confirmed.groups[0].unique_id, GROUP_ID);
    assert_eq!(confirmed.groups[0].endpoint, 3);
    assert_eq!(confirmed.groups[0].label, "Saved group");
}

#[test]
fn published_legacy_group_is_not_restored_as_a_light() {
    let (dir, db) = legacy();
    db.execute(
        "UPDATE device_identity SET published=1 WHERE source='xiaomi' AND device_id='group.42'",
        [],
    )
    .unwrap();
    let store = Store::open(dir.path()).unwrap();
    assert_eq!(
        store
            .devices()
            .restore_cached("account", "home")
            .unwrap()
            .len(),
        2
    );
    assert!(
        store
            .devices()
            .restore_groups("account", "home")
            .unwrap()
            .groups
            .is_empty()
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM group_relation", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn legacy_group_model_with_numeric_did_is_migrated() {
    let (dir, db) = legacy();
    db.execute(
        "INSERT INTO device_identity VALUES('xiaomi','42',?1,5,'Numeric group',0)",
        [NUMERIC_ID],
    )
    .unwrap();
    db.execute(
        "INSERT INTO endpoint_allocation VALUES(5,'xiaomi','42','main')",
        [],
    )
    .unwrap();
    db.execute(
        "INSERT INTO device_function VALUES('xiaomi','42','main',5,'Main power','outlet','[\"on_off\"]')",
        [],
    )
    .unwrap();
    let home = group_id("account", "home");
    db.execute(
        "INSERT INTO device_cache VALUES('account','xiaomi','42','home',?1,'mijia.light.group3','urn:miot-spec-v2:device:light:0000A001:mijia-group3:3:0000C802','light','[\"on_off\"]',?2,NULL,NULL)",
        params![home, outlet_functions()],
    )
    .unwrap();
    let store = Store::open(dir.path()).unwrap();
    let reservation = db
        .query_row(
            "SELECT unique_id, endpoint, label FROM group_identity WHERE device_id='42'",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, u16>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(reservation, (NUMERIC_ID.into(), 5, "Numeric group".into()));
    assert!(
        store
            .devices()
            .load_published(&DeviceKey::new("xiaomi", "42"))
            .unwrap()
            .is_none()
    );
}

#[test]
fn ordinary_light_identity_is_not_reinterpreted_as_a_group() {
    let (dir, db) = legacy();
    let (functions, capabilities) = light_parts();
    db.execute(
        "INSERT INTO device_identity VALUES('xiaomi','42',?1,5,'Ordinary light',1)",
        [ORDINARY_ID],
    )
    .unwrap();
    db.execute(
        "INSERT INTO device_function VALUES('xiaomi','42','main',5,'Main light','light',?1)",
        [&capabilities],
    )
    .unwrap();
    db.execute(
        "INSERT INTO endpoint_allocation VALUES(5,'xiaomi','42','main')",
        [],
    )
    .unwrap();
    let home = group_id("account", "home");
    db.execute(
        "INSERT INTO device_cache VALUES('account','xiaomi','42','home',?1,'xiaomi.light.ceil04','urn:miot-spec-v2:device:light:0000A001:xiaomi:3:0000C802','light',?2,?3,NULL,NULL)",
        params![home, &capabilities, &functions],
    )
    .unwrap();
    let store = Store::open(dir.path()).unwrap();
    assert!(
        store
            .devices()
            .load_published(&DeviceKey::new("xiaomi", "42"))
            .unwrap()
            .is_some()
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM group_identity WHERE device_id='42'",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    assert_eq!(
        store
            .devices()
            .restore_cached("account", "home")
            .unwrap()
            .len(),
        3
    );
}

#[test]
fn invalid_legacy_group_identity_rolls_back_migration() {
    let (dir, db) = legacy();
    db.execute(
        "UPDATE device_identity SET unique_id='broken' WHERE device_id='group.42'",
        [],
    )
    .unwrap();
    assert!(Store::open(dir.path()).is_err());
    assert_eq!(
        db.query_row("PRAGMA user_version", [], |row| row.get::<_, u32>(0))
            .unwrap(),
        5
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM device_identity WHERE device_id='group.42'",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    assert_eq!(
        db.query_row("SELECT value FROM blobs WHERE key=7", [], |row| row
            .get::<_, Vec<u8>>(0))
            .unwrap(),
        b"pairing-and-subscriptions"
    );
    assert!(
        !db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='group_identity')",
            [],
            |row| row.get::<_, bool>(0)
        )
        .unwrap()
    );
}
