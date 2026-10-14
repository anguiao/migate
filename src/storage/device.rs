use super::{StorageError, Store};
use crate::{
    device::{Capability, Category, DeviceKey, PhysicalDeviceDescription},
    xiaomi::{
        capability::{
            ParsedDevice, ParsedFunction, PropertyBinding, is_light_group, is_light_type,
            is_outlet_type,
        },
        cloud::group_id,
    },
};
use rusqlite::{Connection, OptionalExtension as _, params};

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
        validate_scope(&scope.uid, &scope.home_id, &scope.group_id)
            .map_err(|error| self.invalid(operation, error))?;
        self.store
            .connection()
            .execute(
                "INSERT INTO xiaomi_scope VALUES (?1, ?2, ?3)
                 ON CONFLICT(uid) DO UPDATE SET home_id=excluded.home_id, group_id=excluded.group_id",
                params![scope.uid, scope.home_id, scope.group_id],
            )
            .map(|_| ())
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
            validate_scope(&scope.uid, &scope.home_id, &scope.group_id)
                .map_err(|error| self.invalid(operation, error))?;
        }
        Ok(scope)
    }
    pub(crate) fn invalid(&self, operation: &'static str, error: &'static str) -> StorageError {
        StorageError::new(self.store.path(), operation, error)
    }
}
pub(super) fn validate_scope(
    uid: &str,
    home_id: &str,
    stored_group_id: &str,
) -> Result<(), &'static str> {
    validate_text(uid)?;
    validate_text(home_id)?;
    if stored_group_id != group_id(uid, home_id) {
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
    validate_scope(&cached.uid, &cached.home_id, &cached.group_id)?;
    validate_text(&cached.model)?;
    validate_text(&cached.instance_type)?;
    validate_key(&cached.device.key)?;
    let light = is_light_type(&cached.instance_type);
    if is_light_group(&cached.device.key, &cached.model) {
        return Err("Xiaomi light groups cannot be cached as physical devices");
    }
    if cached.device.key.source != "xiaomi"
        || !(is_outlet_type(&cached.instance_type) || light)
        || cached.device.functions.is_empty()
    {
        return Err("Stored Xiaomi device description is invalid");
    }
    let mut ids = std::collections::HashSet::new();
    let mut bindings = std::collections::HashSet::new();
    for function in &cached.device.functions {
        validate_text(&function.description.id)?;
        if !ids.insert(&function.description.id) || !function.description.valid_capabilities() {
            return Err("Stored Xiaomi function description is invalid");
        }
        let on_off = function
            .description
            .capabilities
            .contains(&Capability::OnOff);
        if function.power.is_some() != on_off {
            return Err("Stored Xiaomi power binding does not match its capability");
        }
        if let Some(power) = function.power
            && (power.siid == 0 || power.piid == 0 || !bindings.insert((power.siid, power.piid)))
        {
            return Err("Stored Xiaomi function description is invalid");
        }
        let (brightness, temperature, color, mode) = match &function.lighting {
            Some(lighting) => {
                if !lighting.valid() {
                    return Err("Stored Xiaomi light bindings are invalid");
                }
                (
                    lighting.brightness.as_ref(),
                    lighting.temperature.as_ref(),
                    lighting.color.as_ref(),
                    lighting.mode.as_ref(),
                )
            }
            None => (None, None, None, None),
        };
        if function
            .description
            .capabilities
            .contains(&Capability::Brightness)
            != brightness.is_some()
            || function.description.temperature_range() != temperature.map(|binding| binding.range)
            || function
                .description
                .capabilities
                .contains(&Capability::ColorRgb)
                != color.is_some()
        {
            return Err("Stored Xiaomi light bindings do not match their capabilities");
        }
        for binding in [
            brightness.map(|binding| binding.property),
            temperature.map(|binding| binding.property),
            color.map(|binding| binding.property),
            mode.map(|binding| binding.property),
        ]
        .into_iter()
        .flatten()
        {
            if binding.siid == 0
                || binding.piid == 0
                || !bindings.insert((binding.siid, binding.piid))
            {
                return Err("Stored Xiaomi light bindings overlap");
            }
        }
        if function.description.category
            != if light {
                Category::Light
            } else {
                Category::Outlet
            }
        {
            return Err("Stored Xiaomi function category is invalid");
        }
    }
    if light {
        if cached.device.functions.len() != 1
            || cached.device.functions[0].description.id != "main"
            || cached.device.refresh_trigger.is_some()
        {
            return Err("Stored Xiaomi light topology is invalid");
        }
    } else if cached.device.functions.iter().any(|function| {
        function.description.category != Category::Outlet || function.lighting.is_some()
    }) {
        return Err("Stored Xiaomi outlet description is invalid");
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
    let functions = serde_json::to_string(&cached.device.functions)
        .map_err(|_| rusqlite::Error::InvalidQuery)?;
    let category = match cached.device.functions[0].description.category {
        Category::Outlet => "outlet",
        Category::Light => "light",
    };
    let capabilities = serde_json::to_string(&cached.device.functions[0].description.capabilities)
        .map_err(|_| rusqlite::Error::InvalidQuery)?;
    let key = &cached.device.key;
    connection.execute(
        "INSERT INTO device_cache (
            uid, source, device_id, home_id, group_id, model, instance_type,
            category, capabilities, functions, refresh_siid, refresh_piid
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?11, ?12, ?8, ?9, ?10)
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
            cached.device.refresh_trigger.map(|binding| binding.piid),
            category,
            capabilities
        ],
    )?;
    Ok(())
}
pub(super) fn load_cached(
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
    let functions: Vec<ParsedFunction> =
        serde_json::from_str(&functions).map_err(|_| rusqlite::Error::InvalidQuery)?;
    let first = functions.first().ok_or(rusqlite::Error::InvalidQuery)?;
    let expected_category = match first.description.category {
        Category::Outlet => "outlet",
        Category::Light => "light",
    };
    let expected_capabilities = serde_json::to_string(&first.description.capabilities)
        .map_err(|_| rusqlite::Error::InvalidQuery)?;
    if category != expected_category || capabilities != expected_capabilities {
        return Err(rusqlite::Error::InvalidQuery);
    }
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
            functions,
            refresh_trigger,
        },
    };
    validate_cached(&cached).map_err(|_| rusqlite::Error::InvalidQuery)?;
    Ok(Some(cached))
}
