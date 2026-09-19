use rusqlite::Connection;
use std::{error::Error as StdError, fmt};

const CURRENT_VERSION: i64 = 9;

#[derive(Debug)]
pub(super) enum MigrationError {
    Database(rusqlite::Error),
    InvalidSchema,
    UnsupportedVersion,
}
impl fmt::Display for MigrationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(_) => f.write_str("SQLite operation failed"),
            Self::InvalidSchema => f.write_str("Database schema is invalid"),
            Self::UnsupportedVersion => f.write_str("Database version is not supported"),
        }
    }
}
impl StdError for MigrationError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Database(e) => Some(e),
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
    let version = connection.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))?;
    if version != CURRENT_VERSION {
        return Err(MigrationError::UnsupportedVersion);
    }
    let integrity =
        connection.query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))?;
    if integrity != "ok" {
        return Err(MigrationError::InvalidSchema);
    }
    let has_foreign_key_violation = connection.prepare("PRAGMA foreign_key_check")?.exists([])?;
    if has_foreign_key_violation {
        return Err(MigrationError::InvalidSchema);
    }
    let expected = Connection::open_in_memory()?;
    expected.execute_batch(include_str!("schema.sql"))?;
    if table_definitions(connection)? != table_definitions(&expected)? {
        return Err(MigrationError::InvalidSchema);
    }
    Ok(())
}

fn table_definitions(connection: &Connection) -> Result<Vec<(String, String)>, rusqlite::Error> {
    let mut statement = connection.prepare(
        "SELECT name,sql FROM sqlite_schema
         WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
    )?;
    statement
        .query_map([], |row| {
            let name: String = row.get(0)?;
            let sql: String = row.get(1)?;
            Ok((name, normalize_sql(&sql)))
        })?
        .collect()
}

fn normalize_sql(sql: &str) -> String {
    sql.replace('"', "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}
