use super::{StorageError, Store};
use crate::{
    device::{Capability, Category, DeviceDescription, DeviceKey},
    xiaomi::{
        capability::{ParsedOutlet, PropertyBinding, is_outlet_type},
        cloud::{OwnedHome, group_id},
    },
};
use rusqlite::{OptionalExtension as _, params};
use uuid::Uuid;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CachedDevice {
    pub uid: String,
    pub home_id: String,
    pub group_id: String,
    pub model: String,
    pub instance_type: String,
    pub outlet: ParsedOutlet,
}

impl CachedDevice {
    pub fn home(&self) -> OwnedHome {
        OwnedHome {
            uid: self.uid.clone(),
            home_id: self.home_id.clone(),
            group_id: self.group_id.clone(),
            dids: vec![self.outlet.description.key.id.clone()],
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceIdentity {
    pub unique_id: String,
    pub endpoint: u16,
}

#[derive(Clone)]
pub struct DeviceStore {
    store: Store,
}

impl DeviceStore {
    pub(super) fn new(store: Store) -> Self {
        Self { store }
    }

    pub fn load(&self, uid: &str, key: &DeviceKey) -> Result<Option<CachedDevice>, StorageError> {
        let operation = "load static Xiaomi device description";
        validate_text(uid)
            .and_then(|()| validate_key(key))
            .map_err(|error| self.invalid(operation, error))?;
        let data = self
            .store
            .inner
            .connection
            .query_row(
                "SELECT home_id, group_id, model, instance_type, category, capabilities, siid, piid
             FROM device_cache WHERE uid = ?1 AND source = ?2 AND device_id = ?3",
                params![uid, key.source, key.id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, u32>(6)?,
                        row.get::<_, u32>(7)?,
                    ))
                },
            )
            .optional()
            .map_err(|error| self.store.database_error(operation, error))?;
        let Some((home_id, group_id, model, instance_type, category, capabilities, siid, piid)) =
            data
        else {
            return Ok(None);
        };
        if category != "outlet" || capabilities != "on_off" {
            return Err(self.invalid(operation, "Stored device capabilities are invalid"));
        }
        let cached = CachedDevice {
            uid: uid.to_owned(),
            home_id,
            group_id,
            model,
            instance_type,
            outlet: ParsedOutlet {
                description: DeviceDescription {
                    key: key.clone(),
                    category: Category::Outlet,
                    capabilities: vec![Capability::OnOff],
                },
                main_power: PropertyBinding { siid, piid },
            },
        };
        validate_cached(&cached).map_err(|error| self.invalid(operation, error))?;
        Ok(Some(cached))
    }

    pub fn save(&self, cached: &CachedDevice) -> Result<(), StorageError> {
        let operation = "save static Xiaomi device description";
        validate_cached(cached).map_err(|error| self.invalid(operation, error))?;
        let transaction = self
            .store
            .inner
            .connection
            .unchecked_transaction()
            .map_err(|error| self.store.database_error(operation, error))?;
        let description = &cached.outlet.description;
        transaction.execute(
            "INSERT INTO device_cache (uid, source, device_id, home_id, group_id, model, instance_type, category, capabilities, siid, piid)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'outlet', 'on_off', ?8, ?9)
             ON CONFLICT (uid, source, device_id) DO UPDATE SET home_id = excluded.home_id, group_id = excluded.group_id,
                 model = excluded.model, instance_type = excluded.instance_type, category = excluded.category,
                 capabilities = excluded.capabilities, siid = excluded.siid, piid = excluded.piid",
            params![cached.uid, description.key.source, description.key.id, cached.home_id, cached.group_id,
                cached.model, cached.instance_type, cached.outlet.main_power.siid, cached.outlet.main_power.piid],
        ).map_err(|error| self.store.database_error(operation, error))?;
        transaction
            .commit()
            .map_err(|error| self.store.database_error(operation, error))
    }

    fn invalid(&self, operation: &'static str, error: &'static str) -> StorageError {
        StorageError::new(self.store.path(), operation, error)
    }
}

pub(super) fn load_or_create_identity(
    store: &Store,
    key: &DeviceKey,
) -> Result<DeviceIdentity, StorageError> {
    let operation = "load or create real device identity";
    validate_key(key).map_err(|error| StorageError::new(store.path(), operation, error))?;
    let transaction = store
        .inner
        .connection
        .unchecked_transaction()
        .map_err(|error| store.database_error(operation, error))?;
    transaction
        .execute(
            "INSERT INTO device_identity (source, device_id, unique_id, endpoint)
        VALUES (?1, ?2, ?3, 2) ON CONFLICT (source, device_id) DO NOTHING",
            params![key.source, key.id, Uuid::new_v4().simple().to_string()],
        )
        .map_err(|error| store.database_error(operation, error))?;
    let identity = transaction
        .query_row(
            "SELECT unique_id, endpoint FROM device_identity WHERE source = ?1 AND device_id = ?2",
            params![key.source, key.id],
            |row| {
                Ok(DeviceIdentity {
                    unique_id: row.get(0)?,
                    endpoint: row.get(1)?,
                })
            },
        )
        .map_err(|error| store.database_error(operation, error))?;
    let valid_uuid = Uuid::parse_str(&identity.unique_id)
        .is_ok_and(|uuid| uuid.simple().to_string() == identity.unique_id);
    if !valid_uuid || identity.endpoint != 2 {
        return Err(StorageError::new(
            store.path(),
            operation,
            "Stored real device identity is invalid",
        ));
    }
    transaction
        .commit()
        .map_err(|error| store.database_error(operation, error))?;
    Ok(identity)
}

fn validate_text(value: &str) -> Result<(), &'static str> {
    if value.is_empty() || value.chars().any(char::is_control) {
        Err("Stored device metadata is invalid")
    } else {
        Ok(())
    }
}

fn validate_key(key: &DeviceKey) -> Result<(), &'static str> {
    validate_text(&key.source)?;
    validate_text(&key.id)
}

fn validate_cached(cached: &CachedDevice) -> Result<(), &'static str> {
    for value in [
        &cached.uid,
        &cached.home_id,
        &cached.model,
        &cached.instance_type,
    ] {
        validate_text(value)?;
    }
    validate_key(&cached.outlet.description.key)?;
    if cached.outlet.description.key.source != "xiaomi"
        || !is_outlet_type(&cached.instance_type)
        || cached.group_id != group_id(&cached.uid, &cached.home_id)
        || cached.outlet.description.category != Category::Outlet
        || cached.outlet.description.capabilities != [Capability::OnOff]
        || cached.outlet.main_power.siid == 0
        || cached.outlet.main_power.piid == 0
    {
        return Err("Stored Xiaomi device description is invalid");
    }
    Ok(())
}
