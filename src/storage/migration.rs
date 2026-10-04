use super::identity;
use rusqlite::{Connection, OptionalExtension as _, Transaction};
use std::{error::Error as StdError, fmt};

const CURRENT_VERSION: i64 = 4;

pub(super) const SCHEMA: &str = concat!(include_str!("schema.sql"), include_str!("devices-v4.sql"));

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
    let transaction = connection.transaction()?;
    let version = transaction.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))?;
    match version {
        0..=2 => {
            identity::load(&transaction)?;
            if version == 0 {
                migrate_v0_to_v1(&transaction)?;
            }
            if version < 2 {
                migrate_v1_to_v2(&transaction)?;
            }
            migrate_v2_to_v3(&transaction)?;
            migrate_v3_to_v4(&transaction)?;
        }
        3 => {
            identity::load(&transaction)?;
            migrate_v3_to_v4(&transaction)?;
        }
        CURRENT_VERSION => {}
        _ => return Err(MigrationError::UnsupportedVersion),
    }
    validate_current_schema(&transaction)?;
    transaction.commit()?;
    Ok(())
}

fn validate_current_schema(connection: &Connection) -> Result<(), MigrationError> {
    let expected = Connection::open_in_memory()?;
    expected.execute_batch(SCHEMA)?;
    let mut tables = expected.prepare("SELECT name FROM sqlite_schema WHERE type = 'table'")?;
    for table in tables.query_map([], |row| row.get::<_, String>(0))? {
        let table = table?;
        if table_columns(connection, &table)? != table_columns(&expected, &table)? {
            return Err(MigrationError::InvalidSchema);
        }
    }
    Ok(())
}

fn table_columns(
    connection: &Connection,
    table: &str,
) -> Result<Vec<(String, String, bool, u32)>, MigrationError> {
    let is_table = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name = ?1 AND type = 'table')",
        [table],
        |row| row.get::<_, bool>(0),
    )?;
    if !is_table {
        return Err(MigrationError::InvalidSchema);
    }
    let mut columns =
        connection.prepare("SELECT name, type, \"notnull\", pk FROM pragma_table_info(?1)")?;
    Ok(columns
        .query_map([table], |row| {
            Ok((
                row.get(0)?,
                row.get::<_, String>(1)?.to_ascii_uppercase(),
                row.get(2)?,
                row.get(3)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?)
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
