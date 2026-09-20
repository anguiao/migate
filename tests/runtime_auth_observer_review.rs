use std::{sync::mpsc, thread, time::Duration};

use migate::storage::{SessionCheckErrorKind, SessionCheckFailure, Store, TokenSet, XiaomiRecord};
use rusqlite::Connection;

fn credentials() -> XiaomiRecord {
    XiaomiRecord {
        uid: "10001".into(),
        region: "cn".into(),
        oauth_client_uuid: "550e8400-e29b-41d4-a716-446655440000".into(),
        redirect_uri: "http://127.0.0.1/callback".into(),
        tokens: TokenSet {
            access_token: "synthetic-access".into(),
            refresh_token: "synthetic-refresh".into(),
            expires_at: 2_000_000_000,
            refresh_at: 1_900_000_000,
        },
        virtual_did: "123456789012345".into(),
        private_key_pem: "synthetic-private-key".into(),
        certificate_pem: "synthetic-certificate".into(),
    }
}

#[test]
fn observer_distinguishes_maintenance_from_a_new_login_session() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let observer = store.xiaomi().auth_observer();
    assert!(observer.observe().unwrap().uid.is_none());
    store.xiaomi().replace(&credentials()).unwrap();
    let original = observer.observe().unwrap();

    let other = Store::open(directory.path()).unwrap();
    let mut replacement = credentials().tokens;
    replacement.access_token = "synthetic-refreshed-access".into();
    other.xiaomi().update_tokens(&replacement).unwrap();
    let maintained = observer.observe().unwrap();
    assert_ne!(maintained.revision, original.revision);
    assert_eq!(maintained.session_generation, original.session_generation);
    assert_eq!(maintained.uid, original.uid);
    let snapshot = observer.snapshot().unwrap();
    assert_eq!(snapshot.revision, maintained.revision);
    assert_eq!(snapshot.record.unwrap().tokens, replacement);

    other.xiaomi().logout().unwrap();
    other.xiaomi().replace(&credentials()).unwrap();
    let relogged = observer.observe().unwrap();
    assert_eq!(relogged.uid, original.uid);
    assert_ne!(relogged.session_generation, original.session_generation);
    assert_ne!(relogged.revision, maintained.revision);
    assert_eq!(
        observer.snapshot().unwrap().session_generation,
        relogged.session_generation
    );
}

#[test]
fn both_fast_observation_and_full_snapshot_reject_a_held_database_lock() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    store.xiaomi().replace(&credentials()).unwrap();
    let observer = store.xiaomi().auth_observer();
    let previous = observer.observe().unwrap();
    let locker = Connection::open(store.path()).unwrap();
    locker.execute_batch("BEGIN EXCLUSIVE").unwrap();

    let (completed, result) = mpsc::channel();
    let reader = observer.clone();
    let worker = thread::spawn(move || {
        completed
            .send((reader.observe(), reader.snapshot()))
            .unwrap();
    });
    // Keep the lock held until both operations return. This timeout only detects a hang;
    // the connection's retry budget is checked independently of thread scheduling.
    let reads = result.recv_timeout(Duration::from_secs(10));
    locker.execute_batch("ROLLBACK").unwrap();
    worker.join().unwrap();
    let (observed, snapshot) =
        reads.expect("authentication reads waited for the lock to be released");
    for failure in [observed.unwrap_err(), snapshot.unwrap_err()] {
        assert!(matches!(
            failure,
            SessionCheckFailure::Storage(ref error)
                if matches!(error.kind(), SessionCheckErrorKind::Busy | SessionCheckErrorKind::Locked)
        ));
    }
    assert_eq!(observer.observe().unwrap(), previous);
    assert_eq!(observer.snapshot().unwrap().record, Some(credentials()));
}

#[test]
fn observer_reports_missing_authentication_state_as_a_storage_failure() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    store.xiaomi().replace(&credentials()).unwrap();
    let observer = store.xiaomi().auth_observer();
    let other = Connection::open(store.path()).unwrap();
    other
        .execute("DELETE FROM auth_session_generation", [])
        .unwrap();
    for failure in [
        observer.observe().unwrap_err(),
        observer.snapshot().unwrap_err(),
    ] {
        let SessionCheckFailure::Storage(error) = failure else {
            panic!("missing schema state must not resemble signed-out credentials");
        };
        assert_eq!(error.kind(), &SessionCheckErrorKind::MissingData);
        assert_eq!(error.path(), store.path());
        let details = format!("{error:?}");
        for secret in [
            "synthetic-access",
            "synthetic-refresh",
            "synthetic-private-key",
            "synthetic-certificate",
        ] {
            assert!(!details.contains(secret));
        }
    }
}

#[test]
fn public_runtime_keeps_a_fatal_observation_after_database_repair() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let runtime = migate::xiaomi::runtime::XiaomiRuntime::new(store.clone()).unwrap();
    let other = Connection::open(store.path()).unwrap();
    other
        .execute("DELETE FROM auth_session_generation", [])
        .unwrap();

    let failure = futures_lite::future::block_on(runtime.run()).unwrap_err();
    let migate::xiaomi::runtime::XiaomiRuntimeError::Storage(error) = failure else {
        panic!("a missing authentication row must stop the runtime as a storage failure");
    };
    assert_eq!(error.path(), store.path());
    assert!(error.operation().contains("Xiaomi"));
    other
        .execute(
            "INSERT INTO auth_session_generation(id, generation) VALUES(1, 0)",
            [],
        )
        .unwrap();
    assert!(store.xiaomi().auth_observer().snapshot().is_ok());
    let retained = runtime.check_failure().unwrap_err();
    assert_eq!(retained.path(), error.path());
    assert_eq!(retained.operation(), error.operation());
    assert_eq!(retained.to_string(), error.to_string());
}
