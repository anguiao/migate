use super::XiaomiTestFixture;
use crate::{
    storage::{AccountRecovery, Store, XiaomiRecord},
    xiaomi::{
        auth::{AccountClientIdentity, AccountCredentials},
        mijia::MijiaSession,
    },
};

fn record() -> XiaomiRecord {
    XiaomiRecord {
        uid: "uid".into(),
        region: "cn".into(),
        recovery: Some(AccountRecovery {
            credentials: AccountCredentials::new(
                "uid".into(),
                "cuser".into(),
                "pass".into(),
                AccountClientIdentity::new("device".into(), "agent".into()).unwrap(),
            )
            .unwrap(),
            stop: None,
        }),
        mijia: Some(
            MijiaSession::new(
                "uid".into(),
                "service".into(),
                "c2VjdXJpdHk=".into(),
                "agent".into(),
            )
            .unwrap(),
        ),
        ha: None,
        hub: None,
    }
}

#[test]
fn recovery_snapshot_changes_for_identical_values_after_commit() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let record = record();
    let xiaomi = store.xiaomi();
    xiaomi.fixture_replace(&record).unwrap();
    let before = xiaomi.snapshot().unwrap();
    xiaomi
        .update_recovery(
            &record.recovery.as_ref().unwrap().credentials,
            record.mijia.as_ref().unwrap(),
        )
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
    let record = record();
    let xiaomi = store.xiaomi();
    xiaomi.fixture_replace(&record).unwrap();
    let before = xiaomi.snapshot().unwrap();
    rusqlite::Connection::open(store.path()).unwrap().execute_batch(
        "CREATE TRIGGER reject_session BEFORE UPDATE ON xiaomi_mijia_session BEGIN SELECT RAISE(ABORT, 'reject'); END;"
    ).unwrap();
    assert!(
        xiaomi
            .update_recovery(
                &record.recovery.as_ref().unwrap().credentials,
                record.mijia.as_ref().unwrap(),
            )
            .is_err()
    );
    let after = xiaomi.current_snapshot();
    assert_eq!(after.record, Some(record.clone()));
    assert_eq!(after.mijia_generation, before.mijia_generation);
    assert_eq!(xiaomi.load().unwrap(), Some(record));
}
