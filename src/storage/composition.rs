use super::{
    StorageError,
    device::{CachedScope, DeviceStore, validate_key, validate_scope, validate_text},
    registry::{compatible_light_member, import_label, reserve_endpoint, validate_label},
};
use crate::device::{Composition, CompositionError, DeviceKey, validate_composition_structure};
use rusqlite::{Connection, OptionalExtension as _, params};
use std::collections::HashSet;
use uuid::Uuid;

/// A published group parent identity and its confirmed current-scope members.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredGroup {
    pub key: DeviceKey,
    pub unique_id: String,
    pub endpoint: u16,
    pub label: String,
    pub members: Vec<DeviceKey>,
}

/// One stored group parent identity, loaded by stable device key without
/// re-deriving the account or home scope. The confirmed relation set is the
/// publication authority; this lookup only resolves the reserved endpoint and
/// the saved bridge identity and name.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredGroupIdentity {
    pub key: DeviceKey,
    pub unique_id: String,
    pub endpoint: u16,
    pub label: String,
}

/// The confirmed relation version of one account/home and its published parents.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfirmedComposition {
    pub version: u64,
    pub groups: Vec<StoredGroup>,
}

/// A rejected composition commit. Rejection leaves stored data unchanged.
#[derive(Clone, Debug)]
pub enum CompositionCommitError {
    Invalid(CompositionError),
    UnsupportedMember(DeviceKey),
    RemovedMember(DeviceKey),
    RemovedGroup(DeviceKey),
    EndpointExhausted,
    Storage(StorageError),
}

impl std::fmt::Display for CompositionCommitError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(error) => error.fmt(formatter),
            Self::UnsupportedMember(key) => write!(
                formatter,
                "Group member {} is not a published supported single light",
                key.id
            ),
            Self::RemovedMember(key) => {
                write!(formatter, "Group member {} is confirmed deleted", key.id)
            }
            Self::RemovedGroup(key) => {
                write!(formatter, "Group parent {} is confirmed deleted", key.id)
            }
            Self::EndpointExhausted => formatter.write_str("Device endpoint numbers are exhausted"),
            Self::Storage(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for CompositionCommitError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Invalid(error) => Some(error),
            Self::Storage(error) => Some(error),
            _ => None,
        }
    }
}

impl From<StorageError> for CompositionCommitError {
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}

impl DeviceStore {
    /// Commit one confirmed relation version for a scope in a single
    /// transaction. Members must be published compatible single lights in the
    /// same scope; the optional removals are confirmed member deletions.
    pub fn commit_composition(
        &self,
        scope: &CachedScope,
        compositions: &[Composition],
        removed: &[DeviceKey],
    ) -> Result<ConfirmedComposition, CompositionCommitError> {
        let operation = "commit Xiaomi group composition";
        validate_scope(&scope.uid, &scope.home_id, &scope.group_id)
            .map_err(|error| self.invalid(operation, error))?;
        for key in removed {
            validate_key(key).map_err(|error| self.invalid(operation, error))?;
        }
        validate_composition_structure(compositions).map_err(CompositionCommitError::Invalid)?;
        let transaction = self
            .store
            .connection()
            .unchecked_transaction()
            .map_err(|error| self.store.database_error(operation, error))?;
        let confirmed = commit_in(self, &transaction, scope, compositions, removed, operation)?;
        transaction
            .commit()
            .map_err(|error| self.store.database_error(operation, error))?;
        Ok(confirmed)
    }

    /// The last confirmed relation version for the account/home. Only groups
    /// whose stored members are compatible published single lights are
    /// returned; member runtime availability is unrelated to this check.
    /// Revoked parents have no relations and never return.
    pub fn restore_groups(
        &self,
        uid: &str,
        home_id: &str,
    ) -> Result<ConfirmedComposition, StorageError> {
        let operation = "restore Xiaomi group composition";
        validate_text(uid)
            .and_then(|()| validate_text(home_id))
            .map_err(|error| self.invalid(operation, error))?;
        let scope = CachedScope {
            uid: uid.into(),
            home_id: home_id.into(),
            group_id: crate::xiaomi::cloud::group_id(uid, home_id),
        };
        let connection = self.store.connection();
        let mut confirmed = load_confirmed(connection, &scope)
            .map_err(|error| self.store.database_error(operation, error))?;
        let mut compatible = Vec::with_capacity(confirmed.groups.len());
        for group in confirmed.groups {
            let mut usable = true;
            for member in &group.members {
                if !compatible_light_member(connection, uid, home_id, member)
                    .map_err(|error| self.store.database_error(operation, error))?
                {
                    usable = false;
                    break;
                }
            }
            if usable {
                compatible.push(group);
            }
        }
        confirmed.groups = compatible;
        Ok(confirmed)
    }

    pub fn load_published_group(
        &self,
        uid: &str,
        home_id: &str,
        key: &DeviceKey,
    ) -> Result<Option<StoredGroup>, StorageError> {
        let operation = "load published Xiaomi group";
        validate_text(uid)
            .and_then(|()| validate_text(home_id))
            .and_then(|()| validate_key(key))
            .map_err(|error| self.invalid(operation, error))?;
        let scope = CachedScope {
            uid: uid.into(),
            home_id: home_id.into(),
            group_id: crate::xiaomi::cloud::group_id(uid, home_id),
        };
        load_confirmed(self.store.connection(), &scope)
            .map(|confirmed| confirmed.groups.into_iter().find(|group| &group.key == key))
            .map_err(|error| self.store.database_error(operation, error))
    }

    pub fn set_group_label(&self, key: &DeviceKey, label: &str) -> Result<(), StorageError> {
        let operation = "write group label";
        validate_key(key)
            .and_then(|()| validate_label(label))
            .map_err(|error| self.invalid(operation, error))?;
        let changed = self
            .store
            .connection()
            .execute(
                "UPDATE group_identity SET label=?3 WHERE source=?1 AND device_id=?2",
                params![key.source, key.id, label],
            )
            .map_err(|error| self.store.database_error(operation, error))?;
        if changed == 0 {
            return Err(self.invalid(operation, "Group identity does not exist"));
        }
        Ok(())
    }

    /// Load one stored group parent identity by stable device key. The lookup
    /// only resolves the reserved endpoint allocation and the saved bridge
    /// identity and name; it never decides whether a relation may be
    /// published.
    pub fn load_group_identity(
        &self,
        key: &DeviceKey,
    ) -> Result<Option<StoredGroupIdentity>, StorageError> {
        let operation = "load Xiaomi group identity";
        validate_key(key).map_err(|error| self.invalid(operation, error))?;
        let connection = self.store.connection();
        let row = connection
            .query_row(
                "SELECT unique_id, endpoint, label FROM group_identity
                 WHERE source=?1 AND device_id=?2",
                params![key.source, key.id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, u16>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(|error| self.store.database_error(operation, error))?;
        let Some((unique_id, endpoint, label)) = row else {
            return Ok(None);
        };
        let label =
            label.ok_or_else(|| self.invalid(operation, "Group identity has no saved label"))?;
        if !(2..=65534).contains(&endpoint)
            || !Uuid::parse_str(&unique_id).is_ok_and(|uuid| uuid.simple().to_string() == unique_id)
            || validate_label(&label).is_err()
        {
            return Err(self.invalid(operation, "Stored group identity is invalid"));
        }
        let allocation = connection
            .query_row(
                "SELECT endpoint FROM endpoint_allocation
                 WHERE source=?1 AND device_id=?2 AND slot='group'",
                params![key.source, key.id],
                |row| row.get::<_, u16>(0),
            )
            .optional()
            .map_err(|error| self.store.database_error(operation, error))?;
        let count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM endpoint_allocation WHERE source=?1 AND device_id=?2",
                params![key.source, key.id],
                |row| row.get(0),
            )
            .map_err(|error| self.store.database_error(operation, error))?;
        if allocation != Some(endpoint) || count != 1 {
            return Err(self.invalid(operation, "Group endpoint allocation is inconsistent"));
        }
        Ok(Some(StoredGroupIdentity {
            key: key.clone(),
            unique_id,
            endpoint,
            label,
        }))
    }

    /// Read only the saved label of a group identity reservation.
    ///
    /// `None` covers both an unknown identity and a legacy reservation that
    /// has no saved label yet; a malformed present label and database failures
    /// are errors. Endpoint allocation consistency stays owned by
    /// [`Self::load_group_identity`], which the published parent path uses.
    pub fn load_group_label(&self, key: &DeviceKey) -> Result<Option<String>, StorageError> {
        let operation = "load Xiaomi group label";
        validate_key(key).map_err(|error| self.invalid(operation, error))?;
        let label = self
            .store
            .connection()
            .query_row(
                "SELECT label FROM group_identity WHERE source=?1 AND device_id=?2",
                params![key.source, key.id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map_err(|error| self.store.database_error(operation, error))?;
        match label {
            None | Some(None) => Ok(None),
            Some(Some(label)) => {
                validate_label(&label).map_err(|error| self.invalid(operation, error))?;
                Ok(Some(label))
            }
        }
    }
}

fn commit_in(
    store: &DeviceStore,
    connection: &Connection,
    scope: &CachedScope,
    compositions: &[Composition],
    removed: &[DeviceKey],
    operation: &'static str,
) -> Result<ConfirmedComposition, CompositionCommitError> {
    let database = |error: rusqlite::Error| {
        CompositionCommitError::Storage(store.store.database_error(operation, error))
    };
    for group in compositions {
        for member in &group.members {
            if removed.contains(member) {
                return Err(CompositionCommitError::RemovedMember(member.clone()));
            }
            if !compatible_light_member(connection, &scope.uid, &scope.home_id, member)
                .map_err(database)?
            {
                return Err(CompositionCommitError::UnsupportedMember(member.clone()));
            }
        }
    }
    let proposed_keys = compositions
        .iter()
        .map(|group| group.key.clone())
        .collect::<HashSet<_>>();
    for key in removed {
        if proposed_keys.contains(key) {
            return Err(CompositionCommitError::RemovedGroup(key.clone()));
        }
    }
    let current = load_relation_set(connection, scope).map_err(database)?;
    let proposed = normalized(compositions);
    let effective_removal = removed
        .iter()
        .map(|key| device_published(connection, key).map_err(database))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .any(|published| published);
    if current == proposed && !effective_removal {
        // A no-op sync must not rewrite relations or consume a version.
        return load_confirmed(connection, scope).map_err(database);
    }
    let current_version = load_version(connection, scope).map_err(database)?;
    let version = if current != proposed {
        current_version.wrapping_add(1)
    } else {
        current_version
    };
    connection
        .execute(
            "DELETE FROM group_relation WHERE uid=?1 AND home_id=?2",
            params![scope.uid, scope.home_id],
        )
        .map_err(database)?;
    for group in compositions {
        ensure_group_identity(store, connection, group, operation)?;
        for member in &group.members {
            connection
                .execute(
                    "INSERT INTO group_relation VALUES(?1,?2,?3,?4,?5,?6)",
                    params![
                        scope.uid,
                        scope.home_id,
                        group.key.source,
                        group.key.id,
                        member.source,
                        member.id
                    ],
                )
                .map_err(database)?;
        }
    }
    for key in removed {
        connection
            .execute(
                "UPDATE device_identity SET published=0 WHERE source=?1 AND device_id=?2",
                params![key.source, key.id],
            )
            .map_err(database)?;
    }
    let version = i64::try_from(version).map_err(|_| database(rusqlite::Error::InvalidQuery))?;
    connection
        .execute(
            "INSERT INTO group_snapshot VALUES(?1,?2,?3)
             ON CONFLICT(uid,home_id) DO UPDATE SET version=excluded.version",
            params![scope.uid, scope.home_id, version],
        )
        .map_err(database)?;
    load_confirmed(connection, scope).map_err(database)
}

fn ensure_group_identity(
    store: &DeviceStore,
    connection: &Connection,
    group: &Composition,
    operation: &'static str,
) -> Result<(), CompositionCommitError> {
    let database = |error: rusqlite::Error| {
        CompositionCommitError::Storage(store.store.database_error(operation, error))
    };
    let existing = connection
        .query_row(
            "SELECT label FROM group_identity WHERE source=?1 AND device_id=?2",
            params![group.key.source, group.key.id],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()
        .map_err(database)?;
    if let Some(label) = existing {
        let label = label.unwrap_or_else(|| import_label(&group.label));
        connection
            .execute(
                "UPDATE group_identity SET label=?3 WHERE source=?1 AND device_id=?2",
                params![group.key.source, group.key.id, label],
            )
            .map_err(database)?;
        return Ok(());
    }
    let device = connection
        .query_row(
            "SELECT unique_id, endpoint, label FROM device_identity WHERE source=?1 AND device_id=?2",
            params![group.key.source, group.key.id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, u16>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .optional()
        .map_err(database)?;
    if let Some((unique_id, endpoint, label)) = device {
        // Only known Xiaomi group identities may change from a light to a
        // parent; ordinary admitted devices keep their strict compatibility.
        if !legacy_group_identity(connection, &group.key).map_err(database)? {
            return Err(CompositionCommitError::Invalid(
                CompositionError::GroupKeyConflict(group.key.clone()),
            ));
        }
        if !(2..=65534).contains(&endpoint)
            || !Uuid::parse_str(&unique_id).is_ok_and(|uuid| uuid.simple().to_string() == unique_id)
            || label
                .as_deref()
                .is_some_and(|label| validate_label(label).is_err())
        {
            return Err(CompositionCommitError::Storage(
                store
                    .store
                    .database_error(operation, rusqlite::Error::InvalidQuery),
            ));
        }
        let allocations = connection
            .prepare(
                "SELECT endpoint FROM endpoint_allocation WHERE source=?1 AND device_id=?2 ORDER BY endpoint",
            )
            .and_then(|mut statement| {
                statement
                    .query_map(params![group.key.source, group.key.id], |row| {
                        row.get::<_, u16>(0)
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .map_err(database)?;
        if allocations.len() != 1 || allocations[0] != endpoint {
            return Err(CompositionCommitError::Storage(
                store
                    .store
                    .database_error(operation, rusqlite::Error::InvalidQuery),
            ));
        }
        connection
            .execute(
                "UPDATE endpoint_allocation SET slot='group' WHERE source=?1 AND device_id=?2 AND endpoint=?3",
                params![group.key.source, group.key.id, endpoint],
            )
            .map_err(database)?;
        connection
            .execute(
                "DELETE FROM device_function WHERE source=?1 AND device_id=?2",
                params![group.key.source, group.key.id],
            )
            .map_err(database)?;
        connection
            .execute(
                "DELETE FROM device_identity WHERE source=?1 AND device_id=?2",
                params![group.key.source, group.key.id],
            )
            .map_err(database)?;
        let label = label.unwrap_or_else(|| import_label(&group.label));
        connection
            .execute(
                "INSERT INTO group_identity VALUES(?1,?2,?3,?4,?5)",
                params![group.key.source, group.key.id, unique_id, endpoint, label],
            )
            .map_err(database)?;
        return Ok(());
    }
    let last = connection
        .query_row(
            "SELECT COALESCE(MAX(endpoint),1) FROM endpoint_allocation",
            [],
            |row| row.get::<_, u16>(0),
        )
        .map_err(database)?;
    let endpoint = last + 1;
    if endpoint > 65534 {
        return Err(CompositionCommitError::EndpointExhausted);
    }
    let unique_id = Uuid::new_v4().simple().to_string();
    let label = import_label(&group.label);
    reserve_endpoint(connection, &group.key, "group", endpoint).map_err(database)?;
    connection
        .execute(
            "INSERT INTO group_identity VALUES(?1,?2,?3,?4,?5)",
            params![group.key.source, group.key.id, unique_id, endpoint, label],
        )
        .map_err(database)?;
    Ok(())
}

fn legacy_group_identity(connection: &Connection, key: &DeviceKey) -> rusqlite::Result<bool> {
    if key.source != "xiaomi" {
        return Ok(false);
    }
    if key.id.starts_with("group.") {
        return Ok(true);
    }
    connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM device_cache
         WHERE source=?1 AND device_id=?2 AND model='mijia.light.group3')",
        params![key.source, key.id],
        |row| row.get(0),
    )
}

fn load_confirmed(
    connection: &Connection,
    scope: &CachedScope,
) -> rusqlite::Result<ConfirmedComposition> {
    let version = load_version(connection, scope)?;
    // A relation without a parent identity is corruption, never a silent omission.
    let orphans: i64 = connection.query_row(
        "SELECT COUNT(*) FROM group_relation r
         LEFT JOIN group_identity g ON g.source=r.source AND g.device_id=r.device_id
         WHERE r.uid=?1 AND r.home_id=?2 AND g.source IS NULL",
        params![scope.uid, scope.home_id],
        |row| row.get(0),
    )?;
    if orphans != 0 {
        return Err(rusqlite::Error::InvalidQuery);
    }
    let groups = connection
        .prepare(
            "SELECT g.source, g.device_id, g.unique_id, g.endpoint, g.label
             FROM group_identity g
             WHERE EXISTS (
                 SELECT 1 FROM group_relation r
                 WHERE r.uid=?1 AND r.home_id=?2
                   AND r.source=g.source AND r.device_id=g.device_id)
             ORDER BY g.endpoint",
        )
        .and_then(|mut statement| {
            statement
                .query_map(params![scope.uid, scope.home_id], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, u16>(3)?,
                        row.get::<_, Option<String>>(4)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()
        })?;
    let mut stored = Vec::with_capacity(groups.len());
    for (source, device_id, unique_id, endpoint, label) in groups {
        let key = DeviceKey::new(source, device_id);
        let label = label.ok_or(rusqlite::Error::InvalidQuery)?;
        if validate_key(&key).is_err()
            || !(2..=65534).contains(&endpoint)
            || !Uuid::parse_str(&unique_id).is_ok_and(|uuid| uuid.simple().to_string() == unique_id)
            || validate_label(&label).is_err()
        {
            return Err(rusqlite::Error::InvalidQuery);
        }
        let allocation = connection
            .query_row(
                "SELECT endpoint FROM endpoint_allocation WHERE source=?1 AND device_id=?2 AND slot='group'",
                params![key.source, key.id],
                |row| row.get::<_, u16>(0),
            )
            .optional()?;
        let count: i64 = connection.query_row(
            "SELECT COUNT(*) FROM endpoint_allocation WHERE source=?1 AND device_id=?2",
            params![key.source, key.id],
            |row| row.get(0),
        )?;
        if allocation != Some(endpoint) || count != 1 {
            return Err(rusqlite::Error::InvalidQuery);
        }
        let members = load_members(connection, scope, &key)?;
        if members.iter().any(|member| validate_key(member).is_err()) {
            return Err(rusqlite::Error::InvalidQuery);
        }
        stored.push(StoredGroup {
            key,
            unique_id,
            endpoint,
            label,
            members,
        });
    }
    let structural = stored
        .iter()
        .map(|group| Composition {
            key: group.key.clone(),
            label: group.label.clone(),
            members: group.members.clone(),
        })
        .collect::<Vec<_>>();
    validate_composition_structure(&structural).map_err(|_| rusqlite::Error::InvalidQuery)?;
    Ok(ConfirmedComposition {
        version,
        groups: stored,
    })
}

fn load_members(
    connection: &Connection,
    scope: &CachedScope,
    key: &DeviceKey,
) -> rusqlite::Result<Vec<DeviceKey>> {
    connection
        .prepare(
            "SELECT member_source, member_id FROM group_relation
             WHERE uid=?1 AND home_id=?2 AND source=?3 AND device_id=?4
             ORDER BY member_source, member_id",
        )
        .and_then(|mut statement| {
            statement
                .query_map(
                    params![scope.uid, scope.home_id, key.source, key.id],
                    |row| {
                        Ok(DeviceKey::new(
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                        ))
                    },
                )?
                .collect()
        })
}

fn device_published(connection: &Connection, key: &DeviceKey) -> rusqlite::Result<bool> {
    Ok(connection
        .query_row(
            "SELECT published FROM device_identity WHERE source=?1 AND device_id=?2",
            params![key.source, key.id],
            |row| row.get::<_, bool>(0),
        )
        .optional()?
        .unwrap_or(false))
}

fn load_version(connection: &Connection, scope: &CachedScope) -> rusqlite::Result<u64> {
    let version = connection
        .query_row(
            "SELECT version FROM group_snapshot WHERE uid=?1 AND home_id=?2",
            params![scope.uid, scope.home_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .unwrap_or(0);
    u64::try_from(version).map_err(|_| rusqlite::Error::InvalidQuery)
}

fn load_relation_set(
    connection: &Connection,
    scope: &CachedScope,
) -> rusqlite::Result<Vec<(DeviceKey, Vec<DeviceKey>)>> {
    let rows = connection
        .prepare(
            "SELECT source, device_id, member_source, member_id FROM group_relation
             WHERE uid=?1 AND home_id=?2
             ORDER BY source, device_id, member_source, member_id",
        )
        .and_then(|mut statement| {
            statement
                .query_map(params![scope.uid, scope.home_id], |row| {
                    Ok((
                        DeviceKey::new(row.get::<_, String>(0)?, row.get::<_, String>(1)?),
                        DeviceKey::new(row.get::<_, String>(2)?, row.get::<_, String>(3)?),
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()
        })?;
    let mut groups: Vec<(DeviceKey, Vec<DeviceKey>)> = Vec::new();
    for (group, member) in rows {
        match groups.last_mut() {
            Some((key, members)) if key == &group => members.push(member),
            _ => groups.push((group, vec![member])),
        }
    }
    Ok(groups)
}

fn normalized(compositions: &[Composition]) -> Vec<(DeviceKey, Vec<DeviceKey>)> {
    let mut groups = compositions
        .iter()
        .map(|group| {
            let mut members = group.members.clone();
            members.sort_by(key_order);
            (group.key.clone(), members)
        })
        .collect::<Vec<_>>();
    groups.sort_by(|left, right| key_order(&left.0, &right.0));
    groups
}

fn key_order(left: &DeviceKey, right: &DeviceKey) -> std::cmp::Ordering {
    (left.source.as_str(), left.id.as_str()).cmp(&(right.source.as_str(), right.id.as_str()))
}

/// Confirmed member deletion: revoke every group that references the key and
/// unpublish it in one transaction. Republishing the member alone does not
/// resurrect a revoked group.
pub(super) fn remove_device(store: &DeviceStore, key: &DeviceKey) -> Result<(), StorageError> {
    let operation = "remove published device";
    validate_key(key).map_err(|error| store.invalid(operation, error))?;
    let transaction = store
        .store
        .connection()
        .unchecked_transaction()
        .map_err(|error| store.store.database_error(operation, error))?;
    let database = |error: rusqlite::Error| store.store.database_error(operation, error);
    let mut scopes = HashSet::new();
    let affected = transaction
        .prepare(
            "SELECT DISTINCT uid, home_id, source, device_id FROM group_relation
             WHERE member_source=?1 AND member_id=?2",
        )
        .and_then(|mut statement| {
            statement
                .query_map(params![key.source, key.id], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .map_err(database)?;
    for (uid, home_id, source, device_id) in affected {
        transaction
            .execute(
                "DELETE FROM group_relation WHERE uid=?1 AND home_id=?2 AND source=?3 AND device_id=?4",
                params![uid, home_id, source, device_id],
            )
            .map_err(database)?;
        scopes.insert((uid, home_id));
    }
    let parent_scopes = transaction
        .prepare(
            "SELECT DISTINCT uid, home_id FROM group_relation WHERE source=?1 AND device_id=?2",
        )
        .and_then(|mut statement| {
            statement
                .query_map(params![key.source, key.id], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .map_err(database)?;
    for (uid, home_id) in parent_scopes {
        transaction
            .execute(
                "DELETE FROM group_relation WHERE uid=?1 AND home_id=?2 AND source=?3 AND device_id=?4",
                params![uid, home_id, key.source, key.id],
            )
            .map_err(database)?;
        scopes.insert((uid, home_id));
    }
    for (uid, home_id) in scopes {
        transaction
            .execute(
                "INSERT INTO group_snapshot VALUES(?1,?2,1)
                 ON CONFLICT(uid,home_id) DO UPDATE SET version=version+1",
                params![uid, home_id],
            )
            .map_err(database)?;
    }
    transaction
        .execute(
            "UPDATE device_identity SET published=0 WHERE source=?1 AND device_id=?2",
            params![key.source, key.id],
        )
        .map_err(database)?;
    transaction.commit().map_err(database)
}
