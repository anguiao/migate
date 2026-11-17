use super::{
    CapabilityError, ParsedFunction, RecognitionDiagnostic,
    description::{Description, Service, array, binding},
};

pub(super) fn candidates(
    description: &Description<'_>,
    diagnostics: &mut Vec<RecognitionDiagnostic>,
) -> Vec<ParsedFunction> {
    if !super::is_load_panel_urn(&description.urn) {
        return Vec::new();
    }
    let mut functions = Vec::new();
    for service in description
        .services
        .iter()
        .filter(|service| is_relay_service(service))
    {
        let id = format!("relay:{}", service.iid);
        match parse(service) {
            Ok((power, mode, notify)) => {
                functions.push(ParsedFunction::relay(id, "继电器", power, mode, notify));
            }
            Err(error) => diagnostics.push(RecognitionDiagnostic {
                target: id,
                reason: error.to_string(),
            }),
        }
    }
    functions
}

pub(super) fn is_relay_service(service: &Service<'_>) -> bool {
    service.urn.standard("service", "switch", 0x780c)
}

fn parse(
    service: &Service<'_>,
) -> Result<(super::PropertyBinding, super::PropertyBinding, bool), CapabilityError> {
    let power = service.property("on", 6)?;
    let power = super::outlet::power_binding(service.iid, power)?;
    let mode = service.property("mode", 8)?;
    mode.integer_format()?;
    let notify = mode.access(&["read"])?.contains("notify");
    let values = array(mode.value, "value-list")?;
    let mut wired = false;
    let mut wireless = false;
    for entry in values {
        let description = entry
            .get("description")
            .and_then(serde_json::Value::as_str)
            .ok_or(CapabilityError::InvalidDescription("relay mode semantics"))?
            .trim()
            .to_ascii_lowercase();
        match (
            entry.get("value").and_then(serde_json::Value::as_u64),
            description.as_str(),
        ) {
            (Some(0), "wired" | "wired and wireless") if !wired => wired = true,
            (Some(1), "wireless") if !wireless => wireless = true,
            _ => return Err(CapabilityError::InvalidDescription("relay mode semantics")),
        }
    }
    if !wired || !wireless {
        return Err(CapabilityError::InvalidDescription("relay mode semantics"));
    }
    Ok((power, binding(service, mode), notify))
}
