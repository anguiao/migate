use super::identity;
use rusqlite::{Connection, OptionalExtension as _, Transaction};
use std::{error::Error as StdError, fmt};

const CURRENT_VERSION: i64 = 6;

pub(super) const SCHEMA: &str = concat!(
    include_str!("schema.sql"),
    include_str!("devices-v4.sql"),
    include_str!("groups-v6.sql")
);

#[derive(Debug)]
pub(super) enum MigrationError {
    Database(rusqlite::Error),
    InvalidSchema,
    UnsupportedVersion,
}

impl fmt::Display for MigrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(_) => formatter.write_str("SQLite operation failed"),
            Self::InvalidSchema => formatter.write_str("Database schema is invalid"),
            Self::UnsupportedVersion => formatter.write_str("Database version is not supported"),
        }
    }
}

impl StdError for MigrationError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for MigrationError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Database(error)
    }
}

pub(super) fn migrate(connection: &mut Connection) -> Result<(), MigrationError> {
    let version =
        connection.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))?;
    match version {
        CURRENT_VERSION => return Ok(()),
        0..CURRENT_VERSION => {}
        _ => return Err(MigrationError::UnsupportedVersion),
    }
    let transaction = connection.transaction()?;
    identity::load(&transaction)?;
    if version == 0 {
        migrate_v0_to_v1(&transaction)?;
    }
    if version < 2 {
        migrate_v1_to_v2(&transaction)?;
    }
    if version < 3 {
        migrate_v2_to_v3(&transaction)?;
    }
    if version < 4 {
        migrate_v3_to_v4(&transaction)?;
    }
    if version < 5 {
        migrate_v4_to_v5(&transaction, version == 4)?;
    }
    if version < 6 {
        migrate_v5_to_v6(&transaction)?;
    }
    transaction.commit()?;
    Ok(())
}

fn migrate_v2_to_v3(transaction: &Transaction<'_>) -> rusqlite::Result<()> {
    transaction.execute_batch(
        "CREATE TABLE identity_v3 (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            bridge_id TEXT NOT NULL CHECK (length(bridge_id) = 32)
        ) STRICT;
        INSERT INTO identity_v3 (id, bridge_id) SELECT id, bridge_id FROM identity;
        DROP TABLE identity;
        ALTER TABLE identity_v3 RENAME TO identity;
        PRAGMA user_version = 3;",
    )
}

fn migrate_v1_to_v2(transaction: &Transaction<'_>) -> rusqlite::Result<()> {
    transaction.execute_batch(
        "CREATE TABLE device_cache (
            uid TEXT NOT NULL, source TEXT NOT NULL, device_id TEXT NOT NULL,
            home_id TEXT NOT NULL, group_id TEXT NOT NULL, model TEXT NOT NULL, instance_type TEXT NOT NULL,
            category TEXT NOT NULL, capabilities TEXT NOT NULL, siid INTEGER NOT NULL, piid INTEGER NOT NULL,
            PRIMARY KEY (uid, source, device_id)
        ) STRICT;
        CREATE TABLE device_identity (
            source TEXT NOT NULL, device_id TEXT NOT NULL, unique_id TEXT NOT NULL UNIQUE, endpoint INTEGER NOT NULL,
            PRIMARY KEY (source, device_id)
        ) STRICT;
        PRAGMA user_version = 2;"
    )
}

fn migrate_v0_to_v1(transaction: &Transaction<'_>) -> rusqlite::Result<()> {
    transaction.execute_batch(
        "CREATE TABLE xiaomi_auth (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            uid TEXT NOT NULL,
            region TEXT NOT NULL,
            oauth_client_uuid TEXT NOT NULL,
            redirect_uri TEXT NOT NULL,
            access_token TEXT NOT NULL,
            refresh_token TEXT NOT NULL,
            expires_at INTEGER NOT NULL,
            refresh_at INTEGER NOT NULL,
            virtual_did TEXT NOT NULL,
            private_key_pem TEXT NOT NULL,
            certificate_pem TEXT NOT NULL
        ) STRICT;
        PRAGMA user_version = 1;",
    )
}

fn migrate_v3_to_v4(transaction: &Transaction<'_>) -> Result<(), MigrationError> {
    use crate::{
        device::{Capability, Category, DeviceKey, FunctionDescription},
        storage::device::{CachedPhysicalDevice, save_cached, validate_cached},
        xiaomi::capability::{ParsedDevice, ParsedFunction, PropertyBinding},
    };
    use rusqlite::params;
    let mut caches = Vec::new();
    let mut statement = transaction.prepare("SELECT uid,source,device_id,home_id,group_id,model,instance_type,category,capabilities,siid,piid FROM device_cache")?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, String>(6)?,
            row.get::<_, String>(7)?,
            row.get::<_, String>(8)?,
            row.get::<_, u32>(9)?,
            row.get::<_, u32>(10)?,
        ))
    })?;
    for row in rows {
        let (
            uid,
            source,
            id,
            home_id,
            group_id,
            model,
            instance_type,
            category,
            capabilities,
            siid,
            piid,
        ) = row?;
        if category != "outlet" || capabilities != "on_off" {
            return Err(MigrationError::InvalidSchema);
        }
        let cached = CachedPhysicalDevice {
            uid,
            home_id,
            group_id,
            model,
            instance_type,
            device: ParsedDevice {
                key: DeviceKey::new(source, id),
                functions: vec![ParsedFunction {
                    description: FunctionDescription {
                        id: "main".into(),
                        label: "主电源".into(),
                        category: Category::Outlet,
                        capabilities: vec![Capability::OnOff],
                    },
                    power: PropertyBinding { siid, piid },
                    lighting: None,
                }],
                refresh_trigger: None,
            },
        };
        validate_cached(&cached).map_err(|_| MigrationError::InvalidSchema)?;
        caches.push(cached);
    }
    drop(statement);
    let identities = transaction
        .prepare("SELECT source,device_id,unique_id,endpoint FROM device_identity")?
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, u16>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let active = identities.iter().any(|(source, id, _, _)| {
        caches
            .iter()
            .any(|cached| cached.device.key.source == *source && cached.device.key.id == *id)
    });
    let old_label = if active {
        let data = transaction
            .query_row(
                "SELECT value FROM blobs WHERE key=?1",
                [rs_matter::persist::VENDOR_KEYS_START],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()?;
        match data {
            Some(data) => {
                let label = rs_matter::tlv::TLVElement::new(&data)
                    .utf8()
                    .map_err(|_| MigrationError::InvalidSchema)?;
                super::registry::validate_label(label)
                    .map_err(|_| MigrationError::InvalidSchema)?;
                Some(label.to_owned())
            }
            None => None,
        }
    } else {
        None
    };
    transaction.execute_batch("ALTER TABLE device_cache RENAME TO device_cache_v3; ALTER TABLE device_identity RENAME TO device_identity_v3;")?;
    transaction.execute_batch(include_str!("devices-v4.sql"))?;
    for cached in &caches {
        save_cached(transaction, cached)?;
    }
    for (source, id, unique_id, endpoint) in identities {
        let key = DeviceKey::new(source, id);
        if super::device::validate_key(&key).is_err()
            || endpoint != 2
            || !uuid::Uuid::parse_str(&unique_id)
                .is_ok_and(|uuid| uuid.simple().to_string() == unique_id)
        {
            return Err(MigrationError::InvalidSchema);
        }
        let relevant = caches
            .iter()
            .filter(|cached| cached.device.key == key)
            .collect::<Vec<_>>();
        let label = if relevant.is_empty() {
            None
        } else if let Some(label) = &old_label {
            Some(label.clone())
        } else {
            let labels = relevant
                .iter()
                .map(|cached| legacy_default_label(&cached.model, &key.id))
                .collect::<std::collections::HashSet<_>>();
            if labels.len() != 1 {
                return Err(MigrationError::InvalidSchema);
            }
            labels.into_iter().next()
        };
        transaction.execute(
            "INSERT INTO device_identity VALUES(?1,?2,?3,?4,?5,?6)",
            params![
                key.source,
                key.id,
                unique_id,
                endpoint,
                label,
                label.is_some()
            ],
        )?;
        transaction.execute(
            "INSERT INTO device_function VALUES(?1,?2,'main',?3,'主电源','outlet','on_off')",
            params![key.source, key.id, endpoint],
        )?;
        transaction.execute(
            "INSERT INTO endpoint_allocation VALUES(?1,?2,?3,'main')",
            params![endpoint, key.source, key.id],
        )?;
    }
    let uid = transaction
        .query_row("SELECT uid FROM xiaomi_auth WHERE id=1", [], |row| {
            row.get::<_, String>(0)
        })
        .optional()?;
    if let Some(uid) = uid {
        let homes = caches
            .iter()
            .filter(|cached| cached.uid == uid)
            .map(|cached| (&cached.home_id, &cached.group_id))
            .collect::<std::collections::HashSet<_>>();
        if homes.len() == 1 {
            let (home, group) = homes.into_iter().next().expect("one home");
            transaction.execute(
                "INSERT INTO xiaomi_scope VALUES(?1,?2,?3)",
                params![uid, home, group],
            )?;
        }
    }
    transaction.execute_batch(
        "DROP TABLE device_cache_v3; DROP TABLE device_identity_v3; PRAGMA user_version=4;",
    )?;
    Ok(())
}

fn legacy_default_label(model: &str, id: &str) -> String {
    let mut start = id.len().saturating_sub(16);
    while !id.is_char_boundary(start) {
        start += 1;
    }
    let suffix = &id[start..];
    let mut end = model.len().min(32 - suffix.len() - 3);
    while !model.is_char_boundary(end) {
        end -= 1;
    }
    format!("{} ({suffix})", &model[..end])
}

fn migrate_v4_to_v5(
    transaction: &Transaction<'_>,
    legacy_cache: bool,
) -> Result<(), MigrationError> {
    let rows = transaction
        .prepare("SELECT uid, home_id, source, device_id, category, capabilities, functions FROM device_cache")?
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?,
                row.get::<_, String>(3)?, row.get::<_, String>(4)?, row.get::<_, String>(5)?, row.get::<_, String>(6)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if legacy_cache {
        for (uid, home, source, id, category, capabilities, functions) in &rows {
            if category != "outlet" || capabilities != "on_off" {
                return Err(MigrationError::InvalidSchema);
            }
            let functions = super::device::migrate_functions_v4(functions)?;
            transaction.execute(r#"UPDATE device_cache SET functions=?5, capabilities='["on_off"]' WHERE uid=?1 AND home_id=?2 AND source=?3 AND device_id=?4"#, rusqlite::params![uid, home, source, id, functions])?;
        }
    }
    for (uid, home, source, id, _, _, _) in &rows {
        super::device::load_cached(
            transaction,
            uid,
            home,
            &crate::device::DeviceKey::new(source, id),
        )?
        .ok_or(MigrationError::InvalidSchema)?;
    }
    let invalid: bool = transaction.query_row("SELECT EXISTS(SELECT 1 FROM device_function WHERE category!='outlet' OR capabilities!='on_off')", [], |row| row.get(0))?;
    if invalid {
        return Err(MigrationError::InvalidSchema);
    }
    transaction.execute_batch(
        r#"UPDATE device_function SET capabilities='["on_off"]'; PRAGMA user_version=5;"#,
    )?;
    Ok(())
}

fn migrate_v5_to_v6(transaction: &Transaction<'_>) -> Result<(), MigrationError> {
    transaction.execute_batch(include_str!("groups-v6.sql"))?;
    migrate_legacy_group_identities(transaction)?;
    transaction.execute_batch("PRAGMA user_version=6;")?;
    Ok(())
}

/// Move old Xiaomi group main-light identities into the explicit parent role.
/// Only `group.*` keys and cached `mijia.light.group3` models are converted;
/// ordinary devices keep their strict compatibility checks and stay unpublished
/// parents never revive without a fresh confirmed relation.
fn migrate_legacy_group_identities(transaction: &Transaction<'_>) -> Result<(), MigrationError> {
    use crate::device::DeviceKey;
    use rusqlite::params;
    let mut group_models = std::collections::HashMap::new();
    let models = transaction
        .prepare("SELECT source, device_id, model FROM device_cache")?
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (source, device_id, model) in models {
        let known = group_models.entry((source, device_id)).or_insert(false);
        *known |= model == "mijia.light.group3";
    }
    let identities = transaction
        .prepare("SELECT source, device_id, unique_id, endpoint, label FROM device_identity")?
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, u16>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (source, device_id, unique_id, endpoint, label) in identities {
        let key = DeviceKey::new(source, device_id);
        if key.source != "xiaomi" {
            continue;
        }
        let known = key.id.starts_with("group.")
            || group_models
                .get(&(key.source.clone(), key.id.clone()))
                .copied()
                .unwrap_or(false);
        if !known {
            continue;
        }
        if super::device::validate_key(&key).is_err()
            || !(2..=65534).contains(&endpoint)
            || !uuid::Uuid::parse_str(&unique_id)
                .is_ok_and(|uuid| uuid.simple().to_string() == unique_id)
            || label
                .as_deref()
                .is_some_and(|label| super::registry::validate_label(label).is_err())
        {
            return Err(MigrationError::InvalidSchema);
        }
        let allocations = transaction
            .prepare(
                "SELECT endpoint FROM endpoint_allocation WHERE source=?1 AND device_id=?2 ORDER BY endpoint",
            )?
            .query_map(params![key.source, key.id], |row| row.get::<_, u16>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        // The legacy light topology must be exactly one root reservation.
        // Reservations are preserved, never deleted.
        if allocations.len() != 1 || allocations[0] != endpoint {
            return Err(MigrationError::InvalidSchema);
        }
        transaction.execute(
            "UPDATE endpoint_allocation SET slot='group' WHERE source=?1 AND device_id=?2 AND endpoint=?3",
            params![key.source, key.id, endpoint],
        )?;
        transaction.execute(
            "DELETE FROM device_function WHERE source=?1 AND device_id=?2",
            params![key.source, key.id],
        )?;
        transaction.execute(
            "INSERT INTO group_identity VALUES(?1,?2,?3,?4,?5)",
            params![key.source, key.id, unique_id, endpoint, label],
        )?;
        transaction.execute(
            "DELETE FROM device_identity WHERE source=?1 AND device_id=?2",
            params![key.source, key.id],
        )?;
    }
    Ok(())
}
