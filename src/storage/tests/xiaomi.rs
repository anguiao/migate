use crate::{
    storage::{AccountRecovery, RecoveryStop, Store, XiaomiRecord},
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
fn conditional_recovery_rotates_generation_even_when_values_are_identical() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let record = record();
    let xiaomi = store.xiaomi();
    xiaomi.replace(&record).unwrap();
    let generation = xiaomi.mijia_generation(&record.uid).unwrap().unwrap();
    assert!(
        xiaomi
            .update_recovery_if_current(
                &record.recovery.as_ref().unwrap().credentials,
                record.mijia.as_ref().unwrap(),
                &generation,
            )
            .unwrap()
    );
    assert_ne!(
        xiaomi.mijia_generation(&record.uid).unwrap().unwrap(),
        generation
    );
    assert_eq!(xiaomi.load().unwrap(), Some(record));
}

#[test]
fn conditional_recovery_cannot_update_a_retired_or_stopped_generation() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let record = record();
    let xiaomi = store.xiaomi();
    xiaomi.replace(&record).unwrap();
    let retired = xiaomi.mijia_generation(&record.uid).unwrap().unwrap();
    xiaomi.replace(&record).unwrap();
    assert!(
        !xiaomi
            .stop_recovery_if_current(&record.uid, &retired, RecoveryStop::Rejected)
            .unwrap()
    );
    assert!(
        !xiaomi
            .update_recovery_if_current(
                &record.recovery.as_ref().unwrap().credentials,
                record.mijia.as_ref().unwrap(),
                &retired,
            )
            .unwrap()
    );
    assert_eq!(xiaomi.load().unwrap(), Some(record.clone()));
    let generation = xiaomi.mijia_generation(&record.uid).unwrap().unwrap();
    xiaomi.stop_recovery(RecoveryStop::Rejected).unwrap();
    let stopped = xiaomi.load().unwrap();
    assert!(
        !xiaomi
            .update_recovery_if_current(
                &record.recovery.as_ref().unwrap().credentials,
                record.mijia.as_ref().unwrap(),
                &generation,
            )
            .unwrap()
    );
    assert_eq!(xiaomi.load().unwrap(), stopped);
    assert_eq!(
        xiaomi.mijia_generation(&record.uid).unwrap().unwrap(),
        generation
    );
}

#[test]
fn conditional_recovery_rolls_back_pair_and_generation_on_database_failure() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let record = record();
    let xiaomi = store.xiaomi();
    xiaomi.replace(&record).unwrap();
    let generation = xiaomi.mijia_generation(&record.uid).unwrap().unwrap();
    rusqlite::Connection::open(store.path())
        .unwrap()
        .execute_batch("CREATE TRIGGER reject_session BEFORE UPDATE ON xiaomi_mijia_session BEGIN SELECT RAISE(ABORT, 'reject'); END;")
        .unwrap();
    assert!(
        xiaomi
            .update_recovery_if_current(
                &record.recovery.as_ref().unwrap().credentials,
                record.mijia.as_ref().unwrap(),
                &generation,
            )
            .is_err()
    );
    assert_eq!(xiaomi.load().unwrap(), Some(record.clone()));
    assert_eq!(
        xiaomi.mijia_generation(&record.uid).unwrap().unwrap(),
        generation
    );
}
