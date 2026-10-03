use super::identity;
use rusqlite::{Connection, OptionalExtension as _, Transaction};
use std::{error::Error as StdError, fmt};

const CURRENT_VERSION: i64 = 3;

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
    validate_table(connection, "device_cache", DEVICE_CACHE_COLUMNS)?;
    validate_table(connection, "device_identity", DEVICE_IDENTITY_COLUMNS)
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
