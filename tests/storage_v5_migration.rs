use migate::{device::DeviceKey, storage::Store, xiaomi::cloud::group_id};
use rusqlite::{Connection, params};

fn legacy() -> (tempfile::TempDir, Connection) {
    let dir = tempfile::tempdir().unwrap();
    let db = Connection::open(dir.path().join("state.db")).unwrap();
    db.execute_batch(include_str!("../src/storage/schema.sql"))
        .unwrap();
    db.execute_batch(include_str!("../src/storage/devices-v4.sql"))
        .unwrap();
    db.execute_batch("PRAGMA user_version=4; INSERT INTO identity VALUES(1,'11111111111111111111111111111111'); INSERT INTO device_identity VALUES('xiaomi','did','22222222222222222222222222222222',2,'Saved name',1); INSERT INTO device_function VALUES('xiaomi','did','main',2,'Main power','outlet','on_off'); INSERT INTO endpoint_allocation VALUES(2,'xiaomi','did','main');").unwrap();
    db.execute("INSERT INTO device_cache VALUES('account','xiaomi','did','home',?1,'vendor.plug','urn:miot-spec-v2:device:outlet:0000A002:vendor-v1:1','outlet','on_off',?2,NULL,NULL)",params![group_id("account","home"),r#"[{"id":"main","label":"Main power","siid":2,"piid":1}]"#]).unwrap();
    db.execute(
        "INSERT INTO blobs VALUES(7,?1)",
        [b"pairing-and-subscriptions".as_slice()],
    )
    .unwrap();
    (dir, db)
}

#[test]
fn version_four_static_cache_is_upgraded_locally_without_changing_identity_or_pairing() {
    let (dir, db) = legacy();
    let store = Store::open(dir.path()).unwrap();
    assert_eq!(
        db.query_row("PRAGMA user_version", [], |row| row.get::<_, u32>(0))
            .unwrap(),
        6
    );
    let stored = store
        .devices()
        .load_published(&DeviceKey::new("xiaomi", "did"))
        .unwrap()
        .unwrap();
    assert_eq!(stored.label, "Saved name");
    assert_eq!(stored.unique_id, "22222222222222222222222222222222");
    assert_eq!(stored.functions[0].endpoint, 2);
    assert_eq!(
        store.matter().get(7).unwrap(),
        Some(b"pairing-and-subscriptions".to_vec())
    );
    let restored = store.devices().restore_cached("account", "home").unwrap();
    assert_eq!(restored.len(), 1);
    assert_eq!(restored[0].0.device.functions[0].power.siid, 2);
    let functions: String = db
        .query_row("SELECT functions FROM device_cache", [], |row| row.get(0))
        .unwrap();
    assert!(functions.contains("description"));
}

#[test]
fn malformed_version_four_static_cache_rolls_back_migration_and_keeps_identity() {
    let (dir, db) = legacy();
    db.execute("UPDATE device_cache SET functions='broken'", [])
        .unwrap();
    assert!(Store::open(dir.path()).is_err());
    assert_eq!(
        db.query_row("PRAGMA user_version", [], |row| row.get::<_, u32>(0))
            .unwrap(),
        4
    );
    assert_eq!(
        db.query_row("SELECT capabilities FROM device_function", [], |row| row
            .get::<_, String>(
            0
        ))
        .unwrap(),
        "on_off"
    );
    assert_eq!(
        db.query_row("SELECT label FROM device_identity", [], |row| row
            .get::<_, String>(0))
            .unwrap(),
        "Saved name"
    );
}
