use super::*;

mod ha;
mod hub_certificate;
mod mijia;

#[test]
fn checking_credential_snapshots_does_not_query_storage() {
    let rig = Rig::new(true, &["one"]);
    let path = rig.source.context.mijia_path().unwrap();
    let key = rig.source.context.ha_key().unwrap();
    // Make a fresh storage lookup fail after the adapter captured credentials.
    rusqlite::Connection::open(rig.store.path())
        .unwrap()
        .execute(
            "ALTER TABLE xiaomi_account RENAME TO unavailable_account",
            [],
        )
        .unwrap();
    assert!(rig.source.context.current_mijia(&path));
    assert!(rig.source.context.current_ha(&key));
}
