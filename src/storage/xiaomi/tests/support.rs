use super::super::*;

pub(crate) trait XiaomiTestFixture {
    fn fixture_replace(&self, record: &XiaomiRecord) -> Result<(), StorageError>;
    fn fixture_update_tokens(&self, tokens: &TokenSet) -> Result<(), StorageError>;
}

impl XiaomiTestFixture for XiaomiStore {
    /// Seed an account or simulate an external credential replacement.
    fn fixture_replace(&self, record: &XiaomiRecord) -> Result<(), StorageError> {
        self.validate(record)?;
        (|| {
            let tx = self.store.connection().unchecked_transaction()?;
            tx.execute("DELETE FROM xiaomi_lan WHERE uid <> ?1", [&record.uid])?;
            tx.execute(
                "INSERT INTO xiaomi_account (id, uid, region) VALUES (1, ?1, ?2)
                ON CONFLICT (id) DO UPDATE SET uid = excluded.uid, region = excluded.region",
                (&record.uid, &record.region),
            )?;
            for table in [
                "xiaomi_account_recovery",
                "xiaomi_mijia_session",
                "xiaomi_ha_oauth",
                "xiaomi_ha_client",
                "xiaomi_hub",
            ] {
                tx.execute(&format!("DELETE FROM {table}"), [])?;
            }
            write_recovery(&tx, &record.recovery.credentials, record.recovery.stop)?;
            write_session(&tx, &record.mijia)?;
            write_ha(&tx, &record.ha)?;
            if let Some(hub) = &record.hub {
                write_hub(&tx, hub)?;
            }
            tx.commit()
        })()
        .map_err(|error| {
            self.store
                .database_error("replace Xiaomi credentials", error)
        })?;
        if let Some(state) = self.store.xiaomi_state().borrow_mut().as_mut() {
            state.record = Some(record.clone());
            state.mijia_generation = state.mijia_generation.wrapping_add(1);
            state.ha_generation = state.ha_generation.wrapping_add(1);
        }
        Ok(())
    }

    fn fixture_update_tokens(&self, tokens: &TokenSet) -> Result<(), StorageError> {
        self.update_tokens(tokens)
    }
}

pub(crate) fn credentials(uid: &str) -> XiaomiRecord {
    XiaomiRecord {
        uid: uid.into(),
        region: "cn".into(),
        recovery: AccountRecovery {
            credentials: AccountCredentials::new(
                uid.into(),
                "cuser".into(),
                "pass".into(),
                AccountClientIdentity::new("device".into(), "test-agent".into()).unwrap(),
            )
            .unwrap(),
            stop: None,
        },
        mijia: MijiaSession::new(
            uid.into(),
            "service".into(),
            "c2VjdXJpdHk=".into(),
            "test-agent".into(),
        )
        .unwrap(),
        ha: HaClient {
            client_uuid: "550e8400-e29b-41d4-a716-446655440000".into(),
            authorization: HaAuthorization {
                redirect_uri:
                    "http://homeassistant.local:8123/api/webhook/0123456789abcdef0123456789abcdef"
                        .into(),
                tokens: TokenSet {
                    access_token: "access".into(),
                    refresh_token: "refresh".into(),
                    expires_at: 2_000_000_000,
                    refresh_at: 1_900_000_000,
                },
            },
            stopped: false,
        },
        hub: None,
    }
}

fn write_hub(db: &Connection, hub: &HubIdentity) -> rusqlite::Result<()> {
    db.execute("INSERT INTO xiaomi_hub (id, virtual_did, private_key_pem, certificate_pem) VALUES (1, ?1, ?2, ?3)
        ON CONFLICT(id) DO UPDATE SET virtual_did=excluded.virtual_did, private_key_pem=excluded.private_key_pem, certificate_pem=excluded.certificate_pem",
        (&hub.virtual_did, &hub.private_key_pem, &hub.certificate_pem))?;
    Ok(())
}
