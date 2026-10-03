use migate::{device::DeviceKey, storage::Store, xiaomi::cloud::group_id};
use rusqlite::{Connection, params, types::Value};

const BRIDGE_ID: &str = "11111111111111111111111111111111";
const LIGHT_ID: &str = "22222222222222222222222222222222";
const DEVICE_ID: &str = "33333333333333333333333333333333";

fn legacy_database(path: &std::path::Path, version: i64) -> Connection {
    let db = Connection::open(path).unwrap();
    db.execute_batch(include_str!("fixtures/storage-v2.sql"))
        .unwrap();
    if version < 2 {
        db.execute_batch("DROP TABLE device_cache; DROP TABLE device_identity;")
            .unwrap();
    }
    if version == 0 {
        db.execute_batch("DROP TABLE xiaomi_auth;").unwrap();
    }
    db.pragma_update(None, "user_version", version).unwrap();
    db.execute(
        "INSERT INTO identity VALUES (1, ?1, ?2)",
        [BRIDGE_ID, LIGHT_ID],
    )
    .unwrap();
    for (key, value) in [
        (0, vec![0, 255, 128, 0]),
        (7, b"opaque paired fabric".to_vec()),
        (42, b"opaque legacy Scenes".to_vec()),
        (65535, vec![]),
    ] {
        db.execute("INSERT INTO blobs VALUES (?1, ?2)", params![key, value])
            .unwrap();
    }
    if version >= 1 {
        db.execute(
            "INSERT INTO xiaomi_auth VALUES (1, ?1, 'cn', ?2, ?3, ?4, ?5, 2000000000, 1900000000, ?6, ?7, ?8)",
            params!["account", "550e8400-e29b-41d4-a716-446655440000", "http://homeassistant.local:8123/api/webhook/callback", "test-access", "test-refresh", "18446744073709551615", "test-private-key", "test-certificate"],
        ).unwrap();
    }
    if version >= 2 {
        db.execute(
            "INSERT INTO device_cache VALUES ('account', 'xiaomi', 'did', 'home', ?1, 'vendor.plug', 'urn:miot-spec-v2:device:outlet:0000A002:vendor-v1:1', 'outlet', 'on_off', 12, 9)",
            [group_id("account", "home")],
        ).unwrap();
        db.execute(
            "INSERT INTO device_identity VALUES ('xiaomi', 'did', ?1, 2)",
            [DEVICE_ID],
        )
        .unwrap();
    }
    db
}

fn rows(db: &Connection, table: &str) -> Vec<Vec<Value>> {
    let mut statement = db
        .prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))
        .unwrap();
    statement
        .query_map([], |row| {
            (0..row.as_ref().column_count())
                .map(|column| row.get(column))
                .collect()
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

fn tables(version: i64) -> Vec<&'static str> {
    let mut names = vec!["identity", "blobs"];
    if version >= 1 {
        names.push("xiaomi_auth");
    }
    if version >= 2 {
        names.extend(["device_cache", "device_identity"]);
    }
    names
}

#[test]
fn every_legacy_version_preserves_all_real_data_and_only_removes_light_identity() {
    for version in 0..=2 {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let db = legacy_database(&path, version);
        let preserved = tables(version)
            .into_iter()
            .filter(|table| *table != "identity")
            .map(|table| (table, rows(&db, table)))
            .collect::<Vec<_>>();

        let store = Store::open(dir.path()).unwrap();
        assert_eq!(store.load_identity().unwrap().bridge_id, BRIDGE_ID);
        assert_eq!(
            rows(&db, "identity"),
            [vec![Value::Integer(1), Value::Text(BRIDGE_ID.into())]]
        );
        assert_eq!(
            db.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            3
        );
        for (table, original) in preserved {
            assert_eq!(rows(&db, table), original, "version {version}: {table}");
        }
        if version >= 1 {
            let account = store.xiaomi().load().unwrap().unwrap();
            assert_eq!(account.uid, "account");
            assert_eq!(account.tokens.access_token, "test-access");
            assert_eq!(account.private_key_pem, "test-private-key");
        } else {
            assert!(store.xiaomi().load().unwrap().is_none());
        }
        if version >= 2 {
            let key = DeviceKey::new("xiaomi", "did");
            let cached = store.devices().load("account", &key).unwrap().unwrap();
            assert_eq!(cached.model, "vendor.plug");
            assert_eq!(cached.outlet.main_power.siid, 12);
            assert_eq!(cached.outlet.main_power.piid, 9);
            let identity = store.load_or_create_device_identity(&key).unwrap();
            assert_eq!(identity.unique_id, DEVICE_ID);
            assert_eq!(identity.endpoint, 2);
        }
        drop(store);
        assert_eq!(
            Store::open(dir.path())
                .unwrap()
                .load_identity()
                .unwrap()
                .bridge_id,
            BRIDGE_ID
        );
    }
}

#[test]
fn invalid_legacy_root_identity_rolls_back_without_regenerating_data() {
    for version in 0..=2 {
        for corruption in [
            "DELETE FROM identity;",
            "UPDATE identity SET bridge_id = 'zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz';",
            "PRAGMA ignore_check_constraints = ON; UPDATE identity SET id = 2;",
            "PRAGMA ignore_check_constraints = ON; INSERT INTO identity VALUES (2, '44444444444444444444444444444444', '55555555555555555555555555555555');",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let db = legacy_database(&dir.path().join("state.db"), version);
            db.execute_batch(corruption).unwrap();
            let original = tables(version)
                .into_iter()
                .map(|table| (table, rows(&db, table)))
                .collect::<Vec<_>>();
            assert!(
                Store::open(dir.path()).is_err(),
                "version {version}: {corruption}"
            );
            assert_eq!(
                db.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                    .unwrap(),
                version
            );
            for (table, original) in original {
                assert_eq!(rows(&db, table), original, "version {version}: {table}");
            }
            if version < 2 {
                assert!(
                    !db.query_row(
                        "SELECT EXISTS (SELECT 1 FROM sqlite_schema WHERE name = 'device_cache')",
                        [],
                        |row| row.get::<_, bool>(0)
                    )
                    .unwrap()
                );
            }
        }
    }
}

#[test]
fn identity_rebuild_ddl_failure_rolls_back_the_whole_upgrade() {
    for version in 0..=2 {
        let dir = tempfile::tempdir().unwrap();
        let db = legacy_database(&dir.path().join("state.db"), version);
        db.execute_batch("CREATE VIEW identity_v3 AS SELECT 'block rebuild' AS id;")
            .unwrap();
        let original = tables(version)
            .into_iter()
            .map(|table| (table, rows(&db, table)))
            .collect::<Vec<_>>();
        assert!(Store::open(dir.path()).is_err(), "version {version}");
        assert_eq!(
            db.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            version
        );
        for (table, original) in original {
            assert_eq!(rows(&db, table), original, "version {version}: {table}");
        }
        for table in if version == 0 {
            vec!["xiaomi_auth", "device_cache", "device_identity"]
        } else if version == 1 {
            vec!["device_cache", "device_identity"]
        } else {
            vec![]
        } {
            assert!(
                !db.query_row(
                    "SELECT EXISTS (SELECT 1 FROM sqlite_schema WHERE name = ?1)",
                    [table],
                    |row| row.get::<_, bool>(0)
                )
                .unwrap()
            );
        }
        assert_eq!(
            db.query_row(
                "SELECT type FROM sqlite_schema WHERE name = 'identity_v3'",
                [],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
            "view"
        );
    }
}
