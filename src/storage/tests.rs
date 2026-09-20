use super::Store;

#[test]
fn connections_retry_transient_locks_with_a_100_millisecond_budget() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let timeout: u32 = store
        .inner
        .connection
        .pragma_query_value(None, "busy_timeout", |row| row.get(0))
        .unwrap();
    assert_eq!(timeout, 100);
}
