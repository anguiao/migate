//! Built-in source adaptations. Each rule has an explicit match, target and
//! stage; unrelated candidates never compete through registration order.
pub(super) mod cp7pd;
pub(super) mod dmaker;
pub(super) mod ls2307;

use super::{AppliedPatch, ParsedDevice, RecognitionDiagnostic, description::Urn};

#[derive(Clone, Copy)]
pub(super) struct BuiltinPatch {
    pub id: &'static str,
    pub stage: &'static str,
}
impl BuiltinPatch {
    pub fn applied(self, target: impl Into<String>) -> AppliedPatch {
        AppliedPatch {
            id: self.id.into(),
            target: target.into(),
            stage: self.stage.into(),
        }
    }
    pub fn failed(
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
    cp7pd::validate_trigger(device, model, urn)?;
    if let Some(metadata) = device.recognition() {
        for applied in metadata.patches() {
            let function = device
                .functions()
                .iter()
                .find(|function| function.id() == applied.target())
                .unwrap();
            let matches =
                |rule: BuiltinPatch| rule.id == applied.id() && rule.stage == applied.stage();
            if matches(ls2307::PATCH) {
                ls2307::validate_applied(model, urn, function)?;
            } else if matches(dmaker::PATCH) {
                dmaker::validate_applied(model, urn, function)?;
            } else if matches(cp7pd::CHANNELS) || matches(cp7pd::REFRESH) {
                cp7pd::validate_applied(model, urn, function, device, matches(cp7pd::REFRESH))?;
            } else {
                return Err("Xiaomi applied patch is invalid");
            }
        }
    }
    ls2307::validate_bindings(device, model, urn)?;
    dmaker::validate_bindings(device, model, urn)
}
