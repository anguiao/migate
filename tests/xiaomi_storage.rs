use migate::storage::{Store, TokenSet, XiaomiRecord};
use rusqlite::Connection;
use std::{error::Error as _, fs};

fn tokens(suffix: &str) -> TokenSet {
    TokenSet {
        access_token: format!("access-{suffix}"),
        refresh_token: format!("refresh-{suffix}"),
        expires_at: 2_000_000_000,
        refresh_at: 1_900_000_000,
    }
}

fn record(suffix: &str) -> XiaomiRecord {
    XiaomiRecord {
        uid: format!("uid-{suffix}"),
        region: "cn".into(),
        oauth_client_uuid: "550e8400-e29b-41d4-a716-446655440000".into(),
        redirect_uri: "http://homeassistant.local:8123/api/webhook/callback".into(),
        tokens: tokens(suffix),
        virtual_did: "18446744073709551615".into(),
        private_key_pem: format!("private-{suffix}"),
        certificate_pem: format!("certificate-{suffix}"),
    }
}

#[test]
fn credentials_persist_and_replace_as_one_record() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    assert!(store.xiaomi().load().unwrap().is_none());
    let first = record("first");
    store.xiaomi().replace(&first).unwrap();
    assert_eq!(store.xiaomi().load().unwrap(), Some(first.clone()));
    let second = record("second");
    store.xiaomi().replace(&second).unwrap();
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
    credentials.private_key_pem =
        "-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIA==\n-----END PRIVATE KEY-----\n".into();
    credentials.certificate_pem =
        "-----BEGIN CERTIFICATE-----\nMIIBAA==\n-----END CERTIFICATE-----\n".into();
    let debug = format!("{credentials:?} {:?}", credentials.tokens);
    for secret in [
        credentials.tokens.access_token.as_str(),
        credentials.tokens.refresh_token.as_str(),
        credentials.redirect_uri.as_str(),
        credentials.private_key_pem.as_str(),
        credentials.certificate_pem.as_str(),
    ] {
        assert!(!debug.contains(secret));
    }

    store.xiaomi().replace(&credentials).unwrap();
    assert_eq!(store.xiaomi().load().unwrap(), Some(credentials));
}

#[test]
fn token_and_certificate_updates_change_only_their_fields() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let original = record("original");
    store.xiaomi().replace(&original).unwrap();

    let replacement_tokens = tokens("updated");
    store.xiaomi().update_tokens(&replacement_tokens).unwrap();
    let after_tokens = store.xiaomi().load().unwrap().unwrap();
    assert_eq!(after_tokens.tokens, replacement_tokens);
    assert_eq!(after_tokens.private_key_pem, original.private_key_pem);
    assert_eq!(after_tokens.certificate_pem, original.certificate_pem);

    store
        .xiaomi()
        .update_certificate("certificate-updated")
        .unwrap();
    let after_certificate = store.xiaomi().load().unwrap().unwrap();
    assert_eq!(after_certificate.certificate_pem, "certificate-updated");
    assert_eq!(after_certificate.tokens, replacement_tokens);
    assert_eq!(after_certificate.private_key_pem, original.private_key_pem);
}

#[test]
fn rejected_transactional_writes_preserve_the_original_and_hide_secrets() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let original = record("original");
    store.xiaomi().replace(&original).unwrap();
    let db = Connection::open(store.path()).unwrap();
    db.execute_batch(
        "CREATE TRIGGER reject_xiaomi_update AFTER UPDATE ON xiaomi_auth BEGIN
            SELECT RAISE(ABORT, 'SECRET CONTENT');
        END;",
    )
    .unwrap();

    for result in [
        store.xiaomi().replace(&record("SECRET CONTENT")),
        store.xiaomi().update_tokens(&tokens("SECRET CONTENT")),
        store.xiaomi().update_certificate("SECRET CONTENT"),
    ] {
        let error = result.unwrap_err();
        assert!(error.source().unwrap().is::<rusqlite::Error>());
        assert!(!format!("{error:?} {error}").contains("SECRET CONTENT"));
        assert_eq!(store.xiaomi().load().unwrap(), Some(original.clone()));
    }
}

#[test]
fn malformed_credentials_can_be_removed_without_loading_them() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store.xiaomi().replace(&record("valid")).unwrap();
    let db = Connection::open(store.path()).unwrap();
    db.execute("UPDATE xiaomi_auth SET region = 'us'", [])
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
    store.xiaomi().replace(&record("valid")).unwrap();
    let db = Connection::open(store.path()).unwrap();
    db.execute_batch("PRAGMA ignore_check_constraints = ON; UPDATE xiaomi_auth SET id = 2;")
        .unwrap();

    assert!(store.xiaomi().load().is_err());
    store.xiaomi().logout().unwrap();
    assert_eq!(
        db.query_row("SELECT count(*) FROM xiaomi_auth", [], |row| row
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
    store.xiaomi().replace(&original).unwrap();

    let mut invalid = record("invalid");
    invalid.region = "us".into();
    assert!(store.xiaomi().replace(&invalid).is_err());
    let mut invalid_tokens = tokens("invalid");
    invalid_tokens.refresh_at = invalid_tokens.expires_at + 1;
    assert!(store.xiaomi().update_tokens(&invalid_tokens).is_err());
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
        .replace(&record("secret"))
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

#[test]
fn stale_cross_connection_writes_cannot_replace_new_credentials_or_logout() {
    let dir = tempfile::tempdir().unwrap();
    let first = Store::open(dir.path()).unwrap();
    let second = Store::open(dir.path()).unwrap();
    first.xiaomi().replace(&record("old")).unwrap();
    let stale = first.xiaomi().snapshot().unwrap().revision;
    second.xiaomi().replace(&record("new")).unwrap();
    assert!(
        first
            .xiaomi()
            .replace_if_revision(stale, &record("stale"))
            .unwrap()
            .is_none()
    );
    assert_eq!(first.xiaomi().load().unwrap(), Some(record("new")));
    let stale = first.xiaomi().snapshot().unwrap().revision;
    second.xiaomi().logout().unwrap();
    assert!(
        first
            .xiaomi()
            .update_tokens_if_revision(stale, &tokens("stale"))
            .unwrap()
            .is_none()
    );
    assert!(first.xiaomi().load().unwrap().is_none());
}
