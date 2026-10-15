use super::{
    StorageError,
    device::{
        CachedPhysicalDevice, DeviceStore, save_cached, validate_cached, validate_key,
        validate_text,
    },
};
use crate::device::{
    Capability, Category, DeviceKey, FunctionDescription, FunctionKey, PhysicalDeviceDescription,
};
use rusqlite::{Connection, OptionalExtension as _, params};
use uuid::Uuid;

#[derive(Clone, Debug)]
pub enum AdmissionError {
    EndpointExhausted,
    IncompatibleFunctions,
    Storage(StorageError),
}
impl From<StorageError> for AdmissionError {
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}
impl std::fmt::Display for AdmissionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EndpointExhausted => f.write_str("Device endpoint numbers are exhausted"),
            Self::IncompatibleFunctions => {
                f.write_str("Device function set is incompatible with its stored identity")
            }
            Self::Storage(error) => error.fmt(f),
        }
    }
}
impl std::error::Error for AdmissionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Storage(error) => Some(error),
            _ => None,
        }
    }
}

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

    pub(crate) fn matches_functions(&self, functions: &[FunctionDescription]) -> bool {
        self.functions.len() == functions.len()
            && self.functions.iter().all(|stored| {
                functions.iter().any(|function| {
                    stored.key.id == function.id
                        && stored.category == function.category
                        && stored.capabilities == function.capabilities
                })
            })
    }
}
struct Record {
    device: StoredDevice,
    named: bool,
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
    /// Restore only published members of this account and home, with one topology check.
    pub fn restore_cached(
        &self,
        uid: &str,
        home_id: &str,
    ) -> Result<Vec<(CachedPhysicalDevice, StoredDevice)>, StorageError> {
        let mut restored = Vec::new();
        for stored in self.list_published()? {
            let Some(cached) = self.load_parsed(uid, home_id, &stored.key)? else {
                continue;
            };
            let functions = cached.physical_description("").functions;
            if !stored.matches_functions(&functions)
                || stored.functions.iter().any(|stored| {
                    !functions.iter().any(|function| {
                        stored.key.id == function.id && stored.capabilities == function.capabilities
                    })
                })
            {
                return Err(self.invalid(
                    "restore Xiaomi device topology",
                    "Cached functions differ from the published identity",
                ));
            }
            restored.push((cached, stored));
        }
        Ok(restored)
    }

    pub fn publish(
        &self,
        description: &PhysicalDeviceDescription,
    ) -> Result<StoredDevice, AdmissionError> {
        let operation = "publish device identity";
        validate_description(description).map_err(|error| self.invalid(operation, error))?;
        self.publish_record(description, None)
    }
    /// Commit a compatible adapter description and its complete published identity together.
    pub fn publish_cached(
        &self,
        cached: &CachedPhysicalDevice,
        label: &str,
    ) -> Result<StoredDevice, AdmissionError> {
        let operation = "publish Xiaomi device";
        validate_cached(cached).map_err(|error| self.invalid(operation, error))?;
        self.publish_record(&cached.physical_description(label), Some(cached))
    }
    fn publish_record(
        &self,
        description: &PhysicalDeviceDescription,
        cached: Option<&CachedPhysicalDevice>,
    ) -> Result<StoredDevice, AdmissionError> {
        let operation = "publish device identity";
        let transaction = self
            .store
            .connection()
            .unchecked_transaction()
            .map_err(|error| self.store.database_error(operation, error))?;
        let result = self.publish_in(&transaction, description, operation)?;
        if let Some(cached) = cached {
            save_cached(&transaction, cached)
                .map_err(|error| self.store.database_error(operation, error))?;
        }
        transaction
            .commit()
            .map_err(|error| self.store.database_error(operation, error))?;
        Ok(result)
    }
    fn publish_in(
        &self,
        connection: &Connection,
        description: &PhysicalDeviceDescription,
        operation: &'static str,
    ) -> Result<StoredDevice, AdmissionError> {
        let record = load_record(connection, &description.key)
            .map_err(|error| self.store.database_error(operation, error))?;
        match record {
            Some(mut record) => {
                if !record.device.matches_functions(&description.functions) {
                    return Err(AdmissionError::IncompatibleFunctions);
                }
                if !record.named {
                    record.device.label = import_label(&description.label);
                }
                connection
                    .execute(
                        "UPDATE device_identity SET label=?3,published=1 WHERE source=?1 AND device_id=?2",
                        params![description.key.source, description.key.id, record.device.label],
                    )
                    .map_err(|error| self.store.database_error(operation, error))?;
                for function in &description.functions {
                    let capabilities =
                        serde_json::to_string(&function.capabilities).map_err(|_| {
                            self.invalid(operation, "Device capabilities cannot be encoded")
                        })?;
                    connection.execute("UPDATE device_function SET capabilities=?4 WHERE source=?1 AND device_id=?2 AND function_id=?3", params![description.key.source, description.key.id, function.id, capabilities]).map_err(|error| self.store.database_error(operation, error))?;
                    record
                        .device
                        .functions
                        .iter_mut()
                        .find(|stored| stored.key.id == function.id)
                        .expect("compatible function exists")
                        .capabilities = function.capabilities.clone();
                }
                record.device.published = true;
                Ok(record.device)
            }
            None => {
                let required = if description.functions.len() == 1 {
                    1
                } else {
                    description.functions.len() + 1
                };
                let last = connection
                    .query_row(
                        "SELECT COALESCE(MAX(endpoint),1) FROM endpoint_allocation",
                        [],
                        |row| row.get::<_, u16>(0),
                    )
                    .map_err(|error| self.store.database_error(operation, error))?;
                if required > 65534usize.saturating_sub(last.into()) {
                    return Err(AdmissionError::EndpointExhausted);
                }
                create_record(connection, description, last + 1)
                    .map_err(|error| self.store.database_error(operation, error).into())
            }
        }
    }
    pub fn load_published(&self, key: &DeviceKey) -> Result<Option<StoredDevice>, StorageError> {
        let operation = "load published device identity";
        validate_key(key).map_err(|error| self.invalid(operation, error))?;
        let record = load_record(self.store.connection(), key)
            .map_err(|error| self.store.database_error(operation, error))?;
        Ok(record.and_then(|record| record.device.published.then_some(record.device)))
    }
    pub fn list_published(&self) -> Result<Vec<StoredDevice>, StorageError> {
        let operation = "list published device identities";
        let keys = self
            .store
            .connection()
            .prepare(
                "SELECT source,device_id FROM device_identity WHERE published=1 ORDER BY endpoint",
            )
            .and_then(|mut statement| {
                statement
                    .query_map([], |row| {
                        Ok(DeviceKey::new(
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .map_err(|error| self.store.database_error(operation, error))?;
        keys.iter()
            .map(|key| {
                load_record(self.store.connection(), key)
                    .and_then(|record| record.ok_or(rusqlite::Error::InvalidQuery))
                    .map(|record| record.device)
                    .map_err(|error| self.store.database_error(operation, error))
            })
            .collect()
    }
    pub fn remove(&self, key: &DeviceKey) -> Result<(), StorageError> {
        let operation = "remove published device";
        validate_key(key).map_err(|error| self.invalid(operation, error))?;
        self.store
            .connection()
            .execute(
                "UPDATE device_identity SET published=0 WHERE source=?1 AND device_id=?2",
                params![key.source, key.id],
            )
            .map(|_| ())
            .map_err(|error| self.store.database_error(operation, error))
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
fn validate_description(description: &PhysicalDeviceDescription) -> Result<(), &'static str> {
    validate_key(&description.key)?;
    if description.functions.is_empty() {
        return Err("Device has no functions");
    }
    let mut ids = std::collections::HashSet::new();
    for function in &description.functions {
        validate_text(&function.id)?;
        if !ids.insert(&function.id)
            || !function.valid_capabilities()
            || function.category == Category::Light
                && (description.functions.len() != 1 || function.id != "main")
        {
            return Err("Device function description is invalid");
        }
    }
    Ok(())
}
fn create_record(
    connection: &Connection,
    description: &PhysicalDeviceDescription,
    endpoint: u16,
) -> rusqlite::Result<StoredDevice> {
    let single = description.functions.len() == 1;
    let unique_id = Uuid::new_v4().simple().to_string();
    let label = import_label(&description.label);
    reserve_endpoint(
        connection,
        &description.key,
        if single {
            &description.functions[0].id
        } else {
            ""
        },
        endpoint,
    )?;
    connection.execute(
        "INSERT INTO device_identity VALUES (?1,?2,?3,?4,?5,1)",
        params![
            description.key.source,
            description.key.id,
            unique_id,
            endpoint,
            label
        ],
    )?;
    let mut functions = Vec::with_capacity(description.functions.len());
    for (index, function) in description.functions.iter().enumerate() {
        let endpoint = if single {
            endpoint
        } else {
            let child = endpoint + index as u16 + 1;
            reserve_endpoint(connection, &description.key, &function.id, child)?;
            child
        };
        let label = import_label(&function.label);
        let category = match function.category {
            Category::Outlet => "outlet",
            Category::Light => "light",
        };
        let capabilities = serde_json::to_string(&function.capabilities)
            .map_err(|_| rusqlite::Error::InvalidQuery)?;
        connection.execute(
            "INSERT INTO device_function VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![
                description.key.source,
                description.key.id,
                function.id,
                endpoint,
                label,
                category,
                capabilities
            ],
        )?;
        functions.push(StoredFunction {
            key: FunctionKey::new(description.key.clone(), &function.id),
            endpoint,
            label,
            category: function.category,
            capabilities: function.capabilities.clone(),
        });
    }
    Ok(StoredDevice {
        key: description.key.clone(),
        unique_id,
        endpoint,
        label,
        functions,
        published: true,
    })
}
fn reserve_endpoint(
    connection: &Connection,
    key: &DeviceKey,
    slot: &str,
    endpoint: u16,
) -> rusqlite::Result<()> {
    connection.execute(
        "INSERT INTO endpoint_allocation VALUES (?1,?2,?3,?4)",
        params![endpoint, key.source, key.id, slot],
    )?;
    Ok(())
}
fn load_record(connection: &Connection, key: &DeviceKey) -> rusqlite::Result<Option<Record>> {
    let identity = connection
        .query_row(
            "SELECT unique_id, endpoint, label, published FROM device_identity
             WHERE source=?1 AND device_id=?2",
            params![key.source, key.id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, u16>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, bool>(3)?,
                ))
            },
        )
        .optional()?;
    let Some((unique_id, endpoint, label, published)) = identity else {
        return Ok(None);
    };
    let valid_uuid =
        Uuid::parse_str(&unique_id).is_ok_and(|uuid| uuid.simple().to_string() == unique_id);
    if !valid_uuid
        || !(2..=65534).contains(&endpoint)
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
    Ok(Some(Record {
        named: label.is_some(),
        device: StoredDevice {
            key: key.clone(),
            unique_id,
            endpoint,
            label: label.unwrap_or_default(),
            functions,
            published,
        },
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
