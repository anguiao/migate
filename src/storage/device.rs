use super::{StorageError, Store};
use crate::{
    device::{Capability, DeviceKey, NumericRange, PhysicalDeviceDescription},
    xiaomi::{
        capability::{
            BrightnessBinding, CapabilityError, ColorBinding, LightBindings, ModeBinding,
            NumericFormat, ParsedDevice, ParsedFunction, PropertyBinding, TemperatureBinding,
            validate_model_topology,
        },
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
        self.device.physical_description(label)
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
fn validate_scope(uid: &str, home_id: &str, stored_group_id: &str) -> Result<(), &'static str> {
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
    validate_key(cached.device.key())?;
    if cached.device.key().source != "xiaomi" {
        return Err("Stored Xiaomi device description is invalid");
    }
    validate_model_topology(&cached.device, &cached.model, &cached.instance_type)
}
pub(super) fn save_cached(
    connection: &Connection,
    cached: &CachedPhysicalDevice,
) -> rusqlite::Result<()> {
    let device = WireDevice::from_parsed(&cached.device);
    let functions = serde_json::to_string(&device).map_err(|_| rusqlite::Error::InvalidQuery)?;
    let first = &cached.device.functions()[0];
    let category = if first.kind().is_light() {
        "light"
    } else {
        "outlet"
    };
    let capabilities = capability_json(&first.description().capabilities())
        .map_err(|_| rusqlite::Error::InvalidQuery)?;
    let key = cached.device.key();
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
            cached.device.refresh_trigger().map(|binding| binding.siid),
            cached.device.refresh_trigger().map(|binding| binding.piid),
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
    let refresh_trigger = match (refresh_siid, refresh_piid) {
        (None, None) => None,
        (Some(siid), Some(piid)) => Some(PropertyBinding { siid, piid }),
        _ => return Err(rusqlite::Error::InvalidQuery),
    };
    let wire: WireDevice =
        serde_json::from_str(&functions).map_err(|_| rusqlite::Error::InvalidQuery)?;
    let device = wire
        .into_parsed(key.clone(), refresh_trigger)
        .map_err(|_| rusqlite::Error::InvalidQuery)?;
    let first = &device.functions()[0];
    let expected_category = if first.kind().is_light() {
        "light"
    } else {
        "outlet"
    };
    let expected_capabilities = capability_json(&first.description().capabilities())
        .map_err(|_| rusqlite::Error::InvalidQuery)?;
    if category != expected_category || capabilities != expected_capabilities {
        return Err(rusqlite::Error::InvalidQuery);
    }
    let cached = CachedPhysicalDevice {
        uid: uid.into(),
        home_id: home_id.into(),
        group_id,
        model,
        instance_type,
        device,
    };
    validate_cached(&cached).map_err(|_| rusqlite::Error::InvalidQuery)?;
    Ok(Some(cached))
}

/// Private persistence encoding of the native model. Values are re-created
/// through the domain constructors on load, so the stored form cannot bypass
/// the valid-model constraints.
#[derive(Serialize, Deserialize)]
struct WireDevice {
    functions: Vec<WireFunction>,
}
#[derive(Serialize, Deserialize)]
struct WireFunction {
    id: String,
    label: String,
    power: PropertyBinding,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    lighting: Option<WireLight>,
}
#[derive(Serialize, Deserialize)]
struct WireLight {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    brightness: Option<WireNumeric>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    temperature: Option<WireNumeric>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    color: Option<WireFormat>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mode: Option<WireMode>,
}
#[derive(Serialize, Deserialize)]
struct WireNumeric {
    property: PropertyBinding,
    format: NumericFormat,
    min: u32,
    max: u32,
    step: u32,
}
#[derive(Serialize, Deserialize)]
struct WireFormat {
    property: PropertyBinding,
    format: NumericFormat,
}
#[derive(Serialize, Deserialize)]
struct WireMode {
    property: PropertyBinding,
    format: NumericFormat,
    temperature_value: u32,
    rgb_value: u32,
}
impl WireDevice {
    fn from_parsed(device: &ParsedDevice) -> Self {
        Self {
            functions: device
                .functions()
                .iter()
                .map(WireFunction::from_parsed)
                .collect(),
        }
    }
    fn into_parsed(
        self,
        key: DeviceKey,
        refresh_trigger: Option<PropertyBinding>,
    ) -> Result<ParsedDevice, CapabilityError> {
        let functions = self
            .functions
            .into_iter()
            .map(WireFunction::into_parsed)
            .collect::<Result<Vec<_>, _>>()?;
        ParsedDevice::new(key, functions, refresh_trigger)
    }
}
impl WireFunction {
    fn from_parsed(function: &ParsedFunction) -> Self {
        let lighting = function.lighting().map(|lighting| WireLight {
            brightness: lighting.brightness().map(|binding| WireNumeric {
                property: binding.property(),
                format: binding.format(),
                min: binding.range().min(),
                max: binding.range().max(),
                step: binding.range().step(),
            }),
            temperature: lighting.temperature().map(|binding| WireNumeric {
                property: binding.property(),
                format: binding.format(),
                min: binding.range().min(),
                max: binding.range().max(),
                step: binding.range().step(),
            }),
            color: lighting.color().map(|binding| WireFormat {
                property: binding.property(),
                format: binding.format(),
            }),
            mode: lighting.mode().map(|binding| WireMode {
                property: binding.property(),
                format: binding.format(),
                temperature_value: binding.temperature_value(),
                rgb_value: binding.rgb_value(),
            }),
        });
        Self {
            id: function.id().to_owned(),
            label: function.label().to_owned(),
            power: function.power(),
            lighting,
        }
    }
    fn into_parsed(self) -> Result<ParsedFunction, CapabilityError> {
        let Some(lighting) = self.lighting else {
            return Ok(ParsedFunction::outlet(self.id, self.label, self.power));
        };
        let brightness = lighting
            .brightness
            .map(|binding| {
                BrightnessBinding::new(
                    binding.property,
                    binding.format,
                    numeric_range(binding.min, binding.max, binding.step)?,
                )
            })
            .transpose()?;
        let temperature = lighting
            .temperature
            .map(|binding| {
                TemperatureBinding::new(
                    binding.property,
                    binding.format,
                    numeric_range(binding.min, binding.max, binding.step)?,
                )
            })
            .transpose()?;
        let color = lighting
            .color
            .map(|binding| ColorBinding::new(binding.property, binding.format))
            .transpose()?;
        let mode = lighting
            .mode
            .map(|binding| {
                ModeBinding::new(
                    binding.property,
                    binding.format,
                    binding.temperature_value,
                    binding.rgb_value,
                )
            })
            .transpose()?;
        Ok(ParsedFunction::light(
            self.id,
            self.label,
            self.power,
            LightBindings::new(brightness, temperature, color, mode)?,
        ))
    }
}
fn numeric_range(min: u32, max: u32, step: u32) -> Result<NumericRange, CapabilityError> {
    NumericRange::new(min, max, step).map_err(|_| CapabilityError::InvalidLight("range or format"))
}

/// Derived diagnostics encoding of the temporary legacy capability column.
#[derive(Serialize, Deserialize)]
#[serde(tag = "capability", rename_all = "snake_case")]
enum WireCapability {
    OnOff,
    Brightness,
    ColorTemperature { min: u32, max: u32 },
    ColorRgb,
}
fn capability_json(capabilities: &[Capability]) -> serde_json::Result<String> {
    let wire = capabilities
        .iter()
        .map(|capability| match capability {
            Capability::OnOff => WireCapability::OnOff,
            Capability::Brightness => WireCapability::Brightness,
            Capability::ColorTemperature(interval) => WireCapability::ColorTemperature {
                min: interval.min(),
                max: interval.max(),
            },
            Capability::ColorRgb => WireCapability::ColorRgb,
        })
        .collect::<Vec<_>>();
    serde_json::to_string(&wire)
}
