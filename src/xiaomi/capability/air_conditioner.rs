use super::{
    CapabilityError, NumericFormat, RecognitionDiagnostic,
    description::{
        Description, Property, Service, array, binding, control_access, integer_format, string,
    },
    model::{
        AirConditionerBindings, AirConditionerModeBinding, AirConditionerSpeedBinding,
        AirConditionerTemperatureBinding,
    },
};
use crate::device::air_conditioner::TemperatureGrid;

/// Recognize only an unambiguous standard air-conditioner and fan-control pair.
pub(super) fn parse_air_conditioner(
    instance: &Description<'_>,
    diagnostics: &mut Vec<RecognitionDiagnostic>,
) -> Result<AirConditionerBindings, CapabilityError> {
    let service = unique_service(instance, "air-conditioner", 0x780f)?;
    let fan_service = unique_service(instance, "fan-control", 0x7809)?;
    let power = required(service, "on", 0x06)?;
    boolean(power)?;
    let mode = required(service, "mode", 0x08)?;
    control_access(mode)?;
    let mode = AirConditionerModeBinding::new(
        binding(service, mode),
        numeric_format(mode)?,
        codes(mode, &["cool", "heat", "auto", "fan", "dry"])?,
    )?;
    let temperature = required(service, "target-temperature", 0x21)?;
    control_access(temperature)?;
    if string(temperature.value, "unit")? != "celsius" {
        return Err(CapabilityError::InvalidDescription(
            "air-conditioner temperature unit",
        ));
    }
    if temperature.value.get("value-list").is_some() {
        return Err(CapabilityError::InvalidDescription(
            "air-conditioner temperature shape",
        ));
    }
    let [min, max, step] = array(temperature.value, "value-range")? else {
        return Err(CapabilityError::InvalidDescription(
            "air-conditioner temperature range",
        ));
    };
    let grid = TemperatureGrid::new(integer(min)?, integer(max)?, integer(step)?)
        .map_err(|_| CapabilityError::InvalidDescription("air-conditioner temperature range"))?;
    let temperature = AirConditionerTemperatureBinding::new(
        binding(service, temperature),
        numeric_format(temperature)?,
        grid,
    )?;
    let speed = required(fan_service, "fan-level", 0x16)?;
    control_access(speed)?;
    let speed = AirConditionerSpeedBinding::new(
        binding(fan_service, speed),
        numeric_format(speed)?,
        codes(speed, &["auto", "low", "medium", "high"])?,
    )?;
    let swing_candidates = fan_service
        .properties
        .iter()
        .filter(|property| property.urn.standard("property", "vertical-swing", 0x18))
        .collect::<Vec<_>>();
    let swing = match swing_candidates.as_slice() {
        [] => None,
        [property] if boolean(property).is_ok() => Some(binding(fan_service, property)),
        _ => {
            diagnostics.push(RecognitionDiagnostic {
                target: format!("air-conditioner:{}", service.iid),
                reason: "vertical swing binding is invalid or ambiguous".into(),
            });
            None
        }
    };
    AirConditionerBindings::new(binding(service, power), mode, temperature, speed, swing)
}

fn unique_service<'a>(
    instance: &'a Description<'a>,
    name: &'static str,
    type_id: u32,
) -> Result<&'a Service<'a>, CapabilityError> {
    let matches = instance
        .services
        .iter()
        .filter(|service| service.urn.standard("service", name, type_id))
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [service] => Ok(service),
        [] => Err(CapabilityError::MissingFunction(name)),
        _ => Err(CapabilityError::InvalidDescription(
            "ambiguous air-conditioner service ownership",
        )),
    }
}
fn required<'a>(
    service: &'a Service<'a>,
    name: &'static str,
    type_id: u32,
) -> Result<&'a Property<'a>, CapabilityError> {
    let matches = service
        .properties
        .iter()
        .filter(|property| property.urn.standard("property", name, type_id))
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [property] => Ok(property),
        [] => Err(CapabilityError::MissingFunction(name)),
        _ => Err(CapabilityError::InvalidDescription(
            "ambiguous air-conditioner property",
        )),
    }
}
fn boolean(property: &Property<'_>) -> Result<(), CapabilityError> {
    control_access(property)?;
    if string(property.value, "format")? != "bool" {
        return Err(CapabilityError::InvalidDescription(
            "air-conditioner boolean format",
        ));
    }
    Ok(())
}
fn numeric_format(property: &Property<'_>) -> Result<NumericFormat, CapabilityError> {
    if string(property.value, "format")? == "float" {
        Ok(NumericFormat::Float)
    } else {
        integer_format(property)
    }
}
fn integer(value: &serde_json::Value) -> Result<i16, CapabilityError> {
    let raw = value.as_f64().ok_or(CapabilityError::InvalidDescription(
        "air-conditioner temperature range",
    ))?;
    if !raw.is_finite()
        || raw.fract() != 0.0
        || raw < f64::from(i16::MIN)
        || raw > f64::from(i16::MAX)
    {
        return Err(CapabilityError::InvalidDescription(
            "air-conditioner temperature range",
        ));
    }
    Ok(raw as i16)
}
fn codes<const N: usize>(
    property: &Property<'_>,
    names: &[&str; N],
) -> Result<[i64; N], CapabilityError> {
    let list = array(property.value, "value-list")?;
    if property.value.get("value-range").is_some() || list.len() != N {
        return Err(CapabilityError::InvalidDescription(
            "air-conditioner enum shape",
        ));
    }
    let mut codes = [0; N];
    for (index, name) in names.iter().enumerate() {
        let raw = list
            .iter()
            .find(|item| {
                item.get("description")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|description| description.trim().eq_ignore_ascii_case(name))
            })
            .and_then(|item| item.get("value"))
            .and_then(serde_json::Value::as_i64)
            .ok_or(CapabilityError::InvalidDescription(
                "air-conditioner enum semantics",
            ))?;
        codes[index] = raw;
    }
    Ok(codes)
}
