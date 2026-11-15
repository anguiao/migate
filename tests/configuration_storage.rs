//! Public configuration storage boundary.

use migate::storage::Store;
use rusqlite::Connection;

#[test]
fn a_new_store_creates_one_configuration_record() {
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
}
