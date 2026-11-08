use super::{XiaomiTestFixture, xiaomi_credentials};
use crate::storage::Store;

#[test]
fn recovery_snapshot_changes_for_identical_values_after_commit() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let record = xiaomi_credentials("uid");
    let xiaomi = store.xiaomi();
    xiaomi.fixture_replace(&record).unwrap();
    let before = xiaomi.snapshot().unwrap();
    xiaomi
        .update_recovery(&record.recovery.credentials, &record.mijia)
        .unwrap();
    let after = xiaomi.current_snapshot();
    assert_ne!(after.mijia_generation, before.mijia_generation);
    assert!(!xiaomi.current_mijia(&record.uid, before.mijia_generation));
    assert_eq!(after.record, Some(record.clone()));
    assert_eq!(xiaomi.load().unwrap(), Some(record));
}

#[test]
fn recovery_failure_preserves_database_and_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let record = xiaomi_credentials("uid");
    let xiaomi = store.xiaomi();
    xiaomi.fixture_replace(&record).unwrap();
    let before = xiaomi.snapshot().unwrap();
    rusqlite::Connection::open(store.path()).unwrap().execute_batch(
        "CREATE TRIGGER reject_session BEFORE UPDATE ON xiaomi_mijia_session BEGIN SELECT RAISE(ABORT, 'reject'); END;"
    ).unwrap();
    assert!(
        xiaomi
            .update_recovery(&record.recovery.credentials, &record.mijia)
            .is_err()
    );
    let after = xiaomi.current_snapshot();
    assert_eq!(after.record, Some(record.clone()));
    assert_eq!(after.mijia_generation, before.mijia_generation);
    assert_eq!(xiaomi.load().unwrap(), Some(record));
}
