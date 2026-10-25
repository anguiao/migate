use super::{
    BatteryBinding, CapabilityError, MeasurementBinding, MeasurementUnit, NumericFormat,
    ParsedFunction, PropertyBinding,
    description::{Description, array, string},
};
use crate::device::MeasurementRange;

const TEMPERATURE: u32 = 0x0000_0020;
const RELATIVE_HUMIDITY: u32 = 0x0000_000C;
const SENSOR_SERVICE: u32 = 0x0000_7814;
const BATTERY_SERVICE: u32 = 0x0000_7805;
const BATTERY_LEVEL: u32 = 0x0000_0014;

pub(super) fn measurements(instance: &Description<'_>) -> Vec<ParsedFunction> {
    let mut functions = Vec::new();
    for (name, property_name, type_id, unit) in [
        (
            "temperature",
            "temperature",
            TEMPERATURE,
            MeasurementUnit::Celsius,
        ),
        (
            "humidity",
            "relative-humidity",
            RELATIVE_HUMIDITY,
            MeasurementUnit::Percentage,
        ),
        ("co2", "co2-density", 0x0000_004B, MeasurementUnit::Ppm),
        (
            "illuminance",
            "illumination",
            0x0000_004E,
            MeasurementUnit::Lux,
        ),
    ] {
        let candidate = instance
            .services
            .iter()
            .filter(|service| {
                name != "temperature"
                    || service.urn.standard(
                        "service",
                        "temperature-humidity-sensor",
                        SENSOR_SERVICE,
                    )
                    || service.urn.standard("service", "environment", 0x0000_780A)
            })
            .find_map(|service| {
                service
                    .properties
                    .iter()
                    .filter(|property| property.urn.standard("property", property_name, type_id))
                    .find_map(|property| {
                        measurement_binding(service.iid, property.iid, property.value, unit).ok()
                    })
            });
        if let Some(binding) = candidate {
            functions.push(match name {
                "temperature" => ParsedFunction::temperature(name, "Temperature", binding),
                "humidity" => ParsedFunction::humidity(name, "Humidity", binding),
                "co2" => ParsedFunction::co2(name, "CO₂", binding),
                "illuminance" => ParsedFunction::illuminance(name, "Illuminance", binding),
                _ => unreachable!(),
            });
        }
    }
    functions
}

fn measurement_binding(
    siid: u32,
    piid: u32,
    value: &serde_json::Value,
    unit: MeasurementUnit,
) -> Result<MeasurementBinding, CapabilityError> {
    let access = array(value, "access")?
        .iter()
        .map(|value| {
            value
                .as_str()
                .ok_or(CapabilityError::InvalidDescription("access"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    for permission in ["read", "notify"] {
        if !access.contains(&permission) {
            return Err(CapabilityError::MissingAccess(permission));
        }
    }
    let format = match string(value, "format")? {
        "float" => NumericFormat::Float,
        "uint8" => NumericFormat::Uint8,
        "uint16" => NumericFormat::Uint16,
        "uint32" => NumericFormat::Uint32,
        "int8" => NumericFormat::Int8,
        "int16" => NumericFormat::Int16,
        "int32" => NumericFormat::Int32,
        _ => return Err(CapabilityError::InvalidMeasurement("format")),
    };
    let expected_unit = match unit {
        MeasurementUnit::Celsius => "celsius",
        MeasurementUnit::Percentage => "percentage",
        MeasurementUnit::Ppm => "ppm",
        MeasurementUnit::Lux => "lux",
    };
    if string(value, "unit")? != expected_unit {
        return Err(CapabilityError::InvalidMeasurement("unit"));
    }
    let range = array(value, "value-range")?;
    let min = range
        .first()
        .and_then(serde_json::Value::as_f64)
        .ok_or(CapabilityError::InvalidMeasurement("range"))?;
    let max = range
        .get(1)
        .and_then(serde_json::Value::as_f64)
        .ok_or(CapabilityError::InvalidMeasurement("range"))?;
    let range = MeasurementRange::new(min, max)
        .map_err(|_| CapabilityError::InvalidMeasurement("range"))?;
    MeasurementBinding::new(PropertyBinding { siid, piid }, format, unit, range)
}

pub(super) fn battery(instance: &Description<'_>) -> (Option<BatteryBinding>, Option<String>) {
    let services = instance
        .services
        .iter()
        .filter(|service| service.urn.standard("service", "battery", BATTERY_SERVICE))
        .collect::<Vec<_>>();
    let service = match services.as_slice() {
        [] => return (None, Some("battery service is missing".into())),
        [service] => *service,
        _ => return (None, Some("multiple battery services".into())),
    };
    let properties = service
        .properties
        .iter()
        .filter(|property| {
            property
                .urn
                .standard("property", "battery-level", BATTERY_LEVEL)
        })
        .collect::<Vec<_>>();
    let property = match properties.as_slice() {
        [] => return (None, Some("battery-level property is missing".into())),
        [property] => *property,
        _ => return (None, Some("multiple battery-level properties".into())),
    };
    match measurement_binding(
        service.iid,
        property.iid,
        property.value,
        MeasurementUnit::Percentage,
    )
    .and_then(BatteryBinding::new)
    {
        Ok(binding) => (Some(binding), None),
        Err(error) => (None, Some(format!("invalid battery capability: {error}"))),
    }
}
