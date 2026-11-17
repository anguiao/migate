use super::{
    CapabilityError, ParsedFunction, PropertyBinding,
    description::{Description, Property},
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
        let result = match service.optional_property("on", 6) {
            Ok(Some(property)) => power_binding(service.iid, property),
            Ok(None) => Err(CapabilityError::MissingMainPower),
            Err(_) => Err(CapabilityError::AmbiguousMainPower),
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
    property.boolean_control().map_err(|error| match error {
        CapabilityError::InvalidDescription("boolean property format") => {
            CapabilityError::InvalidMainPowerType
        }
        other => other,
    })?;
    Ok(PropertyBinding {
        siid,
        piid: property.iid,
    })
}
