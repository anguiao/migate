use migate::{device::DeviceKey, storage::Store, xiaomi::cloud::group_id};
use rusqlite::{Connection, params};
fn legacy() -> (tempfile::TempDir, Connection) {
    let dir = tempfile::tempdir().unwrap();
    let db = Connection::open(dir.path().join("state.db")).unwrap();
    db.execute_batch(include_str!("fixtures/storage-v2.sql"))
        .unwrap();
    db.execute_batch("ALTER TABLE identity RENAME TO old_identity; CREATE TABLE identity(id INTEGER PRIMARY KEY CHECK(id=1),bridge_id TEXT NOT NULL CHECK(length(bridge_id)=32)) STRICT;DROP TABLE old_identity; PRAGMA user_version=3;").unwrap();
    db.execute(
        "INSERT INTO identity VALUES(1,'11111111111111111111111111111111')",
        [],
    )
    .unwrap();
    db.execute(
        "INSERT INTO blobs VALUES(7,?1)",
        [b"pairing-and-subscriptions".as_slice()],
    )
    .unwrap();
    db.execute("INSERT INTO device_cache VALUES('account','xiaomi','did','home',?1,'vendor.plug','urn:miot-spec-v2:device:outlet:0000A002:vendor-v1:1','outlet','on_off',2,1)",[group_id("account","home")]).unwrap();
    db.execute(
        "INSERT INTO device_identity VALUES('xiaomi','did','22222222222222222222222222222222',2)",
        [],
    )
    .unwrap();
    (dir, db)
}
#[test]
fn legacy_active_single_device_keeps_endpoint_uuid_and_label_and_other_blobs() {
    for saved in [false, true] {
        let (dir, db) = legacy();
        if saved {
            let mut label = vec![0x0c, 9];
            label.extend_from_slice(b"Home name");
            db.execute(
                "INSERT INTO blobs VALUES(?1,?2)",
                params![rs_matter::persist::VENDOR_KEYS_START, label],
            )
            .unwrap();
        }
        let store = Store::open(dir.path()).unwrap();
        let saved_device = store
            .devices()
            .load_published(&DeviceKey::new("xiaomi", "did"))
            .unwrap()
            .unwrap();
        assert_eq!(saved_device.unique_id, "22222222222222222222222222222222");
        assert_eq!(saved_device.endpoint, 2);
        assert_eq!(saved_device.functions[0].endpoint, 2);
        assert_eq!(saved_device.functions[0].key.id, "main");
        assert_eq!(
            saved_device.label,
            if saved {
                "Home name"
            } else {
                "vendor.plug (did)"
            }
        );
        assert_eq!(
            store.matter().get(7).unwrap(),
            Some(b"pairing-and-subscriptions".to_vec())
        );
        assert_eq!(
            db.query_row("PRAGMA user_version", [], |r| r.get::<_, u32>(0))
                .unwrap(),
            4
        );
    }
}
#[test]
fn legacy_unpublished_identity_without_cache_is_retained_without_publication() {
    let (dir, db) = legacy();
    db.execute("DELETE FROM device_cache", []).unwrap();
    let store = Store::open(dir.path()).unwrap();
    assert!(store.devices().list_published().unwrap().is_empty());
    assert_eq!(
        store
            .load_or_create_device_identity(&DeviceKey::new("xiaomi", "did"))
            .unwrap()
            .unique_id,
        "22222222222222222222222222222222"
    );
}
#[test]
fn invalid_legacy_metadata_label_duplicate_endpoint_and_ddl_failure_roll_back_all_data() {
    for mutation in [
        "UPDATE device_cache SET group_id='wrong'",
        "UPDATE device_cache SET siid=0",
        "UPDATE device_identity SET unique_id='broken'",
        "INSERT INTO device_identity VALUES('xiaomi','other','33333333333333333333333333333333',2)",
        "CREATE VIEW endpoint_allocation AS SELECT 1 AS endpoint",
    ] {
        let (dir, db) = legacy();
        db.execute_batch(mutation).unwrap();
        assert!(Store::open(dir.path()).is_err(), "{mutation}");
        assert_eq!(
            db.query_row("PRAGMA user_version", [], |r| r.get::<_, u32>(0))
                .unwrap(),
            3
        );
        assert_eq!(
            db.query_row("SELECT value FROM blobs WHERE key=7", [], |r| r
                .get::<_, Vec<u8>>(0))
                .unwrap(),
            b"pairing-and-subscriptions"
        );
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM pragma_table_info('device_identity')",
                [],
                |r| r.get::<_, u32>(0)
            )
            .unwrap(),
            4
        );
        assert!(
            !db.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='device_function')",
                [],
                |r| r.get::<_, bool>(0)
            )
            .unwrap()
        );
    }
    for label in [vec![0x15], {
        let mut data = vec![0x0c, 33];
        data.extend_from_slice(&[b'x'; 33]);
        data
    }] {
        let (dir, db) = legacy();
        db.execute(
            "INSERT INTO blobs VALUES(?1,?2)",
            params![rs_matter::persist::VENDOR_KEYS_START, label.clone()],
        )
        .unwrap();
        assert!(Store::open(dir.path()).is_err());
        assert_eq!(
            db.query_row("PRAGMA user_version", [], |r| r.get::<_, u32>(0))
                .unwrap(),
            3
        );
        assert_eq!(
            db.query_row(
                "SELECT value FROM blobs WHERE key=?1",
                [rs_matter::persist::VENDOR_KEYS_START],
                |r| r.get::<_, Vec<u8>>(0)
            )
            .unwrap(),
            label
        );
    }
}
