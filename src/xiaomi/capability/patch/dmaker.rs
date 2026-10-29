use super::super::{
    CapabilityError, FanModeBinding, FanSpeedBinding, FanSpeedEncoding, ParsedDevice,
    ParsedFunction,
    description::{Description, Service, Urn, array, string},
    fan::{access, binding, format, required},
};
use super::BuiltinPatch;

pub(in crate::xiaomi::capability) const PATCH: BuiltinPatch = BuiltinPatch {
    id: "dmaker.fan.straight-wind",
    stage: "recognition and control/feedback",
};

#[derive(Clone, Copy, Eq, PartialEq)]
pub(in crate::xiaomi::capability) enum Match {
    Unrelated,
    Unconfirmed,
    Confirmed,
}

fn identity(model: &str, urn: &Urn<'_>) -> bool {
    model.starts_with("dmaker.") && urn.vendor.starts_with("dmaker-")
}

pub(in crate::xiaomi::capability) fn match_service(
    model: &str,
    instance: &Description<'_>,
    service: &Service<'_>,
) -> Match {
    if !instance.urn.vendor.starts_with("dmaker-") || !semantics(instance, service) {
        Match::Unrelated
    } else if identity(model, &instance.urn) {
        Match::Confirmed
    } else {
        Match::Unconfirmed
    }
}

pub(super) fn validate_applied(
    model: &str,
    urn: &Urn<'_>,
    function: &ParsedFunction,
) -> Result<(), &'static str> {
    if identity(model, urn) && function.fan().is_some_and(|fan| fan.mode().is_some()) {
        Ok(())
    } else {
        Err("Xiaomi applied patch is invalid")
    }
}

pub(super) fn validate_bindings(
    device: &ParsedDevice,
    model: &str,
    urn: &Urn<'_>,
) -> Result<(), &'static str> {
    for fan in device
        .functions()
        .iter()
        .filter_map(|function| function.fan())
    {
        if fan.mode().is_some()
            && (!identity(model, urn)
                || fan.speed().encoding()
                    != &(FanSpeedEncoding::Range {
                        min: 1,
                        max: 100,
                        step: 1,
                    })
                || fan.speed().property().siid == fan.power().siid)
        {
            return Err("Xiaomi fan mode is incompatible with its model or speed binding");
        }
    }
    Ok(())
}

/// Match only the confirmed wind-mode semantics, not the vendor name alone.
fn semantics(instance: &Description<'_>, service: &Service<'_>) -> bool {
    service
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
        })
}

pub(in crate::xiaomi::capability) fn bindings(
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
