#[test]
fn confirmed_records_are_scoped_and_survive_reopen() {
    use crate::xiaomi::lan::Token;
    use crate::{configuration::Scope, storage::Store};
    let dir = tempfile::tempdir().unwrap();
    let scope = Scope {
        uid: "owner".into(),
        home_id: "home".into(),
    };
    let other = Scope {
        uid: "owner".into(),
        home_id: "other".into(),
    };
    let record = crate::storage::LanRecord {
        scope: scope.clone(),
        did: "656878122".into(),
        model: "dmaker.fan.p5c".into(),
        address: "192.0.2.5".parse().unwrap(),
        token: Token::parse("00112233445566778899aabbccddeeff").unwrap(),
    };
    let store = Store::open(dir.path()).unwrap();
    store.lan().save(&record).unwrap();
    drop(store);
    let store = Store::open(dir.path()).unwrap();
    assert_eq!(
        store.lan().load(&scope, &record.did).unwrap(),
        Some(record.clone())
    );
    assert_eq!(store.lan().load(&other, &record.did).unwrap(), None);
    store.lan().remove(&record.did).unwrap();
    assert_eq!(store.lan().load(&scope, &record.did).unwrap(), None);
}

#[test]
fn logout_erases_confirmed_lan_credentials() {
    use crate::{configuration::Scope, storage::Store, xiaomi::lan::Token};
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let scope = Scope {
        uid: "owner".into(),
        home_id: "home".into(),
    };
    let record = crate::storage::LanRecord {
        scope: scope.clone(),
        did: "656878122".into(),
        model: "dmaker.fan.p5c".into(),
        address: "192.0.2.5".parse().unwrap(),
        token: Token::parse("00112233445566778899aabbccddeeff").unwrap(),
    };
    store.lan().save(&record).unwrap();
    store.xiaomi().logout().unwrap();
    assert_eq!(store.lan().load(&scope, &record.did).unwrap(), None);
}

#[test]
fn changing_account_erases_old_lan_records_atomically() {
    use crate::{
        configuration::Scope,
        storage::{Store, TokenSet, XiaomiRecord},
        xiaomi::lan::Token,
    };
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let credentials = |uid: &str| XiaomiRecord {
        uid: uid.into(),
        region: "cn".into(),
        oauth_client_uuid: "550e8400-e29b-41d4-a716-446655440000".into(),
        redirect_uri: "http://homeassistant.local:8123/api/webhook/callback".into(),
        tokens: TokenSet {
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            expires_at: 10,
            refresh_at: 5,
        },
        virtual_did: "18446744073709551615".into(),
        private_key_pem: "private".into(),
        certificate_pem: "certificate".into(),
    };
    store.xiaomi().replace(&credentials("owner")).unwrap();
    let scope = Scope {
        uid: "owner".into(),
        home_id: "home".into(),
    };
    let record = crate::storage::LanRecord {
        scope: scope.clone(),
        did: "656878122".into(),
        model: "dmaker.fan.p5c".into(),
        address: "192.0.2.5".parse().unwrap(),
        token: Token::parse("00112233445566778899aabbccddeeff").unwrap(),
    };
    store.lan().save(&record).unwrap();
    store.xiaomi().replace(&credentials("owner")).unwrap();
    assert_eq!(
        store.lan().load(&scope, &record.did).unwrap(),
        Some(record.clone())
    );
    let db = rusqlite::Connection::open(store.path()).unwrap();
    db.execute_batch("CREATE TRIGGER reject_auth BEFORE UPDATE ON xiaomi_auth BEGIN SELECT RAISE(ABORT, 'rejected'); END;").unwrap();
    assert!(
        store
            .xiaomi()
            .replace(&credentials("different-owner"))
            .is_err()
    );
    assert_eq!(
        store.lan().load(&scope, &record.did).unwrap(),
        Some(record.clone())
    );
    db.execute_batch("DROP TRIGGER reject_auth;").unwrap();
    store
        .xiaomi()
        .replace(&credentials("different-owner"))
        .unwrap();
    assert_eq!(store.lan().load(&scope, &record.did).unwrap(), None);
}
