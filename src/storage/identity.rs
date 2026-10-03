use rusqlite::Connection;
use uuid::Uuid;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Identity {
    pub bridge_id: String,
}

pub(super) fn load(connection: &Connection) -> rusqlite::Result<Identity> {
    let mut statement = connection.prepare("SELECT id, bridge_id FROM identity")?;
    let mut rows = statement.query([])?;
    let row = rows.next()?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
    let id = row.get::<_, i64>(0)?;
    let bridge_id = row.get::<_, String>(1)?;
    if id != 1
        || rows.next()?.is_some()
        || !Uuid::parse_str(&bridge_id).is_ok_and(|uuid| uuid.simple().to_string() == bridge_id)
    {
        return Err(rusqlite::Error::InvalidQuery);
    }
    Ok(Identity { bridge_id })
}

pub(super) fn insert(connection: &Connection) -> rusqlite::Result<()> {
    connection
        .execute(
            "INSERT INTO identity (id, bridge_id) VALUES (1, ?1)",
            [Uuid::new_v4().simple().to_string()],
        )
        .map(|_| ())
}
