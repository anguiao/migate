use super::{
    CapabilityError, ParsedFunction, RecognitionDiagnostic,
    description::{Description, Property, Service, array, binding, integer_format},
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
        .filter(|service| service.urn.standard("service", "switch", 0x780c))
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

fn parse(
    service: &Service<'_>,
) -> Result<(super::PropertyBinding, super::PropertyBinding, bool), CapabilityError> {
    let power = required(service, "on", 6)?;
    let power = super::outlet::power_binding(service.iid, power)?;
    let mode = required(service, "mode", 8)?;
    integer_format(mode)?;
    let access = array(mode.value, "access")?;
    if access.iter().any(|entry| entry.as_str().is_none()) {
        return Err(CapabilityError::InvalidDescription("mode access"));
    }
    if !access.iter().any(|entry| entry.as_str() == Some("read")) {
        return Err(CapabilityError::MissingAccess("read"));
    }
    let notify = access.iter().any(|entry| entry.as_str() == Some("notify"));
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

fn required<'a>(
    service: &'a Service<'_>,
    name: &'static str,
    id: u32,
) -> Result<&'a Property<'a>, CapabilityError> {
    let mut properties = service
        .properties
        .iter()
        .filter(|property| property.urn.standard("property", name, id));
    let property = properties
        .next()
        .ok_or(CapabilityError::MissingFunction(name))?;
    if properties.next().is_some() {
        return Err(CapabilityError::InvalidDescription(
            "ambiguous relay property",
        ));
    }
    Ok(property)
}
