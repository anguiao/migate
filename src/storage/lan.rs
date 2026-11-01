use super::{StorageError, Store};
use crate::{configuration::Scope, xiaomi::lan::Token};
use rusqlite::OptionalExtension;
use std::net::Ipv4Addr;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LanRecord {
    pub(crate) scope: Scope,
    pub(crate) did: String,
    pub(crate) model: String,
    pub(crate) address: Ipv4Addr,
    pub(crate) token: Token,
}

pub(crate) struct LanStore(Store);

impl LanStore {
    pub(super) fn new(store: Store) -> Self {
        Self(store)
    }

    pub(crate) fn load(&self, scope: &Scope, did: &str) -> Result<Option<LanRecord>, StorageError> {
        let row: Option<(String, String, Vec<u8>)> = self.0.connection().query_row(
            "SELECT model, address, token FROM xiaomi_lan WHERE uid = ?1 AND home_id = ?2 AND did = ?3",
            (&scope.uid, &scope.home_id, did),
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).optional().map_err(|error| self.0.database_error("load LAN credentials", error))?;
        row.map(|(model, address, token)| {
            let address: Ipv4Addr = address.parse().map_err(|_| {
                StorageError::new(self.0.path(), "validate LAN credentials", "invalid address")
            })?;
            if address.is_unspecified() || address.is_multicast() || address.is_broadcast() {
                return Err(StorageError::new(
                    self.0.path(),
                    "validate LAN credentials",
                    "invalid address",
                ));
            }
            let token = token.try_into().map(Token::from_bytes).map_err(|_| {
                StorageError::new(self.0.path(), "validate LAN credentials", "invalid token")
            })?;
            Ok(LanRecord {
                scope: scope.clone(),
                did: did.into(),
                model,
                address,
                token,
            })
        })
        .transpose()
    }

    /// Only a successfully authenticated record may be supplied here.
    pub(crate) fn save(&self, record: &LanRecord) -> Result<(), StorageError> {
        self.0.connection().execute(
            "INSERT INTO xiaomi_lan (did, uid, home_id, model, address, token) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (did) DO UPDATE SET uid=excluded.uid, home_id=excluded.home_id, model=excluded.model, address=excluded.address, token=excluded.token",
            (&record.did, &record.scope.uid, &record.scope.home_id, &record.model, record.address.to_string(), record.token.bytes().as_slice()),
        ).map(|_| ()).map_err(|error| self.0.database_error("save LAN credentials", error))
    }

    pub(crate) fn remove_scope(&self, scope: &Scope) -> Result<(), StorageError> {
        self.0
            .connection()
            .execute(
                "DELETE FROM xiaomi_lan WHERE uid = ?1 AND home_id = ?2",
                (&scope.uid, &scope.home_id),
            )
            .map(|_| ())
            .map_err(|error| self.0.database_error("remove LAN scope", error))
    }

    pub(crate) fn remove(&self, did: &str) -> Result<(), StorageError> {
        self.0
            .connection()
            .execute("DELETE FROM xiaomi_lan WHERE did = ?1", [did])
            .map(|_| ())
            .map_err(|error| self.0.database_error("remove LAN credentials", error))
    }
}
