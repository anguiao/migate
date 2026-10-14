use super::{
    StorageError,
    device::{CachedScope, DeviceStore, validate_key, validate_scope, validate_text},
    registry::{compatible_light_member, import_label, validate_label},
};
use crate::device::{
    Capability, Category, Composition, CompositionError, DeviceKey, DeviceRegistry,
    FunctionDescription, GroupConfiguration, PendingConfiguration, PhysicalDevice,
    PublishedEndpoint, validate_composition_structure,
};
use rusqlite::{Connection, OptionalExtension as _, params};
use std::collections::HashSet;
use uuid::Uuid;

/// A group parent identity, derived shape and current-scope members.
/// Preparation reserves these values; a successful transaction persists them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredGroup {
    pub key: DeviceKey,
    pub unique_id: String,
    pub endpoint: u16,
    pub label: String,
    pub members: Vec<DeviceKey>,
    /// The derived member-capability intersection committed with the relation.
    pub capabilities: Vec<Capability>,
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

/// The last committed applied configuration of one account/home: the relation
/// version, its published groups and the standalone device endpoints visible
/// in that version. A later admission reservation never appears here until its
/// own configuration commit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfirmedComposition {
    pub version: u64,
    pub groups: Vec<StoredGroup>,
    /// The applied standalone endpoints, ordered by stable device key.
    pub devices: Vec<PublishedEndpoint>,
}

/// One requested Matter endpoint ConfigurationVersion.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConfigurationVersion {
    pub endpoint: u16,
    pub version: u32,
}

/// One opaque protocol blob change: `None` deletes the key.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlobChange {
    pub key: u16,
    pub value: Option<Vec<u8>>,
}

/// Borrowed by the protocol mapper inside the synchronous apply boundary.
/// Construction and commitment stay private to that boundary.
#[derive(Debug)]
pub struct PreparedConfiguration<'a> {
    groups: Vec<StoredGroup>,
    devices: Vec<PublishedEndpoint>,
    pending: &'a PendingConfiguration,
}

impl PreparedConfiguration<'_> {
    /// The planned group endpoints and derived shapes Matter can encode.
    pub fn groups(&self) -> &[StoredGroup] {
        &self.groups
    }

    /// The exact source confirmation and member instances pinned at staging.
    pub fn pending(&self) -> &PendingConfiguration {
        self.pending
    }
}

/// A rejected composition or configuration commit. Rejection leaves stored
/// data and applied runtime state unchanged.
#[derive(Clone, Debug)]
pub enum CompositionCommitError {
    Invalid(CompositionError),
    UnsupportedMember(DeviceKey),
    RemovedMember(DeviceKey),
    RemovedGroup(DeviceKey),
    EndpointExhausted,
    VersionRegression {
        endpoint: u16,
        current: u32,
        requested: u32,
    },
    DuplicateVersion(u16),
    DuplicateBlob(u16),
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
            Self::VersionRegression {
                endpoint,
                current,
                requested,
            } => write!(
                formatter,
                "Configuration version for endpoint {endpoint} cannot regress from {current} to {requested}"
            ),
            Self::DuplicateVersion(endpoint) => write!(
                formatter,
                "Configuration version for endpoint {endpoint} is repeated"
            ),
            Self::DuplicateBlob(key) => {
                write!(formatter, "Matter blob key {key} is repeated")
            }
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
    /// Resolve the latest directory confirmation, map its protocol metadata,
    /// and commit storage and the applied view under one registry lock. The
    /// mapper may read storage and device handles, but must not re-enter the
    /// registry. Any failure keeps both the candidate and old applied view.
    pub fn apply_pending_configuration(
        &self,
        registry: &DeviceRegistry,
        map: impl FnOnce(
            &PreparedConfiguration<'_>,
        )
            -> Result<(Vec<ConfigurationVersion>, Vec<BlobChange>), CompositionCommitError>,
    ) -> Result<Option<ConfirmedComposition>, CompositionCommitError> {
        registry.apply_pending(|pending| {
            let operation = "apply configuration";
            let scope = CachedScope {
                uid: pending.scope.uid.clone(),
                home_id: pending.scope.home_id.clone(),
                group_id: pending.scope.group_id.clone(),
            };
            validate_scope(&scope.uid, &scope.home_id, &scope.group_id)
                .map_err(|error| self.invalid(operation, error))?;
            for key in &pending.removed {
                validate_key(key).map_err(|error| self.invalid(operation, error))?;
            }
            let transaction = self
                .store
                .connection()
                .unchecked_transaction()
                .map_err(|error| self.store.database_error(operation, error))?;
            validate_members(
                self,
                &transaction,
                &scope,
                pending
                    .groups
                    .iter()
                    .map(|group| (&group.key, group.members.as_slice())),
                &pending.removed,
            )?;
            let groups = plan_groups(self, &transaction, &pending.groups)?;
            let devices = plan_devices(self, &transaction, &pending.devices)?;
            for (group, planned) in pending.groups.iter_mut().zip(&groups) {
                group.label = planned.label.clone();
            }
            let prepared = PreparedConfiguration {
                groups,
                devices,
                pending,
            };
            let (versions, blobs) = map(&prepared)?;
            let confirmed = commit_in(
                self,
                &transaction,
                &scope,
                &prepared.groups,
                &prepared.devices,
                &pending.removed,
                operation,
            )?;
            write_protocol_metadata(self, &transaction, &versions, &blobs, operation)?;
            transaction
                .commit()
                .map_err(|error| self.store.database_error(operation, error))?;
            let version = confirmed.version;
            Ok((confirmed, version))
        })
    }

    /// Commit only Matter protocol metadata: endpoint ConfigurationVersions
    /// and opaque blobs, in one transaction, without rewriting any relation
    /// set. The startup baseline uses this to persist the canonical shape and
    /// the root BasicInfo bump together.
    pub fn commit_protocol_configuration(
        &self,
        versions: Vec<ConfigurationVersion>,
        blobs: Vec<BlobChange>,
    ) -> Result<(), CompositionCommitError> {
        if versions.is_empty() && blobs.is_empty() {
            return Ok(());
        }
        let operation = "commit Matter configuration metadata";
        let transaction = self
            .store
            .connection()
            .unchecked_transaction()
            .map_err(|error| self.store.database_error(operation, error))?;
        write_protocol_metadata(self, &transaction, &versions, &blobs, operation)?;
        transaction
            .commit()
            .map_err(|error| self.store.database_error(operation, error))?;
        Ok(())
    }

    /// Read one persisted Matter endpoint ConfigurationVersion.
    pub fn configuration_version(&self, endpoint: u16) -> Result<Option<u32>, StorageError> {
        let operation = "load Matter configuration version";
        load_configuration_version(self.store.connection(), endpoint)
            .map_err(|error| self.store.database_error(operation, error))
    }

    /// The last committed applied configuration for the account/home: the
    /// relation version, its groups and the standalone endpoints visible in
    /// that version. Only groups whose stored members are compatible published
    /// single lights are returned; member runtime availability is unrelated.
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
        let row = existing_group_identity(connection, key)
            .map_err(|error| self.store.database_error(operation, error))?;
        let Some((unique_id, endpoint, label)) = row else {
            return Ok(None);
        };
        if let Some(problem) = group_identity_problem(connection, key, &unique_id, endpoint, &label)
            .map_err(|error| self.store.database_error(operation, error))?
        {
            return Err(self.invalid(operation, problem));
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
    /// `None` covers an unknown identity; malformed stored labels and database
    /// failures are errors. Endpoint allocation consistency stays owned by
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
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| self.store.database_error(operation, error))?;
        if let Some(label) = &label {
            validate_label(label).map_err(|error| self.invalid(operation, error))?;
        }
        Ok(label)
    }
}

fn plan_groups(
    store: &DeviceStore,
    connection: &Connection,
    groups: &[GroupConfiguration],
) -> Result<Vec<StoredGroup>, CompositionCommitError> {
    let operation = "prepare configuration commit";
    let database = |error: rusqlite::Error| {
        CompositionCommitError::Storage(store.store.database_error(operation, error))
    };
    let mut last = connection
        .query_row(
            "SELECT COALESCE(MAX(endpoint),1) FROM endpoint_allocation",
            [],
            |row| row.get::<_, u16>(0),
        )
        .map_err(database)?;
    let mut planned = Vec::with_capacity(groups.len());
    for group in groups {
        let (unique_id, endpoint, label) = match existing_group_identity(connection, &group.key)
            .map_err(database)?
        {
            Some((unique_id, endpoint, label)) => {
                if let Some(problem) =
                    group_identity_problem(connection, &group.key, &unique_id, endpoint, &label)
                        .map_err(database)?
                {
                    return Err(store.invalid(operation, problem).into());
                }
                (unique_id, endpoint, label)
            }
            None => {
                let (device_exists, reserved): (bool, bool) = connection
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM device_identity WHERE source=?1 AND device_id=?2),
                                EXISTS(SELECT 1 FROM endpoint_allocation WHERE source=?1 AND device_id=?2)",
                        params![group.key.source, group.key.id],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .map_err(database)?;
                if device_exists {
                    return Err(CompositionCommitError::Invalid(
                        CompositionError::GroupKeyConflict(group.key.clone()),
                    ));
                }
                if reserved {
                    return Err(store
                        .invalid(operation, "Group endpoint reservation has no identity")
                        .into());
                }
                last = last
                    .checked_add(1)
                    .ok_or(CompositionCommitError::EndpointExhausted)?;
                if last > 65534 {
                    return Err(CompositionCommitError::EndpointExhausted);
                }
                (
                    Uuid::new_v4().simple().to_string(),
                    last,
                    import_label(&group.label),
                )
            }
        };
        planned.push(StoredGroup {
            key: group.key.clone(),
            unique_id,
            endpoint,
            label,
            members: group.members.clone(),
            capabilities: group.capabilities.clone(),
        });
    }
    Ok(planned)
}

fn plan_devices(
    store: &DeviceStore,
    connection: &Connection,
    devices: &[PhysicalDevice],
) -> Result<Vec<PublishedEndpoint>, CompositionCommitError> {
    let operation = "prepare configuration commit";
    let mut planned = Vec::with_capacity(devices.len());
    for device in devices {
        let key = device.key();
        let endpoint = connection
            .query_row(
                "SELECT endpoint FROM device_identity
                 WHERE source=?1 AND device_id=?2 AND published=1",
                params![key.source, key.id],
                |row| row.get::<_, u16>(0),
            )
            .optional()
            .map_err(|error| store.store.database_error(operation, error))?;
        let Some(endpoint) = endpoint else {
            return Err(CompositionCommitError::UnsupportedMember(key.clone()));
        };
        planned.push(PublishedEndpoint {
            key: key.clone(),
            endpoint,
        });
    }
    planned.sort_by(|left, right| key_order(&left.key, &right.key));
    Ok(planned)
}

fn existing_group_identity(
    connection: &Connection,
    key: &DeviceKey,
) -> rusqlite::Result<Option<(String, u16, String)>> {
    connection
        .query_row(
            "SELECT unique_id, endpoint, label FROM group_identity
             WHERE source=?1 AND device_id=?2",
            params![key.source, key.id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, u16>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()
}

fn validate_blobs(blobs: &[BlobChange]) -> Result<(), CompositionCommitError> {
    let mut keys = HashSet::new();
    for blob in blobs {
        if !keys.insert(blob.key) {
            return Err(CompositionCommitError::DuplicateBlob(blob.key));
        }
    }
    Ok(())
}

/// Write endpoint ConfigurationVersions and opaque blobs inside an existing
/// transaction. The caller owns the transaction boundary; every failure rolls
/// the whole metadata set back together.
fn write_protocol_metadata(
    store: &DeviceStore,
    transaction: &rusqlite::Transaction<'_>,
    versions: &[ConfigurationVersion],
    blobs: &[BlobChange],
    operation: &'static str,
) -> Result<(), CompositionCommitError> {
    let database = |error: rusqlite::Error| {
        CompositionCommitError::Storage(store.store.database_error(operation, error))
    };
    validate_blobs(blobs)?;
    let mut endpoints = HashSet::new();
    for version in versions {
        if !endpoints.insert(version.endpoint) {
            return Err(CompositionCommitError::DuplicateVersion(version.endpoint));
        }
        if let Some(current) =
            load_configuration_version(transaction, version.endpoint).map_err(database)?
            && version.version < current
        {
            return Err(CompositionCommitError::VersionRegression {
                endpoint: version.endpoint,
                current,
                requested: version.version,
            });
        }
    }
    for version in versions {
        transaction
            .execute(
                "INSERT INTO configuration_version VALUES(?1,?2)
                 ON CONFLICT(endpoint) DO UPDATE SET version=excluded.version",
                params![version.endpoint, i64::from(version.version)],
            )
            .map_err(database)?;
    }
    for blob in blobs {
        match &blob.value {
            Some(value) => {
                transaction
                    .execute(
                        "INSERT INTO blobs VALUES(?1,?2)
                         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                        params![blob.key, value],
                    )
                    .map_err(database)?;
            }
            None => {
                transaction
                    .execute("DELETE FROM blobs WHERE key=?1", [blob.key])
                    .map_err(database)?;
            }
        }
    }
    Ok(())
}

fn validate_members<'a>(
    store: &DeviceStore,
    connection: &Connection,
    scope: &CachedScope,
    groups: impl Iterator<Item = (&'a DeviceKey, &'a [DeviceKey])>,
    removed: &[DeviceKey],
) -> Result<(), CompositionCommitError> {
    let operation = "commit Xiaomi group composition";
    for (key, members) in groups {
        if removed.contains(key) {
            return Err(CompositionCommitError::RemovedGroup(key.clone()));
        }
        for member in members {
            if removed.contains(member) {
                return Err(CompositionCommitError::RemovedMember(member.clone()));
            }
            if !compatible_light_member(connection, &scope.uid, &scope.home_id, member)
                .map_err(|error| store.store.database_error(operation, error))?
            {
                return Err(CompositionCommitError::UnsupportedMember(member.clone()));
            }
        }
    }
    Ok(())
}

fn commit_in(
    store: &DeviceStore,
    connection: &Connection,
    scope: &CachedScope,
    groups: &[StoredGroup],
    devices: &[PublishedEndpoint],
    removed: &[DeviceKey],
    operation: &'static str,
) -> Result<ConfirmedComposition, CompositionCommitError> {
    let database = |error: rusqlite::Error| {
        CompositionCommitError::Storage(store.store.database_error(operation, error))
    };
    let current = load_relation_set(connection, scope).map_err(database)?;
    let proposed = normalized(groups);
    let current_devices = load_publication_devices(connection, scope).map_err(database)?;
    let effective_removal = removed
        .iter()
        .map(|key| device_published(connection, key).map_err(database))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .any(|published| published);
    let groups_changed = current != proposed;
    let devices_changed = current_devices != devices;
    if !groups_changed && !devices_changed && !effective_removal {
        // A no-op sync must not rewrite relations or consume a version.
        return load_confirmed(connection, scope).map_err(database);
    }
    let current_version = load_version(connection, scope).map_err(database)?;
    let version = if groups_changed || devices_changed {
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
    for group in groups {
        write_group_identity(store, connection, group, operation)?;
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
    connection
        .execute(
            "DELETE FROM group_publication WHERE uid=?1 AND home_id=?2",
            params![scope.uid, scope.home_id],
        )
        .map_err(database)?;
    for group in groups {
        let capabilities = serde_json::to_string(&group.capabilities)
            .map_err(|_| database(rusqlite::Error::InvalidQuery))?;
        connection
            .execute(
                "INSERT INTO group_publication VALUES(?1,?2,?3,?4,?5)",
                params![
                    scope.uid,
                    scope.home_id,
                    group.key.source,
                    group.key.id,
                    capabilities
                ],
            )
            .map_err(database)?;
    }
    write_publication_devices(connection, scope, devices).map_err(database)?;
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

fn write_group_identity(
    store: &DeviceStore,
    connection: &Connection,
    group: &StoredGroup,
    operation: &'static str,
) -> Result<(), CompositionCommitError> {
    let database = |error: rusqlite::Error| {
        CompositionCommitError::Storage(store.store.database_error(operation, error))
    };
    // Planning validated existing identities under this same transaction.
    // Preserve their names and reservations; only new groups add rows.
    connection
        .execute(
            "INSERT INTO endpoint_allocation VALUES(?1,?2,?3,'group')
             ON CONFLICT(source,device_id,slot) DO NOTHING",
            params![group.endpoint, group.key.source, group.key.id],
        )
        .map_err(database)?;
    connection
        .execute(
            "INSERT INTO group_identity VALUES(?1,?2,?3,?4,?5)
             ON CONFLICT(source,device_id) DO NOTHING",
            params![
                group.key.source,
                group.key.id,
                group.unique_id,
                group.endpoint,
                group.label
            ],
        )
        .map_err(database)?;
    Ok(())
}

fn write_publication_devices(
    connection: &Connection,
    scope: &CachedScope,
    devices: &[PublishedEndpoint],
) -> rusqlite::Result<()> {
    connection.execute(
        "DELETE FROM publication_device WHERE uid=?1 AND home_id=?2",
        params![scope.uid, scope.home_id],
    )?;
    for device in devices {
        connection.execute(
            "INSERT INTO publication_device VALUES(?1,?2,?3,?4,?5)",
            params![
                scope.uid,
                scope.home_id,
                device.key.source,
                device.key.id,
                device.endpoint
            ],
        )?;
    }
    Ok(())
}

fn group_identity_problem(
    connection: &Connection,
    key: &DeviceKey,
    unique_id: &str,
    endpoint: u16,
    label: &str,
) -> rusqlite::Result<Option<&'static str>> {
    if validate_key(key).is_err()
        || !(2..=65534).contains(&endpoint)
        || !Uuid::parse_str(unique_id).is_ok_and(|uuid| uuid.simple().to_string() == unique_id)
        || validate_label(label).is_err()
    {
        return Ok(Some("Stored group identity is invalid"));
    }
    let (allocation, slot, count): (Option<u16>, Option<String>, i64) = connection.query_row(
        "SELECT MIN(endpoint), MIN(slot), COUNT(*) FROM endpoint_allocation
         WHERE source=?1 AND device_id=?2",
        params![key.source, key.id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    Ok(
        (allocation != Some(endpoint) || slot.as_deref() != Some("group") || count != 1)
            .then_some("Group endpoint allocation is inconsistent"),
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
                        row.get::<_, String>(4)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()
        })?;
    let mut stored = Vec::with_capacity(groups.len());
    for (source, device_id, unique_id, endpoint, label) in groups {
        let key = DeviceKey::new(source, device_id);
        if group_identity_problem(connection, &key, &unique_id, endpoint, &label)?.is_some() {
            return Err(rusqlite::Error::InvalidQuery);
        }
        let members = members_of(connection, &scope.uid, &scope.home_id, &key)?;
        if members.iter().any(|member| validate_key(member).is_err()) {
            return Err(rusqlite::Error::InvalidQuery);
        }
        let capabilities = load_group_capabilities(connection, &scope.uid, &scope.home_id, &key)?;
        stored.push(StoredGroup {
            key,
            unique_id,
            endpoint,
            label,
            members,
            capabilities,
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
    let devices = load_publication_devices(connection, scope)?;
    Ok(ConfirmedComposition {
        version,
        groups: stored,
        devices,
    })
}

fn load_publication_devices(
    connection: &Connection,
    scope: &CachedScope,
) -> rusqlite::Result<Vec<PublishedEndpoint>> {
    connection
        .prepare(
            "SELECT source, device_id, endpoint FROM publication_device
             WHERE uid=?1 AND home_id=?2
             ORDER BY source, device_id",
        )
        .and_then(|mut statement| {
            statement
                .query_map(params![scope.uid, scope.home_id], |row| {
                    Ok(PublishedEndpoint {
                        key: DeviceKey::new(row.get::<_, String>(0)?, row.get::<_, String>(1)?),
                        endpoint: row.get(2)?,
                    })
                })?
                .collect()
        })
}

fn load_group_capabilities(
    connection: &Connection,
    uid: &str,
    home_id: &str,
    key: &DeviceKey,
) -> rusqlite::Result<Vec<Capability>> {
    let capabilities: Option<String> = connection
        .query_row(
            "SELECT capabilities FROM group_publication
             WHERE uid=?1 AND home_id=?2 AND source=?3 AND device_id=?4",
            params![uid, home_id, key.source, key.id],
            |row| row.get(0),
        )
        .optional()?;
    let capabilities: Vec<Capability> = serde_json::from_str(
        capabilities
            .as_deref()
            .ok_or(rusqlite::Error::InvalidQuery)?,
    )
    .map_err(|_| rusqlite::Error::InvalidQuery)?;
    if !valid_group_capabilities(&capabilities) {
        return Err(rusqlite::Error::InvalidQuery);
    }
    Ok(capabilities)
}

fn valid_group_capabilities(capabilities: &[Capability]) -> bool {
    if capabilities.is_empty() {
        return false;
    }
    FunctionDescription {
        id: "main".into(),
        label: String::new(),
        category: Category::Light,
        capabilities: capabilities.to_vec(),
    }
    .valid_capabilities()
}

fn members_of(
    connection: &Connection,
    uid: &str,
    home_id: &str,
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
                .query_map(params![uid, home_id, key.source, key.id], |row| {
                    Ok(DeviceKey::new(
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                    ))
                })?
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

fn load_configuration_version(
    connection: &Connection,
    endpoint: u16,
) -> rusqlite::Result<Option<u32>> {
    connection
        .query_row(
            "SELECT version FROM configuration_version WHERE endpoint=?1",
            [endpoint],
            |row| row.get::<_, u32>(0),
        )
        .optional()
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

fn normalized(compositions: &[StoredGroup]) -> Vec<(DeviceKey, Vec<DeviceKey>)> {
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
