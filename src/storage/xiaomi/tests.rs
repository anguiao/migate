use crate::storage::Store;

#[test]
fn authentication_observer_limits_sqlite_lock_retries_to_100_milliseconds() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let connection = store.xiaomi().auth_observer().connection().unwrap();
    let timeout: u32 = connection
        .pragma_query_value(None, "busy_timeout", |row| row.get(0))
        .unwrap();
    assert!(
        timeout <= 100,
        "authentication observation must not inherit SQLite's multi-second lock wait: {timeout}ms"
    );
}
