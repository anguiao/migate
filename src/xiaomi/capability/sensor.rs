use super::{
    BatteryBinding, CapabilityError, MeasurementBinding, MeasurementUnit, NumericFormat,
    ParsedDevice, ParsedFunction, PropertyBinding,
    description::{Description, Service, Urn, array, string},
};
use crate::device::{DeviceKey, MeasurementRange};

const TEMPERATURE: u32 = 0x0000_0020;
const RELATIVE_HUMIDITY: u32 = 0x0000_000C;
const SENSOR_SERVICE: u32 = 0x0000_7814;
const BATTERY_SERVICE: u32 = 0x0000_7805;
const BATTERY_LEVEL: u32 = 0x0000_0014;

pub(super) fn parse_sensor(
    key: DeviceKey,
    instance: &Description<'_>,
) -> Result<ParsedDevice, CapabilityError> {
    if !instance
        .urn
        .standard("device", "temperature-humidity-sensor", 0x0000_A00A)
    {
        return Err(CapabilityError::UnsupportedDevice);
    }
    let services = instance
        .services
        .iter()
        .filter(|service| {
            service
                .urn
                .standard("service", "temperature-humidity-sensor", SENSOR_SERVICE)
        })
        .collect::<Vec<_>>();
    let service = match services.as_slice() {
        [service] => *service,
        [] => return Err(CapabilityError::MissingMeasurement("temperature")),
        _ => return Err(CapabilityError::InvalidMeasurement("service")),
    };
    let temperature = measurement_property(service, "temperature", "temperature", TEMPERATURE)?;
    let humidity =
        measurement_property(service, "relative-humidity", "humidity", RELATIVE_HUMIDITY)?;
    let temperature = measurement_binding(
        temperature.0,
        temperature.1,
        temperature.2,
        MeasurementUnit::Celsius,
    )?;
    let humidity = measurement_binding(
        humidity.0,
        humidity.1,
        humidity.2,
        MeasurementUnit::Percentage,
    )?;
    let (battery, battery_issue) = battery(instance);
    ParsedDevice::new_with_battery_diagnostic(
        key,
        vec![
            ParsedFunction::temperature("temperature", "Temperature", temperature),
            ParsedFunction::humidity("humidity", "Humidity", humidity),
        ],
        None,
        battery,
        battery_issue,
    )
}

pub(crate) fn is_type(value: &str) -> bool {
    Urn::parse(value)
        .is_ok_and(|urn| urn.standard("device", "temperature-humidity-sensor", 0x0000_A00A))
}

fn measurement_property<'a>(
    service: &Service<'a>,
    urn_name: &str,
    name: &'static str,
    type_id: u32,
) -> Result<(u32, u32, &'a serde_json::Value), CapabilityError> {
    let properties = service
        .properties
        .iter()
        .filter(|property| property.urn.standard("property", urn_name, type_id))
        .map(|property| (service.iid, property.iid, property.value))
        .collect::<Vec<_>>();
    match properties.as_slice() {
        [] => Err(CapabilityError::MissingMeasurement(name)),
        [(siid, piid, value)] => Ok((*siid, *piid, *value)),
        _ => Err(CapabilityError::InvalidMeasurement(name)),
    }
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
    };
    if string(value, "unit")? != expected_unit {
        return Err(CapabilityError::InvalidMeasurement("unit"));
    }
    let range = array(value, "value-range")?
        .iter()
        .map(|value| {
            value
                .as_f64()
                .filter(|value| value.is_finite())
                .ok_or(CapabilityError::InvalidMeasurement("range"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let [min, max, step] = range.as_slice() else {
        return Err(CapabilityError::InvalidMeasurement("range"));
    };
    let range = MeasurementRange::new(*min, *max)
        .map_err(|_| CapabilityError::InvalidMeasurement("range"))?;
    let binding =
        MeasurementBinding::new(PropertyBinding { siid, piid }, format, unit, range, *step)?;
    Ok(binding)
}

fn battery(instance: &Description<'_>) -> (Option<BatteryBinding>, Option<String>) {
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
    match battery_binding(service.iid, property.iid, property.value) {
        Ok(binding) => (Some(binding), None),
        Err(error) => (None, Some(format!("invalid battery capability: {error}"))),
    }
}

fn battery_binding(
    siid: u32,
    piid: u32,
    value: &serde_json::Value,
) -> Result<BatteryBinding, CapabilityError> {
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
        _ => return Err(CapabilityError::InvalidMeasurement("battery")),
    };
    if string(value, "unit")? != "percentage" {
        return Err(CapabilityError::InvalidMeasurement("unit"));
    }
    let range = array(value, "value-range")?
        .iter()
        .map(|value| {
            value
                .as_f64()
                .filter(|value| value.is_finite())
                .ok_or(CapabilityError::InvalidMeasurement("battery"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let [min, max, step] = range.as_slice() else {
        return Err(CapabilityError::InvalidMeasurement("battery"));
    };
    let range = MeasurementRange::new(*min, *max)
        .map_err(|_| CapabilityError::InvalidMeasurement("battery"))?;
    BatteryBinding::new(PropertyBinding { siid, piid }, format, range, *step)
}
