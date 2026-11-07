use super::XiaomiTestFixture;
use crate::{
    storage::{
        AccountRecovery, HaAuthorization, HaClient, HubIdentity, RecoveryStop, Store, TokenSet,
        XiaomiRecord,
    },
    xiaomi::{
        auth::{AccountClientIdentity, AccountCredentials},
        mijia::MijiaSession,
    },
};
use rusqlite::Connection;
use std::{error::Error as _, fs};

const UUID: &str = "550e8400-e29b-41d4-a716-446655440000";
const REDIRECT: &str =
    "http://homeassistant.local:8123/api/webhook/0123456789abcdef0123456789abcdef";

fn tokens(suffix: &str) -> TokenSet {
    TokenSet {
        access_token: format!("access-{suffix}"),
        refresh_token: format!("refresh-{suffix}"),
        expires_at: 2_000_000_000,
        refresh_at: 1_900_000_000,
    }
}

fn recovery(uid: &str, suffix: &str) -> AccountCredentials {
    AccountCredentials::new(
        uid.into(),
        format!("cuser-{suffix}"),
        format!("pass-{suffix}"),
        AccountClientIdentity::new("stable-device".into(), "test-user-agent".into()).unwrap(),
    )
    .unwrap()
}

fn session(uid: &str, suffix: &str) -> MijiaSession {
    MijiaSession::new(
        uid.into(),
        format!("service-{suffix}"),
        "c2VjdXJpdHk=".into(),
        "test-user-agent".into(),
    )
    .unwrap()
}

fn record(suffix: &str) -> XiaomiRecord {
    let uid = format!("uid-{suffix}");
    XiaomiRecord {
        uid: uid.clone(),
        region: "cn".into(),
        recovery: Some(AccountRecovery {
            credentials: recovery(&uid, suffix),
            stop: None,
        }),
        mijia: Some(session(&uid, suffix)),
        ha: Some(HaClient {
            client_uuid: UUID.into(),
            authorization: Some(HaAuthorization {
                redirect_uri: REDIRECT.into(),
                tokens: tokens(suffix),
            }),
            stopped: false,
        }),
        hub: Some(HubIdentity {
            virtual_did: "18446744073709551615".into(),
            private_key_pem: format!("private-{suffix}"),
            certificate_pem: Some(format!("certificate-{suffix}")),
        }),
    }
}

#[test]
fn account_identity_can_exist_without_any_service_credentials() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    Connection::open(store.path())
        .unwrap()
        .execute(
            "INSERT INTO xiaomi_account (id, uid, region) VALUES (1, 'uid-only', 'cn')",
            [],
        )
        .unwrap();
    let record = store.xiaomi().load().unwrap().unwrap();
    assert_eq!(record.uid, "uid-only");
    assert_eq!(record.region, "cn");
    assert!(record.recovery.is_none());
    assert!(record.mijia.is_none());
    assert!(record.ha.is_none());
    assert!(record.hub.is_none());
}

#[test]
fn login_credentials_persist_and_replace_atomically() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    assert!(store.xiaomi().load().unwrap().is_none());
    let first = record("first");
    store.xiaomi().fixture_replace(&first).unwrap();
    assert_eq!(store.xiaomi().load().unwrap(), Some(first.clone()));
    let mut second = first.clone();
    second.recovery.as_mut().unwrap().credentials = recovery(&first.uid, "second");
    second.mijia = Some(session(&first.uid, "second"));
    second
        .ha
        .as_mut()
        .unwrap()
        .authorization
        .as_mut()
        .unwrap()
        .tokens = tokens("second");
    store
        .xiaomi()
        .commit_login(
            &second.recovery.as_ref().unwrap().credentials,
            second.mijia.as_ref().unwrap(),
            UUID,
            second.ha.as_ref().unwrap().authorization.as_ref().unwrap(),
        )
        .unwrap();
    drop(store);
    assert_eq!(
        Store::open(dir.path()).unwrap().xiaomi().load().unwrap(),
        Some(second)
    );
}

#[test]
fn pem_line_breaks_are_persisted_without_exposing_secrets_in_debug() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let mut credentials = record("pem");
    let hub = credentials.hub.as_mut().unwrap();
    hub.private_key_pem =
        "-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIA==\n-----END PRIVATE KEY-----\n".into();
    hub.certificate_pem =
        Some("-----BEGIN CERTIFICATE-----\nMIIBAA==\n-----END CERTIFICATE-----\n".into());
    let debug = format!("{credentials:?}");
    let ha = credentials
        .ha
        .as_ref()
        .unwrap()
        .authorization
        .as_ref()
        .unwrap();
    let hub = credentials.hub.as_ref().unwrap();
    let account = credentials.recovery.as_ref().unwrap();
    let session = credentials.mijia.as_ref().unwrap();
    for secret in [
        ha.tokens.access_token.as_str(),
        ha.tokens.refresh_token.as_str(),
        ha.redirect_uri.as_str(),
        hub.private_key_pem.as_str(),
        hub.certificate_pem.as_deref().unwrap(),
        account.credentials.pass_token(),
        account.credentials.c_user_id(),
        session.service_token(),
        session.ssecurity(),
    ] {
        assert!(!debug.contains(secret), "debug leaked secret");
    }
    store.xiaomi().fixture_replace(&credentials).unwrap();
    assert_eq!(store.xiaomi().load().unwrap(), Some(credentials));
}

#[test]
fn token_and_certificate_updates_change_only_their_fields() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let original = record("original");
    store.xiaomi().fixture_replace(&original).unwrap();
    let before = store.xiaomi().snapshot().unwrap();
    let replacement_tokens = tokens("updated");
    store.xiaomi().update_tokens(&replacement_tokens).unwrap();
    let after_tokens = store.xiaomi().current_snapshot();
    assert_ne!(before.ha_generation, after_tokens.ha_generation);
    let updated = after_tokens.record.unwrap();
    assert_eq!(
        updated.ha_authorization().unwrap().tokens,
        replacement_tokens
    );
    assert_eq!(updated.hub, original.hub);
    assert_eq!(updated.mijia, original.mijia);
    store
        .xiaomi()
        .update_certificate("certificate-updated")
        .unwrap();
    let after = store.xiaomi().current_snapshot().record.unwrap();
    assert_eq!(
        after.hub.as_ref().unwrap().certificate_pem.as_deref(),
        Some("certificate-updated")
    );
    assert_eq!(after.ha, updated.ha);
    assert_eq!(store.xiaomi().load().unwrap(), Some(after));
}

#[test]
fn rejected_transactional_writes_preserve_the_original_and_hide_secrets() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let original = record("original");
    store.xiaomi().fixture_replace(&original).unwrap();
    let db = Connection::open(store.path()).unwrap();
    db.execute_batch("CREATE TRIGGER reject_tokens AFTER UPDATE ON xiaomi_ha_oauth BEGIN SELECT RAISE(ABORT, 'SECRET CONTENT'); END;
        CREATE TRIGGER reject_certificate AFTER UPDATE ON xiaomi_hub BEGIN SELECT RAISE(ABORT, 'SECRET CONTENT'); END;").unwrap();
    let before = store.xiaomi().snapshot().unwrap();
    let oauth = HaAuthorization {
        redirect_uri: REDIRECT.into(),
        tokens: tokens("SECRET"),
    };
    for result in [
        store.xiaomi().commit_login(
            &recovery(&original.uid, "SECRET"),
            &session(&original.uid, "SECRET"),
            UUID,
            &oauth,
        ),
        store.xiaomi().update_tokens(&tokens("SECRET")),
        store.xiaomi().update_certificate("SECRET"),
    ] {
        let error = result.unwrap_err();
        assert!(error.source().unwrap().is::<rusqlite::Error>());
        assert!(!format!("{error:?} {error}").contains("SECRET CONTENT"));
        assert_eq!(store.xiaomi().load().unwrap(), Some(original.clone()));
        let current = store.xiaomi().current_snapshot();
        assert_eq!(current.record, before.record);
        assert_eq!(current.ha_generation, before.ha_generation);
        assert_eq!(current.mijia_generation, before.mijia_generation);
    }
}

#[test]
fn stopped_groups_survive_restart_and_only_login_commit_clears_them() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let old = record("original");
    let uid = old.uid.clone();
    store.xiaomi().fixture_replace(&old).unwrap();
    store
        .xiaomi()
        .stop_recovery(RecoveryStop::VerificationRequired)
        .unwrap();
    store.xiaomi().stop_ha_refresh().unwrap();
    drop(store);
    let store = Store::open(dir.path()).unwrap();
    let stopped = store.xiaomi().load().unwrap().unwrap();
    assert_eq!(
        stopped.recovery.as_ref().unwrap().stop,
        Some(RecoveryStop::VerificationRequired)
    );
    assert!(stopped.ha.as_ref().unwrap().stopped);
    let oauth = HaAuthorization {
        redirect_uri: REDIRECT.into(),
        tokens: tokens("login"),
    };
    store
        .xiaomi()
        .commit_login(
            &recovery(&uid, "login"),
            &session(&uid, "login"),
            UUID,
            &oauth,
        )
        .unwrap();
    let saved = store.xiaomi().load().unwrap().unwrap();
    assert_eq!(saved.recovery.as_ref().unwrap().stop, None);
    assert!(!saved.ha.as_ref().unwrap().stopped);
    assert_eq!(saved.hub, old.hub);
    assert_eq!(saved.mijia, Some(session(&uid, "login")));
    assert_eq!(
        saved.ha.as_ref().unwrap().authorization.as_ref(),
        Some(&oauth)
    );
}

#[test]
fn login_commit_rolls_back_all_groups_and_stops_on_storage_failure() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let old = record("old");
    store.xiaomi().fixture_replace(&old).unwrap();
    store.xiaomi().snapshot().unwrap();
    store
        .xiaomi()
        .stop_recovery(RecoveryStop::Rejected)
        .unwrap();
    store.xiaomi().stop_ha_refresh().unwrap();
    let before = store.xiaomi().load().unwrap();
    let before_snapshot = store.xiaomi().current_snapshot();
    let oauth = HaAuthorization {
        redirect_uri: REDIRECT.into(),
        tokens: tokens("new"),
    };
    Connection::open(store.path()).unwrap().execute_batch(
        "CREATE TRIGGER reject_login BEFORE UPDATE ON xiaomi_ha_oauth BEGIN SELECT RAISE(ABORT, 'reject'); END;").unwrap();
    assert!(
        store
            .xiaomi()
            .commit_login(
                &recovery(&old.uid, "new"),
                &session(&old.uid, "new"),
                UUID,
                &oauth
            )
            .is_err()
    );
    assert_eq!(store.xiaomi().load().unwrap(), before);
    assert_eq!(
        store.xiaomi().current_snapshot().record,
        before_snapshot.record
    );
}

#[test]
fn recovery_credentials_and_mijia_session_roll_back_together() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let old = record("old");
    store.xiaomi().fixture_replace(&old).unwrap();
    let before_snapshot = store.xiaomi().snapshot().unwrap();
    let before = store.xiaomi().load().unwrap();
    Connection::open(store.path()).unwrap().execute_batch(
        "CREATE TRIGGER reject_session BEFORE UPDATE ON xiaomi_mijia_session BEGIN SELECT RAISE(ABORT, 'reject'); END;",
    ).unwrap();
    assert!(
        store
            .xiaomi()
            .update_recovery(&recovery(&old.uid, "new"), &session(&old.uid, "new"))
            .is_err()
    );
    assert_eq!(store.xiaomi().load().unwrap(), before);
    let after = store.xiaomi().current_snapshot();
    assert_eq!(after.record, before_snapshot.record);
    assert_eq!(after.mijia_generation, before_snapshot.mijia_generation);
}

#[test]
fn ha_client_and_hub_identity_can_exist_without_token_or_certificate() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let mut partial = record("partial");
    partial.recovery = None;
    partial.mijia = None;
    let mut hub = partial.hub.take().unwrap();
    hub.certificate_pem = None;
    store.xiaomi().fixture_replace(&partial).unwrap();
    store.xiaomi().create_hub(&hub).unwrap();
    partial.hub = Some(hub);
    drop(store);
    let reopened = Store::open(dir.path()).unwrap();
    assert_eq!(reopened.xiaomi().load().unwrap(), Some(partial.clone()));
    reopened
        .xiaomi()
        .update_certificate("new-certificate")
        .unwrap();
    let mut saved = reopened.xiaomi().load().unwrap().unwrap();
    saved.ha.as_mut().unwrap().authorization = None;
    reopened.xiaomi().fixture_replace(&saved).unwrap();
    assert_eq!(reopened.xiaomi().load().unwrap(), Some(saved.clone()));
    assert_eq!(saved.ha.as_ref().unwrap().client_uuid, UUID);
    assert_eq!(
        saved.hub.as_ref().unwrap().certificate_pem.as_deref(),
        Some("new-certificate")
    );
    saved.hub.as_mut().unwrap().certificate_pem = None;
    reopened.xiaomi().fixture_replace(&saved).unwrap();
    drop(reopened);
    assert_eq!(
        Store::open(dir.path()).unwrap().xiaomi().load().unwrap(),
        Some(saved)
    );
}

#[test]
fn malformed_credentials_can_be_removed_without_loading_them() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store.xiaomi().fixture_replace(&record("valid")).unwrap();
    Connection::open(store.path())
        .unwrap()
        .execute("UPDATE xiaomi_account SET region = 'us'", [])
        .unwrap();
    assert!(store.xiaomi().load().is_err());
    store.xiaomi().logout().unwrap();
    assert!(store.xiaomi().load().unwrap().is_none());
    store.xiaomi().logout().unwrap();
}

#[test]
fn malformed_single_account_slot_is_not_treated_as_signed_out() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let identity = store.load_identity().unwrap();
    store.matter().put(7, b"paired").unwrap();
    store.xiaomi().fixture_replace(&record("valid")).unwrap();
    let db = Connection::open(store.path()).unwrap();
    db.execute_batch("PRAGMA ignore_check_constraints = ON; UPDATE xiaomi_account SET id = 2;")
        .unwrap();
    assert!(store.xiaomi().load().is_err());
    store.xiaomi().logout().unwrap();
    assert_eq!(
        db.query_row("SELECT count(*) FROM xiaomi_account", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(store.load_identity().unwrap(), identity);
    assert_eq!(
        store.matter().get(7).unwrap().as_deref(),
        Some(b"paired".as_slice())
    );
}

#[test]
fn rejects_invalid_semantics_without_overwriting_credentials() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let original = record("original");
    store.xiaomi().fixture_replace(&original).unwrap();
    let db = Connection::open(store.path()).unwrap();
    db.execute("UPDATE xiaomi_account SET region = 'us'", [])
        .unwrap();
    assert!(store.xiaomi().load().is_err());
    db.execute("UPDATE xiaomi_account SET region = 'cn'", [])
        .unwrap();
    let mut invalid_tokens = tokens("invalid");
    invalid_tokens.refresh_at = invalid_tokens.expires_at + 1;
    assert!(store.xiaomi().update_tokens(&invalid_tokens).is_err());
    let mut invalid_token = tokens("invalid");
    invalid_token.access_token = "invalid token".into();
    assert!(store.xiaomi().update_tokens(&invalid_token).is_err());
    assert!(
        store
            .xiaomi()
            .update_recovery(
                &original.recovery.as_ref().unwrap().credentials,
                &session("other-uid", "other"),
            )
            .is_err()
    );
    let mut invalid_redirect = original.ha.as_ref().unwrap().authorization.clone().unwrap();
    invalid_redirect.redirect_uri =
        "http://evil.invalid/api/webhook/0123456789abcdef0123456789abcdef".into();
    assert!(
        store
            .xiaomi()
            .commit_login(
                &original.recovery.as_ref().unwrap().credentials,
                original.mijia.as_ref().unwrap(),
                UUID,
                &invalid_redirect
            )
            .is_err()
    );
    assert_eq!(store.xiaomi().load().unwrap(), Some(original));
}

#[cfg(unix)]
#[test]
fn xiaomi_credentials_remain_inside_private_storage() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("private");
    fs::create_dir(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    Store::open(&dir)
        .unwrap()
        .xiaomi()
        .fixture_replace(&record("secret"))
        .unwrap();
    assert_eq!(
        fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(dir.join("state.db"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}
