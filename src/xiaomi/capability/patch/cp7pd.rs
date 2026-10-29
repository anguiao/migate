use super::super::{
    AppliedPatch, CapabilityError, ParsedDevice, ParsedFunction, PropertyBinding,
    RecognitionDiagnostic,
    description::{Description, Urn},
    outlet::power_binding,
};
use super::BuiltinPatch;
use crate::device::FunctionKind;

const MODEL: &str = "cuco.plug.cp7pd";
pub(super) const CHANNELS: BuiltinPatch = BuiltinPatch {
    id: "cuco.cp7pd.channels",
    stage: "recognition",
};
pub(super) const REFRESH: BuiltinPatch = BuiltinPatch {
    id: "cuco.cp7pd.refresh",
    stage: "feedback",
};
const TRIGGER: PropertyBinding = PropertyBinding { siid: 2, piid: 1 };

fn identity(model: &str, urn: &Urn<'_>) -> (bool, bool) {
    let instance = urn.standard("device", "outlet", 0xa002) && urn.vendor == "cuco-cp7pd";
    (model == MODEL || instance, model == MODEL && instance)
}

pub(in crate::xiaomi::capability) fn candidates(
    model: &str,
    instance: &Description<'_>,
    diagnostics: &mut Vec<RecognitionDiagnostic>,
    patches: &mut Vec<AppliedPatch>,
) -> (Vec<ParsedFunction>, Option<PropertyBinding>, bool) {
    let (mentioned, matched) = identity(model, &instance.urn);
    let mut functions = Vec::new();
    let trigger = if mentioned && !matched {
        diagnostics.push(CHANNELS.failed("cp7pd", "model and instance identity do not match"));
        None
    } else if matched {
        let trigger = match cp7pd_binding(instance, "master", 2, "miot-spec-v2", 0x780c) {
            Ok(binding) => Some(binding),
            Err(error) => {
                diagnostics.push(REFRESH.failed("cp7pd master", &error));
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
                    patches.push(CHANNELS.applied(target.clone()));
                    if trigger.is_some() {
                        patches.push(REFRESH.applied(target));
                    }
                }
                Err(error) => {
                    diagnostics.push(CHANNELS.failed(target, &error));
                }
            }
        }
        trigger
    } else {
        None
    };
    (functions, trigger, mentioned)
}

pub(super) fn validate_trigger(
    device: &ParsedDevice,
    model: &str,
    urn: &Urn<'_>,
) -> Result<(), &'static str> {
    if let Some(trigger) = device.refresh_trigger()
        && (!identity(model, urn).1 || trigger != TRIGGER)
    {
        return Err("Xiaomi refresh trigger is unsupported");
    }
    Ok(())
}

pub(super) fn validate_applied(
    model: &str,
    urn: &Urn<'_>,
    function: &ParsedFunction,
    device: &ParsedDevice,
    refresh: bool,
) -> Result<(), &'static str> {
    let siid = match function.id() {
        "outlet:3" => 3,
        "outlet:4" => 4,
        "outlet:9" => 9,
        _ => return Err("Xiaomi cp7pd patch target is invalid"),
    };
    if !identity(model, urn).1
        || function.kind() != FunctionKind::Outlet
        || function.power() != Some(PropertyBinding { siid, piid: 1 })
    {
        return Err("Xiaomi cp7pd patch binding is invalid");
    }
    if refresh && device.refresh_trigger().is_none() {
        return Err("Xiaomi cp7pd refresh patch has no trigger");
    }
    Ok(())
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
