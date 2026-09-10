use rusqlite::Connection;
use uuid::Uuid;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Identity {
    pub bridge_id: String,
    pub light_id: String,
}

pub(super) fn load(connection: &Connection) -> rusqlite::Result<Identity> {
    connection.query_row(
        "SELECT bridge_id, light_id FROM identity WHERE id = 1",
        [],
        |row| {
            Ok(Identity {
                bridge_id: row.get(0)?,
                light_id: row.get(1)?,
            })
        },
    )
}

pub(super) fn insert(connection: &Connection) -> rusqlite::Result<()> {
    connection
        .execute(
            "INSERT INTO identity (id, bridge_id, light_id) VALUES (1, ?1, ?2)",
            (
                Uuid::new_v4().simple().to_string(),
                Uuid::new_v4().simple().to_string(),
            ),
        )
        .map(|_| ())
}
