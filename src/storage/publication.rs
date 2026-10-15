//! Atomic persistence of one complete source configuration.
//!
//! The publication transaction writes the full current picture in one commit:
//! device descriptions, identities (including retained unpublished ones),
//! endpoint allocations, confirmed group relations and the bridge root
//! ConfigurationVersion. A failed commit leaves every previous row untouched.
//! Deletion/permission revocations are written separately before the main
//! commit and cleared only by a newer authorized re-admission in that commit.

use super::{
    DeviceStore, StorageError,
    device::{
        CachedPhysicalDevice, CachedScope, load_cached, save_cached, validate_cached, validate_key,
        validate_text,
    },
    registry::{StoredDevice, load_stored, validate_label},
};
use crate::device::{DeviceKey, FunctionDescription};
use rusqlite::{OptionalExtension as _, Transaction, params};
use std::collections::BTreeSet;

/// One confirmed native group relation of the active scope. Confirmed but
/// empty, currently unpublishable or missing-own-instance-type relations are
/// retained unchanged so a restart never loses a confirmed relation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredRelation {
    pub group_did: String,
    pub record_did: String,
    pub model: String,
    pub instance_type: Option<String>,
    pub is_physical: bool,
    pub active: bool,
    pub error: Option<String>,
    pub members: Vec<String>,
}

/// One durable deletion/permission revocation tombstone.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredRevocation {
    pub key: DeviceKey,
    pub reason: String,
}

/// Everything persisted about the published configuration, including retained
/// unpublished identities needed to restore names, endpoint numbers and
/// versions without republishing.
#[derive(Clone, Debug)]
pub struct PublicationState {
    pub root_version: u32,
    pub records: Vec<StoredDevice>,
    pub cached: Vec<CachedPhysicalDevice>,
    pub relations: Vec<StoredRelation>,
    pub revocations: Vec<StoredRevocation>,
}

/// The complete next configuration, applied in one SQL transaction.
#[derive(Clone, Debug)]
pub struct PublicationCommit {
    pub scope: CachedScope,
    pub cached: Vec<CachedPhysicalDevice>,
    pub records: Vec<StoredDevice>,
    pub relations: Vec<StoredRelation>,
    pub root_version: u32,
    /// Revocations cleared by a genuine fresh re-admission committed here.
    pub clear_revocations: Vec<DeviceKey>,
}

impl DeviceStore {
    /// Load the persisted configuration; descriptions and relations need the active scope.
    pub fn load_publication(
        &self,
        scope: Option<&CachedScope>,
    ) -> Result<PublicationState, StorageError> {
        let operation = "load published configuration";
        let connection = self.store.connection();
        let root_version =
            load_root(connection).map_err(|error| self.store.database_error(operation, error))?;
        let keys = connection
            .prepare("SELECT source,device_id FROM device_identity ORDER BY endpoint")
            .and_then(|mut statement| {
                statement
                    .query_map([], |row| {
                        Ok(DeviceKey::new(
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .map_err(|error| self.store.database_error(operation, error))?;
        let records = keys
            .iter()
            .map(|key| {
                load_stored(connection, key)
                    .and_then(|record| record.ok_or(rusqlite::Error::InvalidQuery))
                    .map_err(|error| self.store.database_error(operation, error))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut cached = Vec::new();
        let mut relations = Vec::new();
        if let Some(scope) = scope {
            self.load_scope_publication(scope, &mut cached, &mut relations)
                .map_err(|error| self.store.database_error(operation, error))?;
        }
        let revocations = connection
            .prepare("SELECT source,device_id,reason FROM publication_revocation")
            .and_then(|mut statement| {
                statement
                    .query_map([], |row| {
                        Ok(StoredRevocation {
                            key: DeviceKey::new(row.get::<_, String>(0)?, row.get::<_, String>(1)?),
                            reason: row.get::<_, String>(2)?,
                        })
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .map_err(|error| self.store.database_error(operation, error))?;
        Ok(PublicationState {
            root_version,
            records,
            cached,
            relations,
            revocations,
        })
    }

    /// Persist one complete configuration in a single transaction.
    pub fn apply_publication(&self, commit: &PublicationCommit) -> Result<(), StorageError> {
        let operation = "commit published configuration";
        commit
            .validate()
            .map_err(|error| self.invalid(operation, error))?;
        let transaction = self
            .store
            .connection()
            .unchecked_transaction()
            .map_err(|error| self.store.database_error(operation, error))?;
        // RAII rolls the whole transaction back when any statement fails.
        self.apply_in(&transaction, commit)?;
        transaction
            .commit()
            .map_err(|error| self.store.database_error(operation, error))?;
        Ok(())
    }

    /// Write focused revocation tombstones before the main configuration commit.
    pub fn record_revocations(&self, revocations: &[StoredRevocation]) -> Result<(), StorageError> {
        let operation = "record revocation";
        for revocation in revocations {
            validate_key(&revocation.key)
                .and_then(|()| validate_text(&revocation.reason))
                .map_err(|error| self.invalid(operation, error))?;
        }
        let transaction = self
            .store
            .connection()
            .unchecked_transaction()
            .map_err(|error| self.store.database_error(operation, error))?;
        for revocation in revocations {
            transaction
                .execute(
                    "INSERT OR REPLACE INTO publication_revocation VALUES (?1,?2,?3)",
                    params![revocation.key.source, revocation.key.id, revocation.reason],
                )
                .map_err(|error| self.store.database_error(operation, error))?;
        }
        transaction
            .commit()
            .map_err(|error| self.store.database_error(operation, error))
    }

    /// Next free endpoint after every allocation ever made, deleted ones included.
    pub fn next_endpoint(&self) -> Result<u16, StorageError> {
        let operation = "load endpoint allocation";
        let last = self
            .store
            .connection()
            .query_row(
                "SELECT COALESCE(MAX(endpoint),1) FROM endpoint_allocation",
                [],
                |row| row.get::<_, u16>(0),
            )
            .map_err(|error| self.store.database_error(operation, error))?;
        last.checked_add(1)
            .ok_or_else(|| self.invalid(operation, "Device endpoint numbers are exhausted"))
    }

    fn load_scope_publication(
        &self,
        scope: &CachedScope,
        cached: &mut Vec<CachedPhysicalDevice>,
        relations: &mut Vec<StoredRelation>,
    ) -> rusqlite::Result<()> {
        let connection = self.store.connection();
        let keys = connection
            .prepare("SELECT source,device_id FROM device_cache WHERE uid=?1 AND home_id=?2")
            .and_then(|mut statement| {
                statement
                    .query_map(params![scope.uid, scope.home_id], |row| {
                        Ok(DeviceKey::new(
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()
            })?;
        for key in keys {
            if let Some(description) = load_cached(connection, &scope.uid, &scope.home_id, &key)? {
                cached.push(description);
            }
        }
        let groups = connection
            .prepare(
                "SELECT group_did,record_did,model,instance_type,is_physical,active,error
                 FROM native_group WHERE uid=?1 AND home_id=?2 ORDER BY group_did",
            )
            .and_then(|mut statement| {
                statement
                    .query_map(params![scope.uid, scope.home_id], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, Option<String>>(3)?,
                            row.get::<_, bool>(4)?,
                            row.get::<_, bool>(5)?,
                            row.get::<_, Option<String>>(6)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()
            })?;
        for (group_did, record_did, model, instance_type, is_physical, active, error) in groups {
            let members = connection
                .prepare(
                    "SELECT member_did FROM native_group_member
                     WHERE uid=?1 AND home_id=?2 AND group_did=?3 ORDER BY member_did",
                )
                .and_then(|mut statement| {
                    statement
                        .query_map(params![scope.uid, scope.home_id, group_did], |row| {
                            row.get::<_, String>(0)
                        })?
                        .collect::<rusqlite::Result<Vec<_>>>()
                })?;
            relations.push(StoredRelation {
                group_did,
                record_did,
                model,
                instance_type,
                is_physical,
                active,
                error,
                members,
            });
        }
        Ok(())
    }

    fn apply_in(
        &self,
        transaction: &Transaction<'_>,
        commit: &PublicationCommit,
    ) -> Result<(), StorageError> {
        let operation = "commit published configuration";
        let current =
            load_root(transaction).map_err(|error| self.store.database_error(operation, error))?;
        if commit.root_version < current {
            return Err(self.invalid(operation, "Published root version went backwards"));
        }
        transaction
            .execute(
                "INSERT INTO xiaomi_scope VALUES (?1,?2,?3)
                 ON CONFLICT(uid) DO UPDATE SET home_id=excluded.home_id, group_id=excluded.group_id",
                params![commit.scope.uid, commit.scope.home_id, commit.scope.group_id],
            )
            .map_err(|error| self.store.database_error(operation, error))?;
        for record in &commit.records {
            self.upsert_record(transaction, record)
                .map_err(|error| self.store.database_error(operation, error))?;
        }
        let committed = commit
            .records
            .iter()
            .map(|record| record.key.clone())
            .collect::<BTreeSet<_>>();
        let published = transaction
            .prepare("SELECT source,device_id FROM device_identity WHERE published=1")
            .map_err(|error| self.store.database_error(operation, error))?
            .query_map([], |row| {
                Ok(DeviceKey::new(
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                ))
            })
            .map_err(|error| self.store.database_error(operation, error))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|error| self.store.database_error(operation, error))?;
        for key in published {
            if !committed.contains(&key) {
                transaction
                    .execute(
                        "UPDATE device_identity SET published=0 WHERE source=?1 AND device_id=?2",
                        params![key.source, key.id],
                    )
                    .map_err(|error| self.store.database_error(operation, error))?;
            }
        }
        transaction
            .execute(
                "DELETE FROM device_cache WHERE uid=?1 AND home_id=?2",
                params![commit.scope.uid, commit.scope.home_id],
            )
            .map_err(|error| self.store.database_error(operation, error))?;
        for cached in &commit.cached {
            save_cached(transaction, cached)
                .map_err(|error| self.store.database_error(operation, error))?;
        }
        transaction
            .execute(
                "DELETE FROM native_group WHERE uid=?1 AND home_id=?2",
                params![commit.scope.uid, commit.scope.home_id],
            )
            .map_err(|error| self.store.database_error(operation, error))?;
        transaction
            .execute(
                "DELETE FROM native_group_member WHERE uid=?1 AND home_id=?2",
                params![commit.scope.uid, commit.scope.home_id],
            )
            .map_err(|error| self.store.database_error(operation, error))?;
        for relation in &commit.relations {
            transaction
                .execute(
                    "INSERT INTO native_group VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                    params![
                        commit.scope.uid,
                        commit.scope.home_id,
                        relation.group_did,
                        relation.record_did,
                        relation.model,
                        relation.instance_type,
                        relation.is_physical,
                        relation.active,
                        relation.error
                    ],
                )
                .map_err(|error| self.store.database_error(operation, error))?;
            for member in &relation.members {
                transaction
                    .execute(
                        "INSERT INTO native_group_member VALUES (?1,?2,?3,?4)",
                        params![
                            commit.scope.uid,
                            commit.scope.home_id,
                            relation.group_did,
                            member
                        ],
                    )
                    .map_err(|error| self.store.database_error(operation, error))?;
            }
        }
        for key in &commit.clear_revocations {
            transaction
                .execute(
                    "DELETE FROM publication_revocation WHERE source=?1 AND device_id=?2",
                    params![key.source, key.id],
                )
                .map_err(|error| self.store.database_error(operation, error))?;
        }
        if commit.root_version > current {
            transaction
                .execute(
                    "UPDATE publication_root SET configuration_version=?1 WHERE id=1",
                    [commit.root_version],
                )
                .map_err(|error| self.store.database_error(operation, error))?;
        }
        Ok(())
    }

    fn upsert_record(
        &self,
        transaction: &Transaction<'_>,
        record: &StoredDevice,
    ) -> rusqlite::Result<()> {
        match load_stored(transaction, &record.key)? {
            Some(existing) => {
                if existing.unique_id != record.unique_id || existing.endpoint != record.endpoint {
                    return Err(rusqlite::Error::InvalidQuery);
                }
                if record.configuration_version < existing.configuration_version {
                    return Err(rusqlite::Error::InvalidQuery);
                }
                // The source topology of a stored identity is static: the
                // function key set is unchanged and every key keeps its
                // reserved endpoint. No generic topology migration exists.
                if existing.functions.len() != record.functions.len()
                    || existing.functions.iter().any(|previous| {
                        !record
                            .functions
                            .iter()
                            .any(|function| function.key.id == previous.key.id)
                    })
                {
                    return Err(rusqlite::Error::InvalidQuery);
                }
                for function in &record.functions {
                    if let Some(previous) = existing
                        .functions
                        .iter()
                        .find(|previous| previous.key.id == function.key.id)
                        && previous.endpoint != function.endpoint
                    {
                        return Err(rusqlite::Error::InvalidQuery);
                    }
                }
                transaction.execute(
                    "UPDATE device_identity SET label=?3,published=?4,configuration_version=?5
                     WHERE source=?1 AND device_id=?2",
                    params![
                        record.key.source,
                        record.key.id,
                        record.label,
                        record.published,
                        record.configuration_version
                    ],
                )?;
            }
            None => {
                transaction.execute(
                    "INSERT INTO device_identity
                     (source,device_id,unique_id,endpoint,label,published,configuration_version)
                     VALUES (?1,?2,?3,?4,?5,?6,?7)",
                    params![
                        record.key.source,
                        record.key.id,
                        record.unique_id,
                        record.endpoint,
                        record.label,
                        record.published,
                        record.configuration_version
                    ],
                )?;
                let single = record.functions.len() == 1;
                let root_slot = if single {
                    record.functions[0].key.id.as_str()
                } else {
                    ""
                };
                transaction.execute(
                    "INSERT INTO endpoint_allocation VALUES (?1,?2,?3,?4)",
                    params![record.endpoint, record.key.source, record.key.id, root_slot],
                )?;
                if !single {
                    for function in &record.functions {
                        transaction.execute(
                            "INSERT INTO endpoint_allocation VALUES (?1,?2,?3,?4)",
                            params![
                                function.endpoint,
                                record.key.source,
                                record.key.id,
                                function.key.id
                            ],
                        )?;
                    }
                }
            }
        }
        transaction.execute(
            "DELETE FROM device_function WHERE source=?1 AND device_id=?2",
            params![record.key.source, record.key.id],
        )?;
        for function in &record.functions {
            transaction.execute(
                "INSERT INTO device_function VALUES (?1,?2,?3,?4,?5,?6,?7)",
                params![
                    record.key.source,
                    record.key.id,
                    function.key.id,
                    function.endpoint,
                    function.label,
                    match function.category {
                        crate::device::Category::Outlet => "outlet",
                        crate::device::Category::Light => "light",
                    },
                    serde_json::to_string(&function.capabilities)
                        .map_err(|_| rusqlite::Error::InvalidQuery)?,
                ],
            )?;
        }
        Ok(())
    }
}

impl PublicationCommit {
    fn validate(&self) -> Result<(), &'static str> {
        validate_text(&self.scope.uid)?;
        validate_text(&self.scope.home_id)?;
        validate_text(&self.scope.group_id)?;
        if self.root_version == 0 {
            return Err("Published root version is invalid");
        }
        let mut endpoints = BTreeSet::new();
        let mut keys = BTreeSet::new();
        for record in &self.records {
            validate_record(record)?;
            if !keys.insert(record.key.clone()) {
                return Err("Published device identity is duplicated");
            }
            if !endpoints.insert(record.endpoint) {
                return Err("Published endpoint is duplicated");
            }
            if record.functions.len() > 1 {
                for function in &record.functions {
                    if !endpoints.insert(function.endpoint) {
                        return Err("Published endpoint is duplicated");
                    }
                }
            }
        }
        for cached in &self.cached {
            validate_cached(cached)?;
            if cached.uid != self.scope.uid
                || cached.home_id != self.scope.home_id
                || cached.group_id != self.scope.group_id
            {
                return Err("Published description belongs to another home");
            }
        }
        let mut groups = BTreeSet::new();
        for relation in &self.relations {
            validate_text(&relation.group_did)?;
            validate_text(&relation.record_did)?;
            validate_text(&relation.model)?;
            if let Some(instance_type) = &relation.instance_type {
                validate_text(instance_type)?;
            }
            if let Some(error) = &relation.error {
                validate_text(error)?;
            }
            if !groups.insert(&relation.group_did) {
                return Err("Published group relation is duplicated");
            }
            for member in &relation.members {
                validate_text(member)?;
            }
        }
        for key in &self.clear_revocations {
            validate_key(key)?;
        }
        Ok(())
    }
}

fn validate_record(record: &StoredDevice) -> Result<(), &'static str> {
    validate_key(&record.key)?;
    if !(2..=65534).contains(&record.endpoint)
        || !uuid::Uuid::parse_str(&record.unique_id)
            .is_ok_and(|uuid| uuid.simple().to_string() == record.unique_id)
        || record.configuration_version == 0
        || validate_label(&record.label).is_err()
        || record.published && record.label.is_empty()
        || record.functions.is_empty()
    {
        return Err("Published device identity is invalid");
    }
    let single = record.functions.len() == 1;
    let mut ids = BTreeSet::new();
    for function in &record.functions {
        if !ids.insert(&function.key.id)
            || function.key.device != record.key
            || !(2..=65534).contains(&function.endpoint)
            || validate_label(&function.label).is_err()
            || !(FunctionDescription {
                id: function.key.id.clone(),
                label: function.label.clone(),
                category: function.category,
                capabilities: function.capabilities.clone(),
            })
            .valid_capabilities()
        {
            return Err("Published device function is invalid");
        }
        if single {
            if function.endpoint != record.endpoint {
                return Err("Published device function endpoint is invalid");
            }
        } else if function.endpoint <= record.endpoint {
            return Err("Published device function endpoint is invalid");
        }
    }
    if !single {
        let children = record
            .functions
            .iter()
            .map(|function| function.endpoint)
            .collect::<BTreeSet<_>>();
        if children.len() != record.functions.len() {
            return Err("Published device function endpoint is duplicated");
        }
    }
    Ok(())
}

fn load_root(connection: &rusqlite::Connection) -> rusqlite::Result<u32> {
    let version = connection
        .query_row(
            "SELECT configuration_version FROM publication_root WHERE id=1",
            [],
            |row| row.get::<_, u32>(0),
        )
        .optional()?;
    version
        .filter(|version| *version > 0)
        .ok_or(rusqlite::Error::InvalidQuery)
}
