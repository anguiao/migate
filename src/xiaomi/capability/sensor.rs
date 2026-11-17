use super::{
    BatteryBinding, CapabilityError, MeasurementBinding, MeasurementUnit, ParsedFunction,
    PropertyBinding,
    description::{Description, Property, array, string},
};
use crate::device::MeasurementRange;

const TEMPERATURE: u32 = 0x0000_0020;
const RELATIVE_HUMIDITY: u32 = 0x0000_000C;
const SENSOR_SERVICE: u32 = 0x0000_7814;
const BATTERY_SERVICE: u32 = 0x0000_7805;
const BATTERY_LEVEL: u32 = 0x0000_0014;

pub(super) fn measurements(
    instance: &Description<'_>,
    diagnostics: &mut Vec<super::RecognitionDiagnostic>,
) -> Vec<ParsedFunction> {
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
        let candidates = instance
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
            .flat_map(|service| {
                service
                    .properties
                    .iter()
                    .filter(|property| property.urn.standard("property", property_name, type_id))
                    .map(move |property| (service.iid, property))
            });
        for (siid, property) in candidates {
            let binding = match measurement_binding(siid, property, unit) {
                Ok(binding) => binding,
                Err(error) => {
                    diagnostics.push(super::RecognitionDiagnostic {
                        target: format!("{name}:{siid}:{}", property.iid),
                        reason: error.to_string(),
                    });
                    continue;
                }
            };
            let property = binding.property();
            let id = format!("{name}:{}:{}", property.siid, property.piid);
            functions.push(match name {
                "temperature" => ParsedFunction::temperature(id, "Temperature", binding),
                "humidity" => ParsedFunction::humidity(id, "Humidity", binding),
                "co2" => ParsedFunction::co2(id, "CO₂", binding),
                "illuminance" => ParsedFunction::illuminance(id, "Illuminance", binding),
                _ => unreachable!(),
            });
        }
    }
    functions
}

fn measurement_binding(
    siid: u32,
    property: &Property<'_>,
    unit: MeasurementUnit,
) -> Result<MeasurementBinding, CapabilityError> {
    property.access(&["read", "notify"])?;
    let format = property.numeric_format()?;
    let value = property.value;
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
    MeasurementBinding::new(
        PropertyBinding {
            siid,
            piid: property.iid,
        },
        format,
        unit,
        range,
    )
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
    let property = match service.optional_property("battery-level", BATTERY_LEVEL) {
        Ok(Some(property)) => property,
        Ok(None) => return (None, Some("battery-level property is missing".into())),
        Err(_) => return (None, Some("multiple battery-level properties".into())),
    };
    match measurement_binding(service.iid, property, MeasurementUnit::Percentage)
        .and_then(BatteryBinding::new)
    {
        Ok(binding) => (Some(binding), None),
        Err(error) => (None, Some(format!("invalid battery capability: {error}"))),
    }
}
