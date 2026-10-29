use super::{
    CapabilityError, FanBindings, FanModeBinding, FanSpeedBinding, FanSpeedEncoding, NumericFormat,
    ParsedDevice, ParsedFunction, PropertyBinding,
    description::{Description, Property, Service, Urn, array, string},
};
use crate::device::DeviceKey;

pub(crate) fn is_fan_type(value: &str) -> bool {
    Urn::parse(value).is_ok_and(|urn| urn.standard("device", "fan", 0x0000a005))
}

pub(crate) fn is_dmaker_fan_type(value: &str) -> bool {
    Urn::parse(value).is_ok_and(|urn| {
        urn.standard("device", "fan", 0x0000a005) && urn.vendor.starts_with("dmaker-")
    })
}

pub(super) fn parse_fan(
    key: DeviceKey,
    model: &str,
    instance: &Description<'_>,
) -> Result<ParsedDevice, CapabilityError> {
    let services = instance
        .services
        .iter()
        .filter(|s| s.urn.standard("service", "fan", 0x7808))
        .collect::<Vec<_>>();
    let [service] = services.as_slice() else {
        return Err(CapabilityError::MissingFunction("fan"));
    };
    let fan = parse_fan_service(model, instance, service)?;
    ParsedDevice::new(
        key,
        vec![ParsedFunction::fan_control("main", "风扇", fan)],
        None,
    )
}

/// Parse one standard fan service; the caller determines the function identity.
pub(super) fn parse_fan_service(
    model: &str,
    instance: &Description<'_>,
    service: &Service<'_>,
) -> Result<FanBindings, CapabilityError> {
    let is_dmaker = model.starts_with("dmaker.");
    let is_dmaker_instance = instance.urn.vendor.starts_with("dmaker-");
    let power = required(service, |p| p.urn.standard("property", "on", 6), "on")?;
    boolean(power)?;
    let swings = service
        .properties
        .iter()
        .filter(|p| p.urn.standard("property", "horizontal-swing", 0x17))
        .collect::<Vec<_>>();
    let swing = match swings.as_slice() {
        [swing] if boolean(swing).is_ok() => Some(binding(service, swing)),
        _ => None,
    };
    // A known wind mode requires the dmaker strategy; an unrelated mode does not.
    let dmaker_semantics = service
        .properties
        .iter()
        .filter(|p| p.urn.standard("property", "mode", 8))
        .any(|mode| {
            let values = mode
                .value
                .get("value-list")
                .and_then(serde_json::Value::as_array);
            match values {
                Some(values)
                    if !values.is_empty()
                        && values.iter().all(|item| {
                            item.get("description")
                                .and_then(serde_json::Value::as_str)
                                .is_some()
                        }) =>
                {
                    values.iter().any(|item| {
                        let name = item["description"].as_str().unwrap();
                        name.eq_ignore_ascii_case("Straight Wind")
                            || name.eq_ignore_ascii_case("Natural Wind")
                    })
                }
                _ => instance.services.iter().any(|s| {
                    s.urn
                        .matches("dmaker-spec", "service", "dm-service", 0x7802)
                        && s.urn.vendor == instance.urn.vendor
                }),
            }
        });
    if is_dmaker_instance && !is_dmaker && dmaker_semantics {
        return Err(CapabilityError::InvalidDescription(
            "fan mode semantics are unconfirmed",
        ));
    }
    let (mode, speed) = if is_dmaker && is_dmaker_instance && dmaker_semantics {
        let (mode, speed) = dmaker_bindings(instance, service)?;
        (Some(mode), speed)
    } else {
        (None, generic_speed(service)?)
    };
    Ok(FanBindings::new(
        binding(service, power),
        mode,
        speed,
        swing,
    ))
}

fn dmaker_bindings(
    instance: &Description<'_>,
    service: &Service<'_>,
) -> Result<(FanModeBinding, FanSpeedBinding), CapabilityError> {
    let mode = required(service, |p| p.urn.standard("property", "mode", 8), "mode")?;
    access(mode)?;
    let mode_format = format(mode)?;
    let values = array(mode.value, "value-list")?;
    let mut straight = None;
    let mut native_values = Vec::new();
    for item in values {
        let value = item
            .get("value")
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .ok_or(CapabilityError::InvalidDescription("fan mode enumeration"))?;
        let name = string(item, "description")?;
        if name.eq_ignore_ascii_case("Straight Wind") && straight.replace(value).is_some() {
            return Err(CapabilityError::InvalidDescription("fan mode enumeration"));
        }
        native_values.push(value);
    }
    let mode = FanModeBinding::new(
        binding(service, mode),
        mode_format,
        straight.ok_or(CapabilityError::InvalidDescription("fan straight mode"))?,
        native_values,
    )?;
    let services = instance
        .services
        .iter()
        .filter(|s| {
            s.urn
                .matches("dmaker-spec", "service", "dm-service", 0x7802)
                && s.urn.vendor == instance.urn.vendor
        })
        .collect::<Vec<_>>();
    let [vendor] = services.as_slice() else {
        return Err(CapabilityError::MissingFunction("dm-service"));
    };
    let speed = required(
        vendor,
        |p| {
            p.urn.matches("dmaker-spec", "property", "speed-level", 1)
                && p.urn.vendor == instance.urn.vendor
        },
        "speed-level",
    )?;
    access(speed)?;
    let range = array(speed.value, "value-range")?;
    if range
        != [
            serde_json::json!(1),
            serde_json::json!(100),
            serde_json::json!(1),
        ]
    {
        return Err(CapabilityError::InvalidDescription("fan speed range"));
    }
    let speed = FanSpeedBinding::new(
        binding(vendor, speed),
        format(speed)?,
        FanSpeedEncoding::Range {
            min: 1,
            max: 100,
            step: 1,
        },
    )?;
    Ok((mode, speed))
}

fn generic_speed(service: &Service<'_>) -> Result<FanSpeedBinding, CapabilityError> {
    let candidates = service
        .properties
        .iter()
        .filter(|p| p.urn.standard("property", "fan-level", 0x16));
    let mut best: Option<FanSpeedBinding> = None;
    let mut tied = false;
    let mut error = CapabilityError::MissingFunction("fan-level");
    for property in candidates {
        let candidate = match parse_speed(service, property) {
            Ok(candidate) => candidate,
            Err(candidate_error) => {
                error = candidate_error;
                continue;
            }
        };
        match &best {
            Some(current) if candidate.count() < current.count() => {}
            Some(current) if candidate.count() == current.count() => tied = true,
            _ => {
                best = Some(candidate);
                tied = false;
            }
        }
    }
    if tied {
        Err(CapabilityError::InvalidDescription("ambiguous fan speed"))
    } else {
        best.ok_or(error)
    }
}

fn parse_speed(
    service: &Service<'_>,
    property: &Property<'_>,
) -> Result<FanSpeedBinding, CapabilityError> {
    access(property)?;
    let format = format(property)?;
    let encoding = match (
        property.value.get("value-range"),
        property.value.get("value-list"),
    ) {
        (Some(_), None) => {
            let values = array(property.value, "value-range")?;
            let [min, max, step] = values else {
                return Err(CapabilityError::InvalidDescription("fan speed range"));
            };
            FanSpeedEncoding::Range {
                min: min
                    .as_i64()
                    .ok_or(CapabilityError::InvalidDescription("fan speed range"))?,
                max: max
                    .as_i64()
                    .ok_or(CapabilityError::InvalidDescription("fan speed range"))?,
                step: step
                    .as_i64()
                    .ok_or(CapabilityError::InvalidDescription("fan speed range"))?,
            }
        }
        (None, Some(_)) => {
            let mut ordered = Vec::new();
            let mut family = None;
            for item in array(property.value, "value-list")? {
                let raw = item
                    .get("value")
                    .and_then(serde_json::Value::as_i64)
                    .ok_or(CapabilityError::InvalidDescription("fan speed enumeration"))?;
                let (kind, order) = level_order(string(item, "description")?).ok_or(
                    CapabilityError::InvalidDescription("fan speed enumeration semantics"),
                )?;
                if family.is_some_and(|current| current != kind)
                    || ordered.iter().any(|(existing, _)| *existing == order)
                {
                    return Err(CapabilityError::InvalidDescription(
                        "fan speed enumeration semantics",
                    ));
                }
                family = Some(kind);
                ordered.push((order, raw));
            }
            ordered.sort_by_key(|(order, _)| *order);
            FanSpeedEncoding::Discrete {
                values: ordered.into_iter().map(|(_, raw)| raw).collect(),
            }
        }
        _ => return Err(CapabilityError::InvalidDescription("fan speed shape")),
    };
    FanSpeedBinding::new(binding(service, property), format, encoding)
}

fn level_order(description: &str) -> Option<(u8, u32)> {
    let text = description.trim().to_ascii_lowercase();
    let compact: String = text
        .chars()
        .filter(|c| !matches!(c, ' ' | '-' | '_'))
        .collect();
    let named = match compact.as_str() {
        "low" | "lowspeed" => Some(1),
        "medium" | "mediumspeed" => Some(2),
        "high" | "highspeed" => Some(3),
        _ => None,
    };
    if let Some(order) = named {
        return Some((0, order));
    }
    let number = compact
        .strip_prefix("level")
        .or_else(|| compact.strip_prefix("gear"))
        .or_else(|| compact.strip_prefix("speed"))
        .unwrap_or(&compact);
    let order = number.parse::<u32>().ok()?;
    (order > 0).then_some((1, order))
}

fn required<'a>(
    service: &'a Service<'a>,
    matches: impl Fn(&Property<'_>) -> bool,
    name: &'static str,
) -> Result<&'a Property<'a>, CapabilityError> {
    let properties = service
        .properties
        .iter()
        .filter(|property| matches(property))
        .collect::<Vec<_>>();
    match properties.as_slice() {
        [property] => Ok(property),
        [] => Err(CapabilityError::MissingFunction(name)),
        _ => Err(CapabilityError::InvalidDescription(
            "ambiguous fan property",
        )),
    }
}
fn binding(service: &Service<'_>, property: &Property<'_>) -> PropertyBinding {
    PropertyBinding {
        siid: service.iid,
        piid: property.iid,
    }
}
fn boolean(property: &Property<'_>) -> Result<(), CapabilityError> {
    access(property)?;
    if string(property.value, "format")? != "bool" {
        return Err(CapabilityError::InvalidDescription("fan boolean format"));
    }
    Ok(())
}
fn access(property: &Property<'_>) -> Result<(), CapabilityError> {
    let access = array(property.value, "access")?;
    if access.iter().any(|item| item.as_str().is_none()) {
        return Err(CapabilityError::InvalidDescription("access"));
    }
    for permission in ["read", "write", "notify"] {
        if !access.iter().any(|item| item.as_str() == Some(permission)) {
            return Err(CapabilityError::MissingAccess(permission));
        }
    }
    Ok(())
}
fn format(property: &Property<'_>) -> Result<NumericFormat, CapabilityError> {
    match string(property.value, "format")? {
        "uint8" => Ok(NumericFormat::Uint8),
        "uint16" => Ok(NumericFormat::Uint16),
        "uint32" => Ok(NumericFormat::Uint32),
        "int8" => Ok(NumericFormat::Int8),
        "int16" => Ok(NumericFormat::Int16),
        "int32" => Ok(NumericFormat::Int32),
        _ => Err(CapabilityError::InvalidDescription("fan integer format")),
    }
}
