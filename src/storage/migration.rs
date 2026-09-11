use rusqlite::{Connection, OptionalExtension as _, Transaction};
use std::{error::Error as StdError, fmt};

const CURRENT_VERSION: i64 = 1;

const IDENTITY_COLUMNS: &[Column] = &[
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
    let version = connection.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))?;
    match version {
        0 => {
            validate_table(connection, "identity", IDENTITY_COLUMNS)?;
            validate_table(connection, "blobs", BLOB_COLUMNS)?;
            let transaction = connection.transaction()?;
            migrate_v0_to_v1(&transaction)?;
            validate_table(&transaction, "identity", IDENTITY_COLUMNS)?;
            validate_table(&transaction, "blobs", BLOB_COLUMNS)?;
            validate_table(&transaction, "xiaomi_auth", XIAOMI_COLUMNS)?;
            transaction.commit()?;
            Ok(())
        }
        CURRENT_VERSION => {
            validate_table(connection, "identity", IDENTITY_COLUMNS)?;
            validate_table(connection, "blobs", BLOB_COLUMNS)?;
            validate_table(connection, "xiaomi_auth", XIAOMI_COLUMNS)
        }
        _ => Err(MigrationError::UnsupportedVersion),
    }
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
