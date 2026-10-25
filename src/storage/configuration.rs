//! Private schema 8 codec and transactions for the singleton configuration.
//!
//! The stored JSON is a storage-private encoding: every layer is tagged and
//! rejects unknown fields and missing required values, and decoded values are
//! re-created through the domain constructors. Consumers only see valid
//! configuration values.

use super::{StorageError, device::DeviceStore};
use crate::xiaomi::capability::NativeDescription;
use crate::{
    configuration::{
        CommittedConfiguration, CommittedDevice, CommittedFunction, GroupRelation, Revocation,
        Scope,
    },
    device::{DeviceKey, MeasurementRange, NumericRange},
    xiaomi::capability::{
        BatteryBinding, BrightnessBinding, ColorBinding, LightBindings, MeasurementBinding,
        MeasurementUnit, ModeBinding, NumericFormat, ParsedDevice, ParsedFunction, PropertyBinding,
        TemperatureBinding,
    },
};
use rusqlite::params;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeSet;

impl DeviceStore {
    /// Load the complete committed configuration. The singleton record is
    /// validated through the domain constructors before it is returned.
    pub(crate) fn load_configuration(&self) -> Result<CommittedConfiguration, StorageError> {
        let operation = "load bridge configuration";
        let json = self
            .store
            .connection()
            .query_row(
                "SELECT json FROM bridge_configuration WHERE id=1",
                [],
                |row| row.get::<_, String>(0),
            )
            .map_err(|error| self.store.database_error(operation, error))?;
        decode(&json).map_err(|error| self.invalid(operation, error))
    }

    /// Persist the complete next configuration and clear the revocations
    /// confirmed again in the same transaction. The value is already a valid
    /// planned configuration; only the record write and the tombstone clears
    /// happen here. A failure leaves the previous record and every tombstone
    /// untouched.
    pub(crate) fn commit_configuration(
        &self,
        configuration: &CommittedConfiguration,
        clear_revocations: &[DeviceKey],
    ) -> Result<(), StorageError> {
        let operation = "commit bridge configuration";
        let json = encode(configuration).map_err(|error| self.invalid(operation, error))?;
        let transaction = self
            .store
            .connection()
            .unchecked_transaction()
            .map_err(|error| self.store.database_error(operation, error))?;
        let changed = transaction
            .execute(
                "UPDATE bridge_configuration SET json=?1 WHERE id=1",
                [&json],
            )
            .map_err(|error| self.store.database_error(operation, error))?;
        if changed != 1 {
            return Err(self.invalid(operation, "Committed configuration record is missing"));
        }
        for key in clear_revocations {
            transaction
                .execute(
                    "DELETE FROM publication_revocation WHERE source=?1 AND device_id=?2",
                    params![key.source, key.id],
                )
                .map_err(|error| self.store.database_error(operation, error))?;
        }
        transaction
            .commit()
            .map_err(|error| self.store.database_error(operation, error))
    }

    /// Write focused revocation tombstones before the main configuration commit.
    pub(crate) fn record_revocations(
        &self,
        revocations: &[Revocation],
    ) -> Result<(), StorageError> {
        let operation = "record revocation";
        for revocation in revocations {
            validate_key(&revocation.key)
                .and_then(|()| validate_text(&revocation.reason))
                .map_err(|error| self.invalid(operation, error))?;
        }
        let transaction = self
            .store
            .connection()
            .unchecked_transaction()
            .map_err(|error| self.store.database_error(operation, error))?;
        for revocation in revocations {
            transaction
                .execute(
                    "INSERT OR REPLACE INTO publication_revocation VALUES (?1,?2,?3)",
                    params![revocation.key.source, revocation.key.id, revocation.reason],
                )
                .map_err(|error| self.store.database_error(operation, error))?;
        }
        transaction
            .commit()
            .map_err(|error| self.store.database_error(operation, error))
    }

    /// Read every durable revocation tombstone. Corrupted or duplicated
    /// records fail the load instead of being treated as valid authority.
    pub(crate) fn load_revocations(&self) -> Result<Vec<Revocation>, StorageError> {
        let operation = "load revocations";
        let rows = self
            .store
            .connection()
            .prepare(
                "SELECT source,device_id,reason FROM publication_revocation ORDER BY source,device_id",
            )
            .and_then(|mut statement| {
                statement
                    .query_map([], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .map_err(|error| self.store.database_error(operation, error))?;
        let mut revocations = Vec::new();
        for (source, device_id, reason) in rows {
            let key = DeviceKey::new(source, device_id);
            validate_key(&key)
                .and_then(|()| validate_text(&reason))
                .map_err(|error| self.invalid(operation, error))?;
            revocations.push(Revocation { key, reason });
        }
        let mut unique = BTreeSet::new();
        if !revocations
            .iter()
            .all(|revocation| unique.insert(revocation.key.clone()))
        {
            return Err(self.invalid(operation, "Stored revocation is duplicated"));
        }
        Ok(revocations)
    }
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

fn decode(json: &str) -> Result<CommittedConfiguration, &'static str> {
    let wire: WireConfiguration =
        serde_json::from_str(json).map_err(|_| "Committed configuration is corrupted")?;
    wire.into_configuration()
        .map_err(|_| "Committed configuration is corrupted")
}

fn encode(configuration: &CommittedConfiguration) -> Result<String, &'static str> {
    let wire = WireConfiguration::from_configuration(configuration);
    serde_json::to_string(&wire).map_err(|_| "Committed configuration cannot be encoded")
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireConfiguration {
    #[serde(default, deserialize_with = "present_option")]
    scope: Option<Option<WireScope>>,
    root_version: u32,
    devices: Vec<WireIdentity>,
    groups: Vec<WireGroup>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireScope {
    uid: String,
    home_id: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireIdentity {
    source: String,
    device_id: String,
    unique_id: String,
    endpoint: u16,
    label: String,
    configuration_version: u32,
    functions: Vec<WireFunctionIdentity>,
    #[serde(default, deserialize_with = "present_option")]
    native: Option<Option<WireNative>>,
}

/// Distinguish a missing critical field from an explicit null: missing stays
/// `None`, an explicit null becomes `Some(None)`.
fn present_option<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireFunctionIdentity {
    id: String,
    endpoint: u16,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireGroup {
    group_did: String,
    record_did: String,
    model: String,
    instance_type: Option<String>,
    is_physical: bool,
    active: bool,
    error: Option<String>,
    members: Vec<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireNative {
    model: String,
    instance_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    refresh_trigger: Option<WireBinding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    battery: Option<WireMeasurement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    battery_issue: Option<String>,
    functions: Vec<WireFunction>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum WireFunction {
    Outlet {
        id: String,
        label: String,
        power: WireBinding,
    },
    Light {
        id: String,
        label: String,
        power: WireBinding,
        lighting: WireLighting,
    },
    Temperature {
        id: String,
        label: String,
        measurement: WireMeasurement,
    },
    Humidity {
        id: String,
        label: String,
        measurement: WireMeasurement,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireMeasurement {
    property: WireBinding,
    format: NumericFormat,
    unit: MeasurementUnit,
    min: f64,
    max: f64,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "profile", rename_all = "snake_case", deny_unknown_fields)]
enum WireLighting {
    OnOff,
    Dimmable {
        brightness: WireNumeric,
    },
    ColorTemperature {
        brightness: WireNumeric,
        temperature: WireNumeric,
    },
    ExtendedColor {
        brightness: WireNumeric,
        temperature: WireNumeric,
        color: WireFormat,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mode: Option<WireMode>,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireNumeric {
    property: WireBinding,
    format: NumericFormat,
    min: u32,
    max: u32,
    step: u32,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireFormat {
    property: WireBinding,
    format: NumericFormat,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireMode {
    property: WireBinding,
    format: NumericFormat,
    temperature_value: u32,
    rgb_value: u32,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireBinding {
    siid: u32,
    piid: u32,
}
impl From<PropertyBinding> for WireBinding {
    fn from(binding: PropertyBinding) -> Self {
        Self {
            siid: binding.siid,
            piid: binding.piid,
        }
    }
}
impl From<WireBinding> for PropertyBinding {
    fn from(binding: WireBinding) -> Self {
        Self {
            siid: binding.siid,
            piid: binding.piid,
        }
    }
}

impl WireConfiguration {
    fn from_configuration(configuration: &CommittedConfiguration) -> Self {
        Self {
            scope: Some(configuration.scope.as_ref().map(|scope| WireScope {
                uid: scope.uid.clone(),
                home_id: scope.home_id.clone(),
            })),
            root_version: configuration.root_version,
            devices: configuration
                .devices
                .iter()
                .map(WireIdentity::from_committed)
                .collect(),
            groups: configuration
                .groups
                .iter()
                .map(WireGroup::from_relation)
                .collect(),
        }
    }

    fn into_configuration(self) -> Result<CommittedConfiguration, &'static str> {
        let scope = self
            .scope
            .ok_or("Stored home scope field is missing")?
            .map(|scope| Scope {
                uid: scope.uid,
                home_id: scope.home_id,
            });
        let configuration = CommittedConfiguration {
            scope,
            root_version: self.root_version,
            devices: self
                .devices
                .into_iter()
                .map(WireIdentity::into_committed)
                .collect::<Result<Vec<_>, _>>()?,
            groups: self
                .groups
                .into_iter()
                .map(WireGroup::into_relation)
                .collect(),
        };
        configuration.validate()?;
        Ok(configuration)
    }
}

impl WireIdentity {
    fn from_committed(device: &CommittedDevice) -> Self {
        Self {
            source: device.key.source.clone(),
            device_id: device.key.id.clone(),
            unique_id: device.unique_id.clone(),
            endpoint: device.endpoint,
            label: device.label.clone(),
            configuration_version: device.configuration_version,
            functions: device
                .functions
                .iter()
                .map(|function| WireFunctionIdentity {
                    id: function.id.clone(),
                    endpoint: function.endpoint,
                })
                .collect(),
            native: Some(device.native.as_ref().map(WireNative::from_native)),
        }
    }

    fn into_committed(self) -> Result<CommittedDevice, &'static str> {
        let key = DeviceKey::new(self.source, self.device_id);
        let native = self
            .native
            .ok_or("Stored native facts field is missing")?
            .map(|native| native.into_native(&key))
            .transpose()?;
        Ok(CommittedDevice {
            key,
            unique_id: self.unique_id,
            endpoint: self.endpoint,
            label: self.label,
            configuration_version: self.configuration_version,
            functions: self
                .functions
                .into_iter()
                .map(|function| CommittedFunction {
                    id: function.id,
                    endpoint: function.endpoint,
                })
                .collect(),
            native,
        })
    }
}

impl WireGroup {
    fn from_relation(relation: &GroupRelation) -> Self {
        Self {
            group_did: relation.group_did.clone(),
            record_did: relation.record_did.clone(),
            model: relation.model.clone(),
            instance_type: relation.instance_type.clone(),
            is_physical: relation.is_physical,
            active: relation.active,
            error: relation.error.clone(),
            members: relation.members.clone(),
        }
    }

    fn into_relation(self) -> GroupRelation {
        GroupRelation {
            group_did: self.group_did,
            record_did: self.record_did,
            model: self.model,
            instance_type: self.instance_type,
            is_physical: self.is_physical,
            active: self.active,
            error: self.error,
            members: self.members,
        }
    }
}

impl WireNative {
    fn from_native(native: &NativeDescription) -> Self {
        Self {
            model: native.model().to_owned(),
            instance_type: native.instance_type().to_owned(),
            refresh_trigger: native.device().refresh_trigger().map(WireBinding::from),
            battery: native.device().battery().map(WireMeasurement::from_battery),
            battery_issue: native.device().battery_issue().map(str::to_owned),
            functions: native
                .device()
                .functions()
                .iter()
                .map(WireFunction::from_parsed)
                .collect(),
        }
    }

    fn into_native(self, key: &DeviceKey) -> Result<NativeDescription, &'static str> {
        let refresh_trigger = self.refresh_trigger.map(PropertyBinding::from);
        let battery = self
            .battery
            .map(WireMeasurement::into_battery)
            .transpose()?;
        let functions = self
            .functions
            .into_iter()
            .map(WireFunction::into_parsed)
            .collect::<Result<Vec<_>, _>>()?;
        let battery_issue = self.battery_issue.or_else(|| {
            (battery.is_none()
                && functions
                    .iter()
                    .any(|function| function.measurement().is_some()))
            .then(|| "battery capability not declared".into())
        });
        let device = ParsedDevice::new_with_battery_diagnostic(
            key.clone(),
            functions,
            refresh_trigger,
            battery,
            battery_issue,
        )
        .map_err(|_| "Stored native description is invalid")?;
        NativeDescription::new(self.model, self.instance_type, device)
            .map_err(|_| "Stored native description is invalid")
    }
}

impl WireFunction {
    fn from_parsed(function: &ParsedFunction) -> Self {
        if let Some(measurement) = function.measurement() {
            let wire = WireMeasurement::from_binding(measurement);
            return match function.kind() {
                crate::device::FunctionKind::Temperature(_) => Self::Temperature {
                    id: function.id().to_owned(),
                    label: function.label().to_owned(),
                    measurement: wire,
                },
                crate::device::FunctionKind::Humidity(_) => Self::Humidity {
                    id: function.id().to_owned(),
                    label: function.label().to_owned(),
                    measurement: wire,
                },
                _ => unreachable!("measurement functions have measurement kinds"),
            };
        }
        let Some(lighting) = function.lighting() else {
            return Self::Outlet {
                id: function.id().to_owned(),
                label: function.label().to_owned(),
                power: function
                    .power()
                    .expect("outlet functions have power")
                    .into(),
            };
        };
        let lighting = match lighting {
            LightBindings::OnOff => WireLighting::OnOff,
            LightBindings::Dimmable { brightness } => WireLighting::Dimmable {
                brightness: WireNumeric::from_brightness(brightness),
            },
            LightBindings::ColorTemperature {
                brightness,
                temperature,
            } => WireLighting::ColorTemperature {
                brightness: WireNumeric::from_brightness(brightness),
                temperature: WireNumeric::from_temperature(temperature),
            },
            LightBindings::ExtendedColor {
                brightness,
                temperature,
                color,
                mode,
            } => WireLighting::ExtendedColor {
                brightness: WireNumeric::from_brightness(brightness),
                temperature: WireNumeric::from_temperature(temperature),
                color: WireFormat {
                    property: color.property().into(),
                    format: color.format(),
                },
                mode: mode.as_ref().map(|mode| WireMode {
                    property: mode.property().into(),
                    format: mode.format(),
                    temperature_value: mode.temperature_value(),
                    rgb_value: mode.rgb_value(),
                }),
            },
        };
        Self::Light {
            id: function.id().to_owned(),
            label: function.label().to_owned(),
            power: function.power().expect("light functions have power").into(),
            lighting,
        }
    }

    fn into_parsed(self) -> Result<ParsedFunction, &'static str> {
        match self {
            Self::Outlet { id, label, power } => {
                Ok(ParsedFunction::outlet(id, label, power.into()))
            }
            Self::Temperature {
                id,
                label,
                measurement,
            } => Ok(ParsedFunction::temperature(
                id,
                label,
                measurement.into_binding()?,
            )),
            Self::Humidity {
                id,
                label,
                measurement,
            } => Ok(ParsedFunction::humidity(
                id,
                label,
                measurement.into_binding()?,
            )),
            Self::Light {
                id,
                label,
                power,
                lighting,
            } => {
                let lighting = match lighting {
                    WireLighting::OnOff => LightBindings::OnOff,
                    WireLighting::Dimmable { brightness } => LightBindings::Dimmable {
                        brightness: brightness.into_brightness()?,
                    },
                    WireLighting::ColorTemperature {
                        brightness,
                        temperature,
                    } => LightBindings::ColorTemperature {
                        brightness: brightness.into_brightness()?,
                        temperature: temperature.into_temperature()?,
                    },
                    WireLighting::ExtendedColor {
                        brightness,
                        temperature,
                        color,
                        mode,
                    } => LightBindings::ExtendedColor {
                        brightness: brightness.into_brightness()?,
                        temperature: temperature.into_temperature()?,
                        color: ColorBinding::new(color.property.into(), color.format)
                            .map_err(|_| "Stored light binding is invalid")?,
                        mode: mode
                            .map(|mode| {
                                ModeBinding::new(
                                    mode.property.into(),
                                    mode.format,
                                    mode.temperature_value,
                                    mode.rgb_value,
                                )
                                .map_err(|_| "Stored light binding is invalid")
                            })
                            .transpose()?,
                    },
                };
                Ok(ParsedFunction::light(id, label, power.into(), lighting))
            }
        }
    }
}

impl WireMeasurement {
    fn from_binding(binding: &MeasurementBinding) -> Self {
        Self {
            property: binding.property().into(),
            format: binding.format(),
            unit: binding.unit(),
            min: binding.range().min(),
            max: binding.range().max(),
        }
    }

    fn from_battery(binding: &BatteryBinding) -> Self {
        Self {
            property: binding.property().into(),
            format: binding.format(),
            unit: MeasurementUnit::Percentage,
            min: binding.range().min(),
            max: binding.range().max(),
        }
    }

    fn into_binding(self) -> Result<MeasurementBinding, &'static str> {
        MeasurementBinding::new(
            self.property.into(),
            self.format,
            self.unit,
            MeasurementRange::new(self.min, self.max)
                .map_err(|_| "Stored measurement binding is invalid")?,
        )
        .map_err(|_| "Stored measurement binding is invalid")
    }

    fn into_battery(self) -> Result<BatteryBinding, &'static str> {
        self.into_binding().and_then(|binding| {
            BatteryBinding::new(binding).map_err(|_| "Stored battery binding is invalid")
        })
    }
}

impl WireNumeric {
    fn from_brightness(binding: &BrightnessBinding) -> Self {
        Self {
            property: binding.property().into(),
            format: binding.format(),
            min: binding.range().min(),
            max: binding.range().max(),
            step: binding.range().step(),
        }
    }

    fn from_temperature(binding: &TemperatureBinding) -> Self {
        Self {
            property: binding.property().into(),
            format: binding.format(),
            min: binding.range().min(),
            max: binding.range().max(),
            step: binding.range().step(),
        }
    }

    fn into_brightness(self) -> Result<BrightnessBinding, &'static str> {
        BrightnessBinding::new(
            self.property.into(),
            self.format,
            NumericRange::new(self.min, self.max, self.step)
                .map_err(|_| "Stored light binding is invalid")?,
        )
        .map_err(|_| "Stored light binding is invalid")
    }

    fn into_temperature(self) -> Result<TemperatureBinding, &'static str> {
        TemperatureBinding::new(
            self.property.into(),
            self.format,
            NumericRange::new(self.min, self.max, self.step)
                .map_err(|_| "Stored light binding is invalid")?,
        )
        .map_err(|_| "Stored light binding is invalid")
    }
}
