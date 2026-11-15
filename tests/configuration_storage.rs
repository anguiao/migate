//! Public configuration storage boundary.

use migate::storage::Store;
use rusqlite::Connection;

#[test]
fn schema13_is_one_private_singleton() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let db = Connection::open(store.path()).unwrap();
    let version = db
        .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
        .unwrap();
    assert_eq!(version, 13);
    let rows = db
        .query_row("SELECT count(*) FROM bridge_configuration", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap();
    assert_eq!(rows, 1);
    for table in [
        "device_cache",
        "device_identity",
        "device_function",
        "endpoint_allocation",
        "xiaomi_scope",
        "native_group",
        "native_group_member",
        "publication_root",
        "xiaomi_auth",
    ] {
        let exists = db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
                [table],
                |row| row.get::<_, bool>(0),
            )
            .unwrap();
        assert!(!exists, "legacy table {table} must be removed");
    }
}
