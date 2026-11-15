//! Creation and validation of the current database schema.

use rusqlite::Connection;

/// Current application schema version.
pub(super) const CURRENT_VERSION: i64 = 13;

pub(super) const SCHEMA: &str = include_str!("schema.sql");

/// Accept only the current schema version; reject other formats untouched.
pub(super) fn validate(connection: &Connection) -> Result<(), &'static str> {
    let version = connection
        .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
        .map_err(|_| "Database version cannot be read")?;
    if version != CURRENT_VERSION {
        return Err("Database version is not supported. Choose a new data directory.");
    }
    Ok(())
}
