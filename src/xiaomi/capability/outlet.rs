use super::{
    CapabilityError, ParsedFunction, PropertyBinding,
    description::{Description, Property, Urn, array, string},
};

pub(super) fn candidates(
    model: &str,
    instance: &Description<'_>,
    diagnostics: &mut Vec<super::RecognitionDiagnostic>,
    patches: &mut Vec<super::AppliedPatch>,
) -> (Vec<ParsedFunction>, Option<PropertyBinding>) {
    let (mentioned, matched) = super::patch::cp7pd_identity(model, &instance.urn);
    let mut functions = Vec::new();
    let trigger = if mentioned && !matched {
        diagnostics.push(
            super::patch::CP7PD_CHANNELS
                .failed("cp7pd", "model and instance identity do not match"),
        );
        None
    } else if matched {
        let trigger = match cp7pd_binding(instance, "master", 2, "miot-spec-v2", 0x780c) {
            Ok(binding) => Some(binding),
            Err(error) => {
                diagnostics.push(super::patch::CP7PD_REFRESH.failed("cp7pd master", &error));
                None
            }
        };
        for (id, label, siid, namespace, type_id) in [
            ("outlet_1", "开关 1", 3, "cuco-spec", 0x7801),
            ("outlet_2", "开关 2", 4, "cuco-spec", 0x7802),
            ("usb", "USB", 9, "miot-spec-v2", 0x780c),
        ] {
            let target = format!("outlet:{siid}");
            match cp7pd_binding(instance, id, siid, namespace, type_id) {
                Ok(binding) => {
                    functions.push(ParsedFunction::outlet(target.clone(), label, binding));
                    patches.push(super::patch::CP7PD_CHANNELS.applied(target.clone()));
                    if trigger.is_some() {
                        patches.push(super::patch::CP7PD_REFRESH.applied(target));
                    }
                }
                Err(error) => {
                    diagnostics.push(super::patch::CP7PD_CHANNELS.failed(target, &error));
                }
            }
        }
        trigger
    } else {
        None
    };
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

pub(crate) const CP7PD_MODEL: &str = "cuco.plug.cp7pd";

pub(crate) fn is_cp7pd_type(value: &str) -> bool {
    Urn::parse(value)
        .is_ok_and(|urn| urn.standard("device", "outlet", 0x0000A002) && urn.vendor == "cuco-cp7pd")
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

fn cp7pd_binding(
    instance: &Description<'_>,
    id: &'static str,
    siid: u32,
    namespace: &str,
    type_id: u32,
) -> Result<PropertyBinding, CapabilityError> {
    let service = instance
        .services
        .iter()
        .find(|service| service.iid == siid)
        .ok_or(CapabilityError::MissingFunction(id))?;
    if !cp7pd_urn(&service.urn, namespace, "service", "switch", type_id) {
        return Err(CapabilityError::IncompatibleModel("cuco.plug.cp7pd"));
    }
    let property = service
        .properties
        .iter()
        .find(|property| property.iid == 1)
        .ok_or(CapabilityError::MissingFunction(id))?;
    let property_id = if namespace == "cuco-spec" { 1 } else { 6 };
    if !cp7pd_urn(&property.urn, namespace, "property", "on", property_id) {
        return Err(CapabilityError::IncompatibleModel("cuco.plug.cp7pd"));
    }
    power_binding(siid, property)
}

fn cp7pd_urn(urn: &Urn<'_>, namespace: &str, entity: &str, name: &str, type_id: u32) -> bool {
    urn.matches(namespace, entity, name, type_id) && urn.vendor == "cuco-cp7pd"
}
