use super::identity;
use rusqlite::{Connection, OptionalExtension as _, Transaction};
use std::{error::Error as StdError, fmt};

const CURRENT_VERSION: i64 = 4;

const DEVICE_CACHE_COLUMNS: &[Column] = &[
    Column::new("uid", "TEXT", true, true),
    Column::new("source", "TEXT", true, true),
    Column::new("device_id", "TEXT", true, true),
    Column::new("home_id", "TEXT", true, false),
    Column::new("group_id", "TEXT", true, false),
    Column::new("model", "TEXT", true, false),
    Column::new("instance_type", "TEXT", true, false),
    Column::new("category", "TEXT", true, false),
    Column::new("capabilities", "TEXT", true, false),
    Column::new("siid", "INTEGER", true, false),
    Column::new("piid", "INTEGER", true, false),
];
const DEVICE_IDENTITY_COLUMNS: &[Column] = &[
    Column::new("source", "TEXT", true, true),
    Column::new("device_id", "TEXT", true, true),
    Column::new("unique_id", "TEXT", true, false),
    Column::new("endpoint", "INTEGER", true, false),
];

const DEVICE_CACHE_V4_COLUMNS: &[Column] = &[
    Column::new("uid", "TEXT", true, true),
    Column::new("source", "TEXT", true, true),
    Column::new("device_id", "TEXT", true, true),
    Column::new("home_id", "TEXT", true, true),
    Column::new("group_id", "TEXT", true, false),
    Column::new("model", "TEXT", true, false),
    Column::new("instance_type", "TEXT", true, false),
    Column::new("category", "TEXT", true, false),
    Column::new("capabilities", "TEXT", true, false),
    Column::new("functions", "TEXT", true, false),
    Column::new("refresh_siid", "INTEGER", false, false),
    Column::new("refresh_piid", "INTEGER", false, false),
];
const DEVICE_IDENTITY_V4_COLUMNS: &[Column] = &[
    Column::new("source", "TEXT", true, true),
    Column::new("device_id", "TEXT", true, true),
    Column::new("unique_id", "TEXT", true, false),
    Column::new("endpoint", "INTEGER", true, false),
    Column::new("label", "TEXT", false, false),
    Column::new("published", "INTEGER", true, false),
];
const DEVICE_FUNCTION_COLUMNS: &[Column] = &[
    Column::new("source", "TEXT", true, true),
    Column::new("device_id", "TEXT", true, true),
    Column::new("function_id", "TEXT", true, true),
    Column::new("endpoint", "INTEGER", true, false),
    Column::new("label", "TEXT", true, false),
    Column::new("category", "TEXT", true, false),
    Column::new("capabilities", "TEXT", true, false),
];
const ENDPOINT_ALLOCATION_COLUMNS: &[Column] = &[
    Column::new("endpoint", "INTEGER", false, true),
    Column::new("source", "TEXT", true, false),
    Column::new("device_id", "TEXT", true, false),
    Column::new("slot", "TEXT", true, false),
];
const XIAOMI_SCOPE_COLUMNS: &[Column] = &[
    Column::new("uid", "TEXT", true, true),
    Column::new("home_id", "TEXT", true, false),
    Column::new("group_id", "TEXT", true, false),
];

const IDENTITY_COLUMNS: &[Column] = &[
    Column::new("id", "INTEGER", false, true),
    Column::new("bridge_id", "TEXT", true, false),
];
const LEGACY_IDENTITY_COLUMNS: &[Column] = &[
    Column::new("id", "INTEGER", false, true),
    Column::new("bridge_id", "TEXT", true, false),
    Column::new("light_id", "TEXT", true, false),
];
const BLOB_COLUMNS: &[Column] = &[
    Column::new("key", "INTEGER", false, true),
    Column::new("value", "BLOB", true, false),
];
const XIAOMI_COLUMNS: &[Column] = &[
    Column::new("id", "INTEGER", false, true),
    Column::new("uid", "TEXT", true, false),
    Column::new("region", "TEXT", true, false),
    Column::new("oauth_client_uuid", "TEXT", true, false),
    Column::new("redirect_uri", "TEXT", true, false),
    Column::new("access_token", "TEXT", true, false),
    Column::new("refresh_token", "TEXT", true, false),
    Column::new("expires_at", "INTEGER", true, false),
    Column::new("refresh_at", "INTEGER", true, false),
    Column::new("virtual_did", "TEXT", true, false),
    Column::new("private_key_pem", "TEXT", true, false),
    Column::new("certificate_pem", "TEXT", true, false),
];

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

#[derive(Eq, PartialEq)]
struct Column {
    name: &'static str,
    data_type: &'static str,
    not_null: bool,
    primary_key: bool,
}

impl Column {
    const fn new(
        name: &'static str,
        data_type: &'static str,
        not_null: bool,
        primary_key: bool,
    ) -> Self {
        Self {
            name,
            data_type,
            not_null,
            primary_key,
        }
    }
}

pub(super) fn migrate(connection: &mut Connection) -> Result<(), MigrationError> {
    let transaction = connection.transaction()?;
    let version = transaction.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))?;
    match version {
        0..=2 => {
            validate_table(&transaction, "identity", LEGACY_IDENTITY_COLUMNS)?;
            validate_table(&transaction, "blobs", BLOB_COLUMNS)?;
            if version >= 1 {
                validate_table(&transaction, "xiaomi_auth", XIAOMI_COLUMNS)?;
            }
            if version >= 2 {
                validate_table(&transaction, "device_cache", DEVICE_CACHE_COLUMNS)?;
                validate_table(&transaction, "device_identity", DEVICE_IDENTITY_COLUMNS)?;
            }
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
            validate_table(&transaction, "identity", IDENTITY_COLUMNS)?;
            validate_table(&transaction, "blobs", BLOB_COLUMNS)?;
            validate_table(&transaction, "xiaomi_auth", XIAOMI_COLUMNS)?;
            validate_table(&transaction, "device_cache", DEVICE_CACHE_COLUMNS)?;
            validate_table(&transaction, "device_identity", DEVICE_IDENTITY_COLUMNS)?;
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
    validate_table(connection, "identity", IDENTITY_COLUMNS)?;
    validate_table(connection, "blobs", BLOB_COLUMNS)?;
    validate_table(connection, "xiaomi_auth", XIAOMI_COLUMNS)?;
    validate_table(connection, "device_cache", DEVICE_CACHE_V4_COLUMNS)?;
    validate_table(connection, "device_identity", DEVICE_IDENTITY_V4_COLUMNS)?;
    validate_table(connection, "device_function", DEVICE_FUNCTION_COLUMNS)?;
    validate_table(
        connection,
        "endpoint_allocation",
        ENDPOINT_ALLOCATION_COLUMNS,
    )?;
    validate_table(connection, "xiaomi_scope", XIAOMI_SCOPE_COLUMNS)
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

fn validate_table(
    connection: &Connection,
    table: &str,
    expected: &[Column],
) -> Result<(), MigrationError> {
    let object_type = connection
        .query_row(
            "SELECT type FROM sqlite_schema WHERE name = ?1",
            [table],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    if object_type.as_deref() != Some("table") {
        return Err(MigrationError::InvalidSchema);
    }

    let sql = format!("PRAGMA table_info({table})");
    let mut statement = connection.prepare(&sql)?;
    let actual = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, bool>(3)?,
                row.get::<_, bool>(5)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let matches = actual.len() == expected.len()
        && actual.iter().zip(expected).all(|(actual, expected)| {
            actual.0 == expected.name
                && actual.1.eq_ignore_ascii_case(expected.data_type)
                && actual.2 == expected.not_null
                && actual.3 == expected.primary_key
        });
    if matches {
        Ok(())
    } else {
        Err(MigrationError::InvalidSchema)
    }
}

fn migrate_v3_to_v4(transaction: &Transaction<'_>) -> Result<(), MigrationError> {
    use crate::{
        device::{Capability, Category, DeviceDescription, DeviceKey},
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
                description: DeviceDescription {
                    key: DeviceKey::new(source, id),
                    category: Category::Outlet,
                    capabilities: vec![Capability::OnOff],
                },
                functions: vec![ParsedFunction {
                    id: "main".into(),
                    label: "主电源".into(),
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
        caches.iter().any(|cached| {
            cached.device.description.key.source == *source
                && cached.device.description.key.id == *id
        })
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
            .filter(|cached| cached.device.description.key == key)
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
