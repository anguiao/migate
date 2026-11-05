use super::*;

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
            if let Some(recovery) = &record.recovery {
                write_recovery(&tx, &recovery.credentials, recovery.stop)?;
            }
            if let Some(session) = &record.mijia {
                write_session(&tx, session)?;
            }
            if let Some(ha) = &record.ha {
                write_ha(&tx, ha)?;
            }
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

fn write_hub(db: &Connection, hub: &HubIdentity) -> rusqlite::Result<()> {
    db.execute("INSERT INTO xiaomi_hub (id, virtual_did, private_key_pem, certificate_pem) VALUES (1, ?1, ?2, ?3)
        ON CONFLICT(id) DO UPDATE SET virtual_did=excluded.virtual_did, private_key_pem=excluded.private_key_pem, certificate_pem=excluded.certificate_pem",
        (&hub.virtual_did, &hub.private_key_pem, &hub.certificate_pem))?;
    Ok(())
}
