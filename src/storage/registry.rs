use super::{
    StorageError,
    device::{DeviceStore, validate_key, validate_text},
};
use crate::device::{
    Capability, Category, DeviceKey, FunctionDescription, FunctionKey, PhysicalDeviceDescription,
};
use rusqlite::{Connection, OptionalExtension as _, params};
use uuid::Uuid;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredFunction {
    pub key: FunctionKey,
    pub endpoint: u16,
    pub label: String,
    pub category: Category,
    pub capabilities: Vec<Capability>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredDevice {
    pub key: DeviceKey,
    pub unique_id: String,
    pub endpoint: u16,
    pub label: String,
    pub functions: Vec<StoredFunction>,
    pub published: bool,
    pub configuration_version: u32,
}
impl StoredDevice {
    /// Build runtime metadata from the identity and names committed at admission.
    pub fn description(&self) -> PhysicalDeviceDescription {
        PhysicalDeviceDescription {
            key: self.key.clone(),
            label: self.label.clone(),
            functions: self
                .functions
                .iter()
                .map(|function| FunctionDescription {
                    id: function.key.id.clone(),
                    label: function.label.clone(),
                    category: function.category,
                    capabilities: function.capabilities.clone(),
                })
                .collect(),
        }
    }
}

/// Import a name without splitting a UTF-8 character. Protocol writes use validation instead.
pub fn import_label(label: &str) -> String {
    let mut end = label.len().min(32);
    while !label.is_char_boundary(end) {
        end -= 1;
    }
    label[..end].to_owned()
}
pub(super) fn validate_label(label: &str) -> Result<(), &'static str> {
    if label.len() > 32 {
        Err("Device label exceeds 32 bytes")
    } else {
        Ok(())
    }
}
impl DeviceStore {
    pub fn load_published(&self, key: &DeviceKey) -> Result<Option<StoredDevice>, StorageError> {
        let operation = "load published device identity";
        validate_key(key).map_err(|error| self.invalid(operation, error))?;
        let record = load_stored(self.store.connection(), key)
            .map_err(|error| self.store.database_error(operation, error))?;
        Ok(record.filter(|record| record.published))
    }
    pub fn set_device_label(&self, key: &DeviceKey, label: &str) -> Result<(), StorageError> {
        let operation = "write device label";
        validate_key(key)
            .and_then(|()| validate_label(label))
            .map_err(|error| self.invalid(operation, error))?;
        let changed = self
            .store
            .connection()
            .execute(
                "UPDATE device_identity SET label=?3 WHERE source=?1 AND device_id=?2",
                params![key.source, key.id, label],
            )
            .map_err(|error| self.store.database_error(operation, error))?;
        if changed == 0 {
            return Err(self.invalid(operation, "Device identity does not exist"));
        }
        Ok(())
    }
}
pub(super) fn load_stored(
    connection: &Connection,
    key: &DeviceKey,
) -> rusqlite::Result<Option<StoredDevice>> {
    let identity = connection
        .query_row(
            "SELECT unique_id, endpoint, label, published, configuration_version FROM device_identity
             WHERE source=?1 AND device_id=?2",
            params![key.source, key.id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, u16>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, bool>(3)?,
                    row.get::<_, u32>(4)?,
                ))
            },
        )
        .optional()?;
    let Some((unique_id, endpoint, label, published, configuration_version)) = identity else {
        return Ok(None);
    };
    let valid_uuid =
        Uuid::parse_str(&unique_id).is_ok_and(|uuid| uuid.simple().to_string() == unique_id);
    if !valid_uuid
        || !(2..=65534).contains(&endpoint)
        || configuration_version == 0
        || label
            .as_deref()
            .is_some_and(|label| validate_label(label).is_err())
        || published && label.is_none()
    {
        return Err(rusqlite::Error::InvalidQuery);
    }
    let functions = connection
        .prepare(
            "SELECT function_id, endpoint, label, category, capabilities FROM device_function
             WHERE source=?1 AND device_id=?2 ORDER BY endpoint",
        )
        .and_then(|mut statement| {
            statement
                .query_map(params![key.source, key.id], |row| {
                    let id = row.get::<_, String>(0)?;
                    let label = row.get::<_, String>(2)?;
                    if validate_text(&id).is_err() || validate_label(&label).is_err() {
                        return Err(rusqlite::Error::InvalidQuery);
                    }
                    let category = match row.get::<_, String>(3)?.as_str() {
                        "outlet" => Category::Outlet,
                        "light" => Category::Light,
                        _ => return Err(rusqlite::Error::InvalidQuery),
                    };
                    let capabilities: Vec<Capability> =
                        serde_json::from_str(&row.get::<_, String>(4)?)
                            .map_err(|_| rusqlite::Error::InvalidQuery)?;
                    if !(FunctionDescription {
                        id: id.clone(),
                        label: label.clone(),
                        category,
                        capabilities: capabilities.clone(),
                    })
                    .valid_capabilities()
                    {
                        return Err(rusqlite::Error::InvalidQuery);
                    }
                    Ok(StoredFunction {
                        key: FunctionKey::new(key.clone(), id),
                        endpoint: row.get(1)?,
                        label,
                        category,
                        capabilities,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()
        })?;
    if functions.is_empty() {
        return Err(rusqlite::Error::InvalidQuery);
    }
    let single = functions.len() == 1;
    let root_slot = if single {
        functions[0].key.id.as_str()
    } else {
        ""
    };
    check_allocation(connection, key, root_slot, endpoint)?;
    for function in &functions {
        if single && function.endpoint != endpoint || !single && function.endpoint == endpoint {
            return Err(rusqlite::Error::InvalidQuery);
        }
        check_allocation(connection, key, &function.key.id, function.endpoint)?;
    }
    Ok(Some(StoredDevice {
        key: key.clone(),
        unique_id,
        endpoint,
        label: label.unwrap_or_default(),
        functions,
        published,
        configuration_version,
    }))
}
fn check_allocation(
    connection: &Connection,
    key: &DeviceKey,
    slot: &str,
    endpoint: u16,
) -> rusqlite::Result<()> {
    let allocated = connection
        .query_row(
            "SELECT endpoint FROM endpoint_allocation WHERE source=?1 AND device_id=?2 AND slot=?3",
            params![key.source, key.id, slot],
            |row| row.get::<_, u16>(0),
        )
        .optional()?;
    if allocated != Some(endpoint) {
        return Err(rusqlite::Error::InvalidQuery);
    }
    Ok(())
}
