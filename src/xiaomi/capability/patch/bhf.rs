use super::{
    super::{
        ActionBinding, AppliedPatch, CapabilityError, ParsedDevice, ParsedFunction,
        PropertyBinding, RecognitionDiagnostic, SharedFanChannel, SharedFanSpeedBinding,
        description::{
            Description, Service, Urn, array, binding, control_access, integer_format, string,
        },
    },
    BuiltinPatch,
};
use crate::xiaomi::capability::{FanBindings, LegacyArgument, LegacyCommandBinding};

const MODEL: &str = "yeelink.bhf_light.v13";
const HEAT_LEVEL: PropertyBinding = PropertyBinding { siid: 3, piid: 5 };
const MODE: PropertyBinding = PropertyBinding { siid: 3, piid: 1 };
const BLOW: &str = "fan:3:2";
const VENTILATION: &str = "fan:3:4";
const HEATER: &str = "heater:3";

pub(in crate::xiaomi::capability) const FAN_PATCH: BuiltinPatch = BuiltinPatch {
    id: "yeelink.bhf-light.v13.shared-fans",
    stage: "recognition and control/feedback",
};
pub(in crate::xiaomi::capability) const MODE_PATCH: BuiltinPatch = BuiltinPatch {
    id: "yeelink.bhf-light.v13.native-mode",
    stage: "recognition and diagnostics",
};
pub(in crate::xiaomi::capability) const SHARED_PATCH: BuiltinPatch = BuiltinPatch {
    id: "yeelink.bhf-light.v13.shared-feedback",
    stage: "shared legacy feedback",
};

fn matches(model: &str, urn: &Urn<'_>) -> bool {
    model == MODEL && urn.standard("device", "bath-heater", 0xa028) && urn.vendor == "yeelink-v13"
}

pub(in crate::xiaomi::capability) fn apply(
    model: &str,
    instance: &Description<'_>,
    functions: &mut Vec<ParsedFunction>,
    diagnostics: &mut Vec<RecognitionDiagnostic>,
    patches: &mut Vec<AppliedPatch>,
) {
    if model != MODEL {
        return;
    }
    if !matches(model, &instance.urn) {
        diagnostics
            .push(FAN_PATCH.failed("bath-heater", "model and instance identity do not match"));
        return;
    }

    for (channel, id, label, name, type_id) in [
        (SharedFanChannel::Blow, BLOW, "吹风", "blow", 0x00cd_u32),
        (
            SharedFanChannel::Ventilation,
            VENTILATION,
            "换气",
            "ventilation",
            0x00ce_u32,
        ),
    ] {
        match parse_fan(instance, channel, name, type_id, diagnostics) {
            Ok(fan) => {
                patches.push(FAN_PATCH.applied(id));
                functions.push(ParsedFunction::fan_control(id, label, fan));
            }
            Err(error) => diagnostics.push(FAN_PATCH.failed(id, error)),
        }
    }

    let Some(index) = functions
        .iter()
        .position(|function| function.id() == HEATER)
    else {
        return;
    };
    let Some(mut heater) = functions[index].heater().cloned() else {
        return;
    };
    // The shared group exists independently of fan/mode admission.
    patches.push(SHARED_PATCH.applied(HEATER));
    match feedback_property(instance, MODE, "mode", 0x08, &["read", "notify"])
        .and_then(|mode| heater.set_diagnostic_mode(mode))
    {
        Ok(()) => patches.push(MODE_PATCH.applied(HEATER)),
        Err(error) => diagnostics.push(MODE_PATCH.failed(HEATER, error)),
    }
    match feedback_property(instance, HEAT_LEVEL, "heat-level", 0x47, &["notify"]) {
        Ok(trigger) => heater.set_power_refresh_trigger(Some(trigger)),
        Err(error) => diagnostics.push(SHARED_PATCH.failed(HEATER, error)),
    }
    functions[index] = ParsedFunction::heater_control(functions[index].label(), heater);
}

fn ptc_service<'a>(instance: &'a Description<'a>) -> Result<&'a Service<'a>, CapabilityError> {
    let services = instance
        .services
        .iter()
        .filter(|service| service.urn.standard("service", "ptc-bath-heater", 0x783b))
        .collect::<Vec<_>>();
    match services.as_slice() {
        [service] => Ok(service),
        [] => Err(CapabilityError::MissingFunction("ptc-bath-heater")),
        _ => Err(CapabilityError::InvalidDescription(
            "bath-heater service is missing or ambiguous",
        )),
    }
}

fn parse_fan(
    instance: &Description<'_>,
    channel: SharedFanChannel,
    name: &str,
    type_id: u32,
    diagnostics: &mut Vec<RecognitionDiagnostic>,
) -> Result<FanBindings, CapabilityError> {
    let service = ptc_service(instance)?;
    let properties = service
        .properties
        .iter()
        .filter(|property| property.urn.standard("property", name, type_id))
        .collect::<Vec<_>>();
    let [property] = properties.as_slice() else {
        return Err(CapabilityError::MissingFunction(match channel {
            SharedFanChannel::Blow => "blow",
            SharedFanChannel::Ventilation => "ventilation",
        }));
    };
    control_access(property)?;
    if string(property.value, "format")? != "bool" {
        return Err(CapabilityError::InvalidDescription("fan boolean format"));
    }
    let target = match channel {
        SharedFanChannel::Blow => BLOW,
        SharedFanChannel::Ventilation => VENTILATION,
    };
    let cloud_action = match low_action(instance, channel) {
        Ok(action) => action,
        Err(error) => {
            diagnostics.push(FAN_PATCH.failed(target, format!("cloud low-speed action: {error}")));
            None
        }
    };
    let (method, args) = match channel {
        SharedFanChannel::Blow => (
            "set_bh_mode",
            vec![
                LegacyArgument::String("coolwind".into()),
                LegacyArgument::Integer(1),
            ],
        ),
        SharedFanChannel::Ventilation => (
            "set_bh_mode",
            vec![
                LegacyArgument::String("venting".into()),
                LegacyArgument::Integer(1),
            ],
        ),
    };
    let direct = LegacyCommandBinding::new(method, args)?;
    let speed = SharedFanSpeedBinding::new(channel, cloud_action, Some(direct))?;
    Ok(FanBindings::shared(binding(service, property), speed))
}

fn feedback_property(
    instance: &Description<'_>,
    expected: PropertyBinding,
    name: &'static str,
    type_id: u32,
    permissions: &[&'static str],
) -> Result<PropertyBinding, CapabilityError> {
    let service = ptc_service(instance)?;
    let properties = service
        .properties
        .iter()
        .filter(|property| property.urn.standard("property", name, type_id))
        .collect::<Vec<_>>();
    let [property] = properties.as_slice() else {
        return Err(CapabilityError::MissingFunction(name));
    };
    let access = array(property.value, "access")?;
    for &permission in permissions {
        if !access.iter().any(|item| item.as_str() == Some(permission)) {
            return Err(CapabilityError::MissingAccess(permission));
        }
    }
    integer_format(property)?;
    let binding = binding(service, property);
    if binding != expected {
        return Err(CapabilityError::InvalidDescription(
            "heater feedback binding",
        ));
    }
    Ok(binding)
}

fn low_action(
    instance: &Description<'_>,
    channel: SharedFanChannel,
) -> Result<Option<ActionBinding>, CapabilityError> {
    let (name, type_id) = match channel {
        SharedFanChannel::Blow => ("blow-low", 0x2804_u32),
        SharedFanChannel::Ventilation => ("ventilation-low", 0x2805_u32),
    };
    let actions = instance
        .services
        .iter()
        .flat_map(|service| {
            service.actions.iter().filter_map(move |action| {
                action
                    .urn
                    .matches("yeelink-spec", "action", name, type_id)
                    .then_some((service, action))
            })
        })
        .collect::<Vec<_>>();
    let [(service, action)] = actions.as_slice() else {
        return if actions.is_empty() {
            Ok(None)
        } else {
            Err(CapabilityError::InvalidDescription(
                "low-speed action is ambiguous",
            ))
        };
    };
    for field in ["in", "out"] {
        if !array(action.value, field)?.is_empty() {
            return Err(CapabilityError::InvalidDescription(
                "low-speed action arguments",
            ));
        }
    }
    Ok(Some(ActionBinding::new(service.iid, action.iid)?))
}

pub(super) fn validate_shared_applied(
    model: &str,
    urn: &Urn<'_>,
    function: &ParsedFunction,
) -> Result<(), &'static str> {
    if matches(model, urn) && function.heater().is_some() {
        Ok(())
    } else {
        Err("Xiaomi bhf shared feedback patch is invalid")
    }
}

pub(in crate::xiaomi::capability) fn shared_feedback_target(device: &ParsedDevice) -> Option<&str> {
    let mut targets = device
        .recognition()?
        .patches()
        .iter()
        .filter(|patch| patch.id() == SHARED_PATCH.id && patch.stage() == SHARED_PATCH.stage)
        .map(AppliedPatch::target);
    let target = targets.next()?;
    (targets.next().is_none()
        && device
            .functions()
            .iter()
            .any(|function| function.id() == target && function.heater().is_some()))
    .then_some(target)
}

pub(super) fn validate_applied(
    model: &str,
    urn: &Urn<'_>,
    function: &ParsedFunction,
) -> Result<(), &'static str> {
    if !matches(model, urn) {
        return Err("Xiaomi bhf patch does not match the device");
    }
    if let Some(fan) = function.fan() {
        let Some(speed) = fan.shared_speed() else {
            return Err("Xiaomi bhf shared fan patch is invalid");
        };
        let (channel, cloud_aiid, mode) = match function.id() {
            BLOW => (SharedFanChannel::Blow, 4, "coolwind"),
            VENTILATION => (SharedFanChannel::Ventilation, 5, "venting"),
            _ => return Err("Xiaomi bhf shared fan patch is invalid"),
        };
        let direct = speed.direct_command.as_ref();
        let direct_valid = direct.is_none_or(|command| {
            command.method == "set_bh_mode"
                && command.args
                    == [
                        LegacyArgument::String(mode.into()),
                        LegacyArgument::Integer(1),
                    ]
        });
        let cloud_valid = speed
            .cloud_action
            .is_none_or(|action| action.siid == 4 && action.aiid == cloud_aiid);
        let path_valid =
            (direct.is_some() || speed.cloud_action.is_some()) && direct_valid && cloud_valid;
        let valid = fan.power() == shared_power(function.id())
            && fan.speed_count() == 2
            && speed.channel == channel
            && path_valid
            && fan.mode().is_none()
            && fan.swing().is_none();
        return if valid {
            Ok(())
        } else {
            Err("Xiaomi bhf shared fan patch is invalid")
        };
    }
    if let Some(heater) = function.heater() {
        return if heater.diagnostic_mode() == Some(MODE) {
            Ok(())
        } else {
            Err("Xiaomi bhf native mode patch is invalid")
        };
    }
    Err("Xiaomi bhf patch target is invalid")
}

fn shared_power(id: &str) -> PropertyBinding {
    match id {
        BLOW => PropertyBinding { siid: 3, piid: 2 },
        VENTILATION => PropertyBinding { siid: 3, piid: 4 },
        _ => PropertyBinding { siid: 0, piid: 0 },
    }
}

pub(super) fn validate_bindings(
    device: &ParsedDevice,
    model: &str,
    urn: &Urn<'_>,
) -> Result<(), &'static str> {
    let supported = model == MODEL && matches(model, urn);
    let metadata = device.recognition();
    if device.recognition().is_some_and(|metadata| {
        metadata
            .patches()
            .iter()
            .any(|patch| patch.id() == SHARED_PATCH.id && patch.stage() == SHARED_PATCH.stage)
    }) {
        let target = shared_feedback_target(device)
            .ok_or("Xiaomi bhf shared feedback patch target is invalid")?;
        let function = device
            .functions()
            .iter()
            .find(|function| function.id() == target)
            .ok_or("Xiaomi bhf shared feedback patch target is missing")?;
        validate_shared_applied(model, urn, function)?;
        if !supported {
            return Err("Xiaomi shared feedback is unsupported");
        }
    }
    for function in device.functions() {
        if let Some(trigger) = function
            .heater()
            .and_then(|heater| heater.power_refresh_trigger())
            && (!supported || function.id() != HEATER || trigger != HEAT_LEVEL)
        {
            return Err("Xiaomi heater power refresh trigger is invalid");
        }
        if function
            .fan()
            .is_some_and(|fan| fan.shared_speed().is_some())
        {
            if !supported {
                return Err("Xiaomi shared fan feedback is unsupported");
            }
            validate_applied(model, urn, function)?;
            if !has_patch(metadata, FAN_PATCH, function.id()) {
                return Err("Xiaomi bhf shared fan patch metadata is missing");
            }
        }
        if function
            .heater()
            .is_some_and(|heater| heater.diagnostic_mode().is_some())
        {
            if !supported {
                return Err("Xiaomi native mode diagnostic is unsupported");
            }
            validate_applied(model, urn, function)?;
            if !has_patch(metadata, MODE_PATCH, function.id()) {
                return Err("Xiaomi bhf native mode patch metadata is missing");
            }
        }
    }
    Ok(())
}

fn has_patch(
    metadata: Option<&super::super::RecognitionMetadata>,
    patch: BuiltinPatch,
    target: &str,
) -> bool {
    metadata.is_some_and(|metadata| {
        metadata.patches().iter().any(|applied| {
            applied.id() == patch.id && applied.stage() == patch.stage && applied.target() == target
        })
    })
}
