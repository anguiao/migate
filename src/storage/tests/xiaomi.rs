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
