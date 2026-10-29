use super::{
    CapabilityError, ParsedFunction, PropertyBinding,
    description::{Description, Property, array, string},
};

pub(super) fn candidates(
    model: &str,
    instance: &Description<'_>,
    diagnostics: &mut Vec<super::RecognitionDiagnostic>,
    patches: &mut Vec<super::AppliedPatch>,
) -> (Vec<ParsedFunction>, Option<PropertyBinding>) {
    let (mut functions, trigger, mentioned) =
        super::patch::cp7pd::candidates(model, instance, diagnostics, patches);
    if !instance.urn.standard("device", "outlet", 0x0000a002) {
        return (functions, trigger);
    }
    for service in instance.services.iter().filter(|s| {
        s.urn.standard("service", "switch", 0x780c)
            && (!mentioned || ![2, 3, 4, 9].contains(&s.iid))
    }) {
        let target = format!("outlet:{}", service.iid);
        let properties = service
            .properties
            .iter()
            .filter(|p| p.urn.standard("property", "on", 6))
            .collect::<Vec<_>>();
        let result = match properties.as_slice() {
            [property] => power_binding(service.iid, property),
            [] => Err(CapabilityError::MissingMainPower),
            _ => Err(CapabilityError::AmbiguousMainPower),
        };
        match result {
            Ok(power) => functions.push(ParsedFunction::outlet(target, "主电源", power)),
            Err(error) => {
                diagnostics.push(super::RecognitionDiagnostic {
                    target,
                    reason: error.to_string(),
                });
            }
        }
    }
    if functions.is_empty() && diagnostics.is_empty() {
        diagnostics.push(super::RecognitionDiagnostic {
            target: "outlet".into(),
            reason: CapabilityError::MissingMainPower.to_string(),
        });
    }
    (functions, trigger)
}

pub(super) fn power_binding(
    siid: u32,
    property: &Property<'_>,
) -> Result<PropertyBinding, CapabilityError> {
    if string(property.value, "format")? != "bool" {
        return Err(CapabilityError::InvalidMainPowerType);
    }
    let access = array(property.value, "access")?
        .iter()
        .map(|value| {
            value
                .as_str()
                .ok_or(CapabilityError::InvalidDescription("access"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    for permission in ["read", "write", "notify"] {
        if !access.contains(&permission) {
            return Err(CapabilityError::MissingAccess(permission));
        }
    }
    Ok(PropertyBinding {
        siid,
        piid: property.iid,
    })
}
