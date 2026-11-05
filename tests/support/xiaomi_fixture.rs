use migate::storage::{RecoveryStop, XiaomiRecord, XiaomiStore};
use rusqlite::{Connection, params};

/// Seed a fresh process-test database without exposing unconditional writes in the library.
pub fn seed(store: &XiaomiStore, record: &XiaomiRecord) {
    let mut db = Connection::open(store.path()).unwrap();
    let tx = db.transaction().unwrap();
    tx.execute(
        "INSERT INTO xiaomi_account (id, uid, region) VALUES (1, ?1, ?2)",
        (&record.uid, &record.region),
    )
    .unwrap();
    if let Some(recovery) = &record.recovery {
        let credentials = &recovery.credentials;
        let stop = recovery.stop.map(|stop| match stop {
            RecoveryStop::Rejected => "rejected",
            RecoveryStop::VerificationRequired => "verification_required",
        });
        tx.execute("INSERT INTO xiaomi_account_recovery (id, user_id, c_user_id, pass_token, device_id, user_agent, stopped) VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6)",
            params![credentials.user_id(), credentials.c_user_id(), credentials.pass_token(), credentials.identity().device_id(), credentials.identity().user_agent(), stop]).unwrap();
    }
    if let Some(session) = &record.mijia {
        tx.execute("INSERT INTO xiaomi_mijia_session (id, user_id, service_token, ssecurity, user_agent) VALUES (1, ?1, ?2, ?3, ?4)",
            params![session.user_id(), session.service_token(), session.ssecurity(), session.user_agent()]).unwrap();
    }
    if let Some(ha) = &record.ha {
        tx.execute(
            "INSERT INTO xiaomi_ha_client (id, client_uuid, stopped) VALUES (1, ?1, ?2)",
            (&ha.client_uuid, ha.stopped),
        )
        .unwrap();
        if let Some(authorization) = &ha.authorization {
            let tokens = &authorization.tokens;
            tx.execute("INSERT INTO xiaomi_ha_oauth (id, redirect_uri, access_token, refresh_token, expires_at, refresh_at) VALUES (1, ?1, ?2, ?3, ?4, ?5)",
                params![authorization.redirect_uri, tokens.access_token, tokens.refresh_token, tokens.expires_at, tokens.refresh_at]).unwrap();
        }
    }
    if let Some(hub) = &record.hub {
        tx.execute("INSERT INTO xiaomi_hub (id, virtual_did, private_key_pem, certificate_pem) VALUES (1, ?1, ?2, ?3)",
            (&hub.virtual_did, &hub.private_key_pem, &hub.certificate_pem)).unwrap();
    }
    tx.commit().unwrap();
}
