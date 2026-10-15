//! Current database schema creation and validation.
//!
//! Only the current format is created, read and restored. Older or unknown
//! formats are rejected without modifying the database file; incompatible
//! local data is prepared separately and is not part of the runtime flow.

use rusqlite::Connection;

/// Current application schema version. Version 6 was an unrelated experiment
/// and is deliberately not accepted or migrated.
pub(super) const CURRENT_VERSION: i64 = 7;

pub(super) const SCHEMA: &str = concat!(
    include_str!("schema.sql"),
    include_str!("devices-v4.sql"),
    include_str!("publication.sql")
);

/// Accept only the current schema version. Everything else is rejected
/// untouched so a one-time repair can be planned outside the runtime.
pub(super) fn validate(connection: &Connection) -> Result<(), &'static str> {
    let version = connection
        .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
        .map_err(|_| "Database version cannot be read")?;
    if version != CURRENT_VERSION {
        return Err("Database version is not supported");
    }
    Ok(())
}
