use super::{
    StorageError, Store,
    device::{
        CachedPhysicalDevice, DeviceIdentity, DeviceStore, save_cached, validate_cached,
        validate_key, validate_text,
    },
};
use crate::device::{Capability, Category, DeviceKey, FunctionKey, PhysicalDeviceDescription};
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
    pub fn publish(
        &self,
        description: &PhysicalDeviceDescription,
    ) -> Result<StoredDevice, StorageError> {
        let operation = "publish device identity";
        validate_description(description).map_err(|error| self.invalid(operation, error))?;
        let transaction = self
            .store
            .connection()
            .unchecked_transaction()
            .map_err(|error| self.store.database_error(operation, error))?;
        let result = self.publish_in(&transaction, description, operation)?;
        transaction
            .commit()
            .map_err(|error| self.store.database_error(operation, error))?;
        Ok(result)
    }
    /// Commit a compatible adapter description and its complete published identity together.
    pub fn publish_cached(
        &self,
        cached: &CachedPhysicalDevice,
        label: &str,
    ) -> Result<StoredDevice, StorageError> {
        let operation = "publish Xiaomi device";
        validate_cached(cached).map_err(|error| self.invalid(operation, error))?;
        let description = cached.physical_description(label);
        validate_description(&description).map_err(|error| self.invalid(operation, error))?;
        let transaction = self
            .store
            .connection()
            .unchecked_transaction()
            .map_err(|error| self.store.database_error(operation, error))?;
        let result = self.publish_in(&transaction, &description, operation)?;
        save_cached(&transaction, cached)
            .map_err(|error| self.store.database_error(operation, error))?;
        transaction
            .commit()
            .map_err(|error| self.store.database_error(operation, error))?;
        Ok(result)
    }
    fn require_capacity(
        &self,
        connection: &Connection,
        required: usize,
        operation: &'static str,
    ) -> Result<(), StorageError> {
        if !has_capacity(connection, required)
            .map_err(|error| self.store.database_error(operation, error))?
        {
            return Err(self.invalid(operation, "Device endpoint numbers are exhausted"));
        }
        Ok(())
    }
    fn publish_in(
        &self,
        connection: &Connection,
        description: &PhysicalDeviceDescription,
        operation: &'static str,
    ) -> Result<StoredDevice, StorageError> {
        let record = load_record(connection, &description.key)
            .map_err(|error| self.store.database_error(operation, error))?;
        match record {
            Some(record) => {
                let matches = record.device.functions.len() == description.functions.len()
                    && record.device.functions.iter().all(|stored| {
                        description.functions.iter().any(|function| {
                            stored.key.id == function.id
                                && stored.category == function.category
                                && stored.capabilities == function.capabilities
                        })
                    });
                if !matches {
                    return Err(self.invalid(
                        operation,
                        "Device function set is incompatible with its stored identity",
                    ));
                }
                if !record.named {
                    connection
                        .execute(
                            "UPDATE device_identity SET label=?3 WHERE source=?1 AND device_id=?2",
                            params![
                                description.key.source,
                                description.key.id,
                                import_label(&description.label)
                            ],
                        )
                        .map_err(|error| self.store.database_error(operation, error))?;
                }
                connection
                    .execute(
                        "UPDATE device_identity SET published=1 WHERE source=?1 AND device_id=?2",
                        params![description.key.source, description.key.id],
                    )
                    .map_err(|error| self.store.database_error(operation, error))?;
            }
            None => {
                let required = if description.functions.len() == 1 {
                    1
                } else {
                    description.functions.len() + 1
                };
                self.require_capacity(connection, required, operation)?;
                create_record(connection, description, true)
                    .map_err(|error| self.store.database_error(operation, error))?;
            }
        }
        let result = load_record(connection, &description.key)
            .and_then(|record| record.ok_or(rusqlite::Error::InvalidQuery))
            .map_err(|error| self.store.database_error(operation, error))?
            .device;
        Ok(result)
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
    pub fn set_function_label(&self, key: &FunctionKey, label: &str) -> Result<(), StorageError> {
        let operation = "write device function label";
        validate_key(&key.device)
            .and_then(|()| validate_text(&key.id))
            .and_then(|()| validate_label(label))
            .map_err(|error| self.invalid(operation, error))?;
        let changed=self.store.connection().execute("UPDATE device_function SET label=?4 WHERE source=?1 AND device_id=?2 AND function_id=?3",params![key.device.source,key.device.id,key.id,label]).map_err(|error|self.store.database_error(operation,error))?;
        if changed == 0 {
            return Err(self.invalid(operation, "Function identity does not exist"));
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
            || function.category != Category::Outlet
            || function.capabilities != [Capability::OnOff]
        {
            return Err("Device function description is invalid");
        }
    }
    Ok(())
}
fn has_capacity(connection: &Connection, required: usize) -> rusqlite::Result<bool> {
    let last = connection.query_row(
        "SELECT COALESCE(MAX(endpoint),1) FROM endpoint_allocation",
        [],
        |row| row.get::<_, u32>(0),
    )?;
    Ok(required <= 65534u32.saturating_sub(last) as usize)
}
fn allocate(connection: &Connection, key: &DeviceKey, slot: &str) -> rusqlite::Result<u16> {
    let endpoint = connection.query_row(
        "SELECT COALESCE(MAX(endpoint),1)+1 FROM endpoint_allocation",
        [],
        |row| row.get::<_, u32>(0),
    )?;
    if endpoint > 65534 {
        return Err(rusqlite::Error::InvalidQuery);
    }
    connection.execute(
        "INSERT INTO endpoint_allocation VALUES (?1,?2,?3,?4)",
        params![endpoint, key.source, key.id, slot],
    )?;
    Ok(endpoint as u16)
}
fn create_record(
    connection: &Connection,
    description: &PhysicalDeviceDescription,
    published: bool,
) -> rusqlite::Result<()> {
    let single = description.functions.len() == 1;
    let endpoint = allocate(
        connection,
        &description.key,
        if single {
            &description.functions[0].id
        } else {
            ""
        },
    )?;
    connection.execute(
        "INSERT INTO device_identity VALUES (?1,?2,?3,?4,?5,?6)",
        params![
            description.key.source,
            description.key.id,
            Uuid::new_v4().simple().to_string(),
            endpoint,
            published.then(|| import_label(&description.label)),
            published
        ],
    )?;
    for function in &description.functions {
        let endpoint = if single {
            endpoint
        } else {
            allocate(connection, &description.key, &function.id)?
        };
        connection.execute(
            "INSERT INTO device_function VALUES (?1,?2,?3,?4,?5,'outlet','on_off')",
            params![
                description.key.source,
                description.key.id,
                function.id,
                endpoint,
                import_label(&function.label)
            ],
        )?;
    }
    Ok(())
}
fn load_record(connection: &Connection, key: &DeviceKey) -> rusqlite::Result<Option<Record>> {
    let identity=connection.query_row("SELECT unique_id,endpoint,label,published FROM device_identity WHERE source=?1 AND device_id=?2",params![key.source,key.id],|row|Ok((row.get::<_,String>(0)?,row.get::<_,u16>(1)?,row.get::<_,Option<String>>(2)?,row.get::<_,bool>(3)?))).optional()?;
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
    let functions=connection.prepare("SELECT function_id,endpoint,label,category,capabilities FROM device_function WHERE source=?1 AND device_id=?2 ORDER BY endpoint").and_then(|mut statement|statement.query_map(params![key.source,key.id],|row|Ok((row.get::<_,String>(0)?,row.get::<_,u16>(1)?,row.get::<_,String>(2)?,row.get::<_,String>(3)?,row.get::<_,String>(4)?)))?.collect::<rusqlite::Result<Vec<_>>>())?;
    if functions.is_empty() {
        return Err(rusqlite::Error::InvalidQuery);
    }
    let single = functions.len() == 1;
    let root_slot = if single { functions[0].0.as_str() } else { "" };
    check_allocation(connection, key, root_slot, endpoint)?;
    let mut stored = Vec::new();
    for (id, function_endpoint, function_label, category, capabilities) in functions {
        if validate_text(&id).is_err()
            || validate_label(&function_label).is_err()
            || category != "outlet"
            || capabilities != "on_off"
            || single && function_endpoint != endpoint
            || !single && function_endpoint == endpoint
        {
            return Err(rusqlite::Error::InvalidQuery);
        }
        check_allocation(connection, key, &id, function_endpoint)?;
        stored.push(StoredFunction {
            key: FunctionKey::new(key.clone(), id),
            endpoint: function_endpoint,
            label: function_label,
            category: Category::Outlet,
            capabilities: vec![Capability::OnOff],
        });
    }
    Ok(Some(Record {
        named: label.is_some(),
        device: StoredDevice {
            key: key.clone(),
            unique_id,
            endpoint,
            label: label.unwrap_or_default(),
            functions: stored,
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
pub(super) fn load_or_create_identity(
    store: &Store,
    key: &DeviceKey,
) -> Result<DeviceIdentity, StorageError> {
    let operation = "load or create real device identity";
    validate_key(key).map_err(|error| StorageError::new(store.path(), operation, error))?;
    let transaction = store
        .connection()
        .unchecked_transaction()
        .map_err(|error| store.database_error(operation, error))?;
    if load_record(&transaction, key)
        .map_err(|error| store.database_error(operation, error))?
        .is_none()
    {
        if !has_capacity(&transaction, 1).map_err(|error| store.database_error(operation, error))? {
            return Err(StorageError::new(
                store.path(),
                operation,
                "Device endpoint numbers are exhausted",
            ));
        }
        create_record(
            &transaction,
            &PhysicalDeviceDescription {
                key: key.clone(),
                label: String::new(),
                functions: vec![crate::device::FunctionDescription {
                    id: "main".into(),
                    label: "主电源".into(),
                    category: Category::Outlet,
                    capabilities: vec![Capability::OnOff],
                }],
            },
            false,
        )
        .map_err(|error| store.database_error(operation, error))?;
    }
    let record = load_record(&transaction, key)
        .and_then(|record| record.ok_or(rusqlite::Error::InvalidQuery))
        .map_err(|error| store.database_error(operation, error))?;
    transaction
        .commit()
        .map_err(|error| store.database_error(operation, error))?;
    Ok(DeviceIdentity {
        unique_id: record.device.unique_id,
        endpoint: record.device.endpoint,
    })
}
