use super::{
    super::{
        AirConditionerBindings, CapabilityError, ParsedDevice, ParsedFunction,
        description::{Description, Property, Urn, string},
        fan,
    },
    BuiltinPatch,
};

pub(in crate::xiaomi::capability) const PATCH: BuiltinPatch = BuiltinPatch {
    id: "lumi.mcn04.air-conditioner",
    stage: "recognition and control/feedback",
};
const MODEL: &str = "lumi.acpartner.mcn04";

fn matches(model: &str, urn: &Urn<'_>) -> bool {
    model == MODEL
        && urn.standard("device", "air-condition-outlet", 0xa045)
        && urn.vendor == "lumi-mcn04"
}

pub(in crate::xiaomi::capability) fn parse(
    model: &str,
    instance: &Description<'_>,
    bindings: AirConditionerBindings,
) -> Result<AirConditionerBindings, CapabilityError> {
    if !matches(model, &instance.urn) {
        return Err(CapabilityError::IncompatibleModel(MODEL));
    }
    let services = instance
        .services
        .iter()
        .filter(|service| {
            service
                .urn
                .matches("lumi-spec", "service", "ac-function", 0x7801)
                && service.urn.vendor == "lumi-mcn04"
        })
        .collect::<Vec<_>>();
    let [service] = services.as_slice() else {
        return Err(CapabilityError::InvalidDescription(
            "mcn04 ac-function service",
        ));
    };
    let property = |name, type_id| -> Result<&Property<'_>, CapabilityError> {
        let properties = service
            .properties
            .iter()
            .filter(|property| {
                property.urn.matches("lumi-spec", "property", name, type_id)
                    && property.urn.vendor == "lumi-mcn04"
            })
            .collect::<Vec<_>>();
        let [property] = properties.as_slice() else {
            return Err(CapabilityError::InvalidDescription(
                "mcn04 feedback property",
            ));
        };
        let access = property
            .value
            .get("access")
            .and_then(serde_json::Value::as_array)
            .ok_or(CapabilityError::InvalidDescription("mcn04 feedback access"))?;
        if string(property.value, "format")? != "string"
            || !["read", "notify"]
                .iter()
                .all(|required| access.iter().any(|item| item.as_str() == Some(required)))
        {
            return Err(CapabilityError::InvalidDescription(
                "mcn04 feedback access or format",
            ));
        }
        Ok(property)
    };
    let state = property("ac-state", 7)?;
    let range = property("ac-ctrl-range", 6)?;
    bindings.with_mcn04(fan::binding(service, state), fan::binding(service, range))
}

pub(super) fn validate_applied(
    model: &str,
    urn: &Urn<'_>,
    function: &ParsedFunction,
) -> Result<(), &'static str> {
    if matches(model, urn)
        && function
            .air_conditioner()
            .is_some_and(|ac| ac.mcn04().is_some())
    {
        Ok(())
    } else {
        Err("Xiaomi mcn04 patch is invalid")
    }
}

pub(super) fn validate_bindings(
    device: &ParsedDevice,
    model: &str,
    urn: &Urn<'_>,
) -> Result<(), &'static str> {
    for function in device.functions() {
        if function
            .air_conditioner()
            .is_some_and(|ac| ac.mcn04().is_some())
        {
            validate_applied(model, urn, function)?;
            if !device.recognition().is_some_and(|metadata| {
                metadata.patches().iter().any(|patch| {
                    patch.id() == PATCH.id
                        && patch.stage() == PATCH.stage
                        && patch.target() == function.id()
                })
            }) {
                return Err("Xiaomi mcn04 patch metadata is missing");
            }
        } else if model == MODEL && function.air_conditioner().is_some() {
            return Err("Xiaomi mcn04 requires its feedback patch");
        }
    }
    Ok(())
}
