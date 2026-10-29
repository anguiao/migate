use super::patch::dmaker;
use super::{
    CapabilityError, FanBindings, FanSpeedBinding, FanSpeedEncoding, NumericFormat,
    PropertyBinding, RecognitionDiagnostic,
    description::{Description, Property, Service, array, string},
};

pub(super) fn parse_fan_service(
    model: &str,
    instance: &Description<'_>,
    service: &Service<'_>,
    diagnostics: &mut Vec<RecognitionDiagnostic>,
    patches: &mut Vec<super::AppliedPatch>,
) -> Result<FanBindings, RecognitionDiagnostic> {
    let matched = dmaker::match_service(model, instance, service);
    let target = format!("fan:{}", service.iid);
    match parse_bindings(instance, service, matched, diagnostics) {
        Ok(fan) => {
            if matched == dmaker::Match::Confirmed {
                patches.push(dmaker::PATCH.applied(target));
            }
            Ok(fan)
        }
        Err(error) => Err(if matched == dmaker::Match::Confirmed {
            dmaker::PATCH.failed(target, error)
        } else {
            RecognitionDiagnostic {
                target,
                reason: error.to_string(),
            }
        }),
    }
}

fn parse_bindings(
    instance: &Description<'_>,
    service: &Service<'_>,
    matched: dmaker::Match,
    diagnostics: &mut Vec<RecognitionDiagnostic>,
) -> Result<FanBindings, CapabilityError> {
    let power = required(service, |p| p.urn.standard("property", "on", 6), "on")?;
    boolean(power)?;
    let swings = service
        .properties
        .iter()
        .filter(|p| p.urn.standard("property", "horizontal-swing", 0x17))
        .collect::<Vec<_>>();
    let swing = match swings.as_slice() {
        [] => None,
        [swing] => match boolean(swing) {
            Ok(()) => Some(binding(service, swing)),
            Err(error) => {
                diagnostics.push(super::RecognitionDiagnostic {
                    target: format!("fan:{}", service.iid),
                    reason: format!("horizontal swing: {error}"),
                });
                None
            }
        },
        _ => {
            diagnostics.push(super::RecognitionDiagnostic {
                target: format!("fan:{}", service.iid),
                reason: "horizontal swing property is ambiguous".into(),
            });
            None
        }
    };
    let (mode, speed) = match matched {
        dmaker::Match::Confirmed => {
            let (mode, speed) = dmaker::bindings(instance, service)?;
            (Some(mode), speed)
        }
        dmaker::Match::Unconfirmed => {
            return Err(CapabilityError::InvalidDescription(
                "fan mode semantics are unconfirmed",
            ));
        }
        dmaker::Match::Unrelated => (None, generic_speed(service, diagnostics)?),
    };
    Ok(FanBindings::new(
        binding(service, power),
        mode,
        speed,
        swing,
    ))
}

fn generic_speed(
    service: &Service<'_>,
    diagnostics: &mut Vec<super::RecognitionDiagnostic>,
) -> Result<FanSpeedBinding, CapabilityError> {
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
                diagnostics.push(super::RecognitionDiagnostic {
                    target: format!("fan:{}", service.iid),
                    reason: format!(
                        "fan-level {}.{}: {candidate_error}",
                        service.iid, property.iid
                    ),
                });
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

pub(super) fn required<'a>(
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
pub(super) fn binding(service: &Service<'_>, property: &Property<'_>) -> PropertyBinding {
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
pub(super) fn access(property: &Property<'_>) -> Result<(), CapabilityError> {
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
pub(super) fn format(property: &Property<'_>) -> Result<NumericFormat, CapabilityError> {
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
