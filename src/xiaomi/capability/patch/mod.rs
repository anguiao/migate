//! Built-in source adaptations. Each rule has an explicit match, target and
//! stage; unrelated candidates never compete through registration order.
pub(super) mod bhf;
pub(super) mod cp7pd;
pub(super) mod dmaker;
pub(super) mod ls2307;
pub(super) mod mcn02;
pub(super) mod mcn04;

use super::{AppliedPatch, ParsedDevice, RecognitionDiagnostic, description::Urn};

#[derive(Clone, Copy, Debug)]
pub(super) struct BuiltinPatch {
    pub(super) id: &'static str,
    pub(super) stage: &'static str,
}
impl BuiltinPatch {
    pub(super) fn applied(self, target: impl Into<String>) -> AppliedPatch {
        AppliedPatch {
            id: self.id.into(),
            target: target.into(),
            stage: self.stage.into(),
        }
    }
    pub(super) fn failed(
        self,
        target: impl Into<String>,
        reason: impl std::fmt::Display,
    ) -> RecognitionDiagnostic {
        RecognitionDiagnostic {
            target: target.into(),
            reason: format!("patch {}: {reason}", self.id),
        }
    }
}

pub(super) fn validate_native(
    device: &ParsedDevice,
    model: &str,
    urn: &Urn<'_>,
) -> Result<(), &'static str> {
    if device
        .functions()
        .iter()
        .any(|function| function.relay_mode().is_some())
        && !super::is_load_panel_urn(urn)
    {
        return Err("relay device type is invalid");
    }
    bhf::validate_bindings(device, model, urn)?;
    cp7pd::validate_trigger(device, model, urn)?;
    for applied in device.recognition().patches() {
        let function = device
            .functions()
            .iter()
            .find(|function| function.id() == applied.target())
            .unwrap();
        let matches = |rule: BuiltinPatch| rule.id == applied.id() && rule.stage == applied.stage();
        if matches(bhf::SHARED_PATCH) {
            bhf::validate_shared_applied(model, urn, function)?;
        } else if matches(bhf::FAN_PATCH) || matches(bhf::MODE_PATCH) {
            bhf::validate_applied(model, urn, function)?;
        } else if matches(mcn04::PATCH) {
            mcn04::validate_applied(model, urn, function)?;
        } else if matches(ls2307::PATCH) {
            ls2307::validate_applied(model, urn, function)?;
        } else if matches(dmaker::PATCH) {
            dmaker::validate_applied(model, urn, function)?;
        } else if matches(cp7pd::CHANNELS) || matches(cp7pd::REFRESH) {
            cp7pd::validate_applied(model, urn, function, device, matches(cp7pd::REFRESH))?;
        } else {
            return Err("Xiaomi applied patch is invalid");
        }
    }
    mcn04::validate_bindings(device, model, urn)?;
    ls2307::validate_bindings(device, model, urn)?;
    dmaker::validate_bindings(device, model, urn)
}
