use super::{StorageError, Store};
use crate::{
    device::{Capability, Category, DeviceKey, FunctionDescription, PhysicalDeviceDescription},
    xiaomi::{
        capability::{ParsedDevice, ParsedFunction, PropertyBinding, is_outlet_type},
        cloud::group_id,
    },
};
use rusqlite::{Connection, OptionalExtension as _, params};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CachedPhysicalDevice {
    pub uid: String,
    pub home_id: String,
    pub group_id: String,
    pub model: String,
    pub instance_type: String,
    pub device: ParsedDevice,
}
impl CachedPhysicalDevice {
    pub fn physical_description(&self, label: impl Into<String>) -> PhysicalDeviceDescription {
        PhysicalDeviceDescription {
            key: self.device.key.clone(),
            label: label.into(),
            functions: self
                .device
                .functions
                .iter()
                .map(|function| function.description.clone())
                .collect(),
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CachedScope {
    pub uid: String,
    pub home_id: String,
    pub group_id: String,
}
#[derive(Clone)]
pub struct DeviceStore {
    pub(super) store: Store,
}
#[derive(Serialize, Deserialize)]
struct CachedFunction {
    id: String,
    label: String,
    siid: u32,
    piid: u32,
}
impl DeviceStore {
    pub(super) fn new(store: Store) -> Self {
        Self { store }
    }
    pub fn load_parsed(
        &self,
        uid: &str,
        home_id: &str,
        key: &DeviceKey,
    ) -> Result<Option<CachedPhysicalDevice>, StorageError> {
        let operation = "load static Xiaomi device description";
        validate_text(uid)
            .and_then(|()| validate_text(home_id))
            .and_then(|()| validate_key(key))
            .map_err(|error| self.invalid(operation, error))?;
        load_cached(self.store.connection(), uid, home_id, key)
            .map_err(|error| self.store.database_error(operation, error))
    }
    pub fn save_scope(&self, scope: &CachedScope) -> Result<(), StorageError> {
        let operation = "save current Xiaomi home";
        validate_scope(scope).map_err(|error| self.invalid(operation, error))?;
        save_scope(self.store.connection(), scope)
            .map_err(|error| self.store.database_error(operation, error))
    }
    pub fn load_scope(&self, uid: &str) -> Result<Option<CachedScope>, StorageError> {
        let operation = "load current Xiaomi home";
        validate_text(uid).map_err(|error| self.invalid(operation, error))?;
        let scope = self
            .store
            .connection()
            .query_row(
                "SELECT home_id,group_id FROM xiaomi_scope WHERE uid=?1",
                [uid],
                |row| {
                    Ok(CachedScope {
                        uid: uid.into(),
                        home_id: row.get(0)?,
                        group_id: row.get(1)?,
                    })
                },
            )
            .optional()
            .map_err(|error| self.store.database_error(operation, error))?;
        if let Some(scope) = &scope {
            validate_scope(scope).map_err(|error| self.invalid(operation, error))?;
        }
        Ok(scope)
    }
    pub(crate) fn invalid(&self, operation: &'static str, error: &'static str) -> StorageError {
        StorageError::new(self.store.path(), operation, error)
    }
}
fn validate_scope(scope: &CachedScope) -> Result<(), &'static str> {
    validate_text(&scope.uid)?;
    validate_text(&scope.home_id)?;
    if scope.group_id != group_id(&scope.uid, &scope.home_id) {
        return Err("Stored Xiaomi home is invalid");
    }
    Ok(())
}
pub(super) fn validate_text(value: &str) -> Result<(), &'static str> {
    if value.is_empty() || value.chars().any(char::is_control) {
        Err("Stored device metadata is invalid")
    } else {
        Ok(())
    }
}
pub(super) fn validate_key(key: &DeviceKey) -> Result<(), &'static str> {
    validate_text(&key.source)?;
    validate_text(&key.id)
}
pub(super) fn validate_cached(cached: &CachedPhysicalDevice) -> Result<(), &'static str> {
    validate_scope(&CachedScope {
        uid: cached.uid.clone(),
        home_id: cached.home_id.clone(),
        group_id: cached.group_id.clone(),
    })?;
    validate_text(&cached.model)?;
    validate_text(&cached.instance_type)?;
    validate_key(&cached.device.key)?;
    if cached.device.key.source != "xiaomi"
        || !is_outlet_type(&cached.instance_type)
        || cached.device.functions.is_empty()
    {
        return Err("Stored Xiaomi device description is invalid");
    }
    let mut ids = std::collections::HashSet::new();
    let mut bindings = std::collections::HashSet::new();
    for function in &cached.device.functions {
        validate_text(&function.description.id)?;
        if !ids.insert(&function.description.id)
            || !bindings.insert((function.power.siid, function.power.piid))
            || function.description.category != Category::Outlet
            || function.description.capabilities != [Capability::OnOff]
            || function.power.siid == 0
            || function.power.piid == 0
        {
            return Err("Stored Xiaomi function description is invalid");
        }
    }
    if let Some(binding) = cached.device.refresh_trigger
        && (binding.siid == 0
            || binding.piid == 0
            || bindings.contains(&(binding.siid, binding.piid)))
    {
        return Err("Stored Xiaomi refresh binding is invalid");
    }
    Ok(())
}
pub(super) fn save_cached(
    connection: &Connection,
    cached: &CachedPhysicalDevice,
) -> rusqlite::Result<()> {
    let functions = cached
        .device
        .functions
        .iter()
        .map(|function| CachedFunction {
            id: function.description.id.clone(),
            label: function.description.label.clone(),
            siid: function.power.siid,
            piid: function.power.piid,
        })
        .collect::<Vec<_>>();
    let functions = serde_json::to_string(&functions).map_err(|_| rusqlite::Error::InvalidQuery)?;
    let key = &cached.device.key;
    connection.execute(
        "INSERT INTO device_cache (
            uid, source, device_id, home_id, group_id, model, instance_type,
            category, capabilities, functions, refresh_siid, refresh_piid
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'outlet', 'on_off', ?8, ?9, ?10)
         ON CONFLICT(uid, home_id, source, device_id) DO UPDATE SET
            group_id=excluded.group_id, model=excluded.model,
            instance_type=excluded.instance_type, category=excluded.category,
            capabilities=excluded.capabilities, functions=excluded.functions,
            refresh_siid=excluded.refresh_siid, refresh_piid=excluded.refresh_piid",
        params![
            cached.uid,
            key.source,
            key.id,
            cached.home_id,
            cached.group_id,
            cached.model,
            cached.instance_type,
            functions,
            cached.device.refresh_trigger.map(|binding| binding.siid),
            cached.device.refresh_trigger.map(|binding| binding.piid)
        ],
    )?;
    Ok(())
}
fn load_cached(
    connection: &Connection,
    uid: &str,
    home_id: &str,
    key: &DeviceKey,
) -> rusqlite::Result<Option<CachedPhysicalDevice>> {
    let data = connection
        .query_row(
            "SELECT group_id, model, instance_type, category, capabilities, functions,
                    refresh_siid, refresh_piid
             FROM device_cache WHERE uid=?1 AND home_id=?2 AND source=?3 AND device_id=?4",
            params![uid, home_id, key.source, key.id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, Option<u32>>(6)?,
                    row.get::<_, Option<u32>>(7)?,
                ))
            },
        )
        .optional()?;
    let Some((
        group_id,
        model,
        instance_type,
        category,
        capabilities,
        functions,
        refresh_siid,
        refresh_piid,
    )) = data
    else {
        return Ok(None);
    };
    if category != "outlet" || capabilities != "on_off" {
        return Err(rusqlite::Error::InvalidQuery);
    }
    let functions: Vec<CachedFunction> =
        serde_json::from_str(&functions).map_err(|_| rusqlite::Error::InvalidQuery)?;
    let refresh_trigger = match (refresh_siid, refresh_piid) {
        (None, None) => None,
        (Some(siid), Some(piid)) => Some(PropertyBinding { siid, piid }),
        _ => return Err(rusqlite::Error::InvalidQuery),
    };
    let cached = CachedPhysicalDevice {
        uid: uid.into(),
        home_id: home_id.into(),
        group_id,
        model,
        instance_type,
        device: ParsedDevice {
            key: key.clone(),
            functions: functions
                .into_iter()
                .map(|function| ParsedFunction {
                    description: FunctionDescription {
                        id: function.id,
                        label: function.label,
                        category: Category::Outlet,
                        capabilities: vec![Capability::OnOff],
                    },
                    power: PropertyBinding {
                        siid: function.siid,
                        piid: function.piid,
                    },
                })
                .collect(),
            refresh_trigger,
        },
    };
    validate_cached(&cached).map_err(|_| rusqlite::Error::InvalidQuery)?;
    Ok(Some(cached))
}

fn save_scope(connection: &Connection, scope: &CachedScope) -> rusqlite::Result<()> {
    connection.execute(
        "INSERT INTO xiaomi_scope VALUES (?1, ?2, ?3)
         ON CONFLICT(uid) DO UPDATE SET home_id=excluded.home_id, group_id=excluded.group_id",
        params![scope.uid, scope.home_id, scope.group_id],
    )?;
    Ok(())
}
