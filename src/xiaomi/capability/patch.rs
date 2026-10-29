//! Built-in source adaptations. Each rule has an explicit match, target and
//! stage; unrelated candidates never compete through registration order.
use super::{AppliedPatch, RecognitionDiagnostic, description::Urn, ls2307, outlet};

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

pub(super) const LS2307_RGB: BuiltinPatch = BuiltinPatch {
    id: "devcea.ls2307.rgb",
    stage: "recognition and control/feedback",
};
pub(super) const CP7PD_CHANNELS: BuiltinPatch = BuiltinPatch {
    id: "cuco.cp7pd.channels",
    stage: "recognition",
};
pub(super) const CP7PD_REFRESH: BuiltinPatch = BuiltinPatch {
    id: "cuco.cp7pd.refresh",
    stage: "feedback",
};
pub(super) const DMAKER_STRAIGHT: BuiltinPatch = BuiltinPatch {
    id: "dmaker.fan.straight-wind",
    stage: "recognition and control/feedback",
};

/// This rule targets only the confirmed light service, never its ambient source.
pub(super) fn ls2307_target(model: &str, siid: u32) -> bool {
    model == ls2307::MODEL && siid == ls2307::POWER.siid
}

/// An identified cp7pd must not reinterpret its master switch as an outlet.
pub(super) fn cp7pd_identity(model: &str, urn: &Urn<'_>) -> (bool, bool) {
    let instance = urn.standard("device", "outlet", 0xa002) && urn.vendor == "cuco-cp7pd";
    let mentioned = model == outlet::CP7PD_MODEL || instance;
    let matched = instance && model == outlet::CP7PD_MODEL;
    (mentioned, matched)
}

pub(super) fn dmaker_identity(model: &str, urn: &Urn<'_>) -> bool {
    model.starts_with("dmaker.") && urn.vendor.starts_with("dmaker-")
}
