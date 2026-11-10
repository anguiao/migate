//! A native device description validated once at the source boundary.

use super::{
    AppliedPatch, CapabilityError, ParsedDevice, SharedFanChannel, description::Urn, patch,
};
use crate::device::PhysicalDeviceDescription;
use crate::xiaomi::capability::PropertyBinding;
use crate::xiaomi::legacy_miio::Mapping;

/// A derived transport adaptation; its diagnostic patch is not persisted.
#[derive(Clone, Debug)]
pub(crate) struct LegacyProfile {
    patch: patch::BuiltinPatch,
    target: String,
    mapping: &'static Mapping,
}

impl LegacyProfile {
    pub(super) fn new(
        patch: patch::BuiltinPatch,
        target: impl Into<String>,
        mapping: &'static Mapping,
    ) -> Self {
        Self {
            patch,
            target: target.into(),
            mapping,
        }
    }

    pub(crate) fn patch(&self) -> AppliedPatch {
        self.patch.applied(&self.target)
    }

    pub(in crate::xiaomi) fn target(&self) -> &str {
        &self.target
    }

    pub(in crate::xiaomi) fn mapping(&self) -> &'static Mapping {
        self.mapping
    }
}

impl PartialEq for LegacyProfile {
    fn eq(&self, other: &Self) -> bool {
        self.target == other.target && self.mapping == other.mapping
    }
}
impl Eq for LegacyProfile {}

/// One device read demand: a real MIoT property or the shared legacy group.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum ReadInput {
    Property(PropertyBinding),
    SharedFeedback,
}
impl ReadInput {
    pub(crate) fn property(self) -> Option<PropertyBinding> {
        match self {
            Self::Property(binding) => Some(binding),
            Self::SharedFeedback => None,
        }
    }
}

pub(crate) const SHARED_FEEDBACK_CHANNEL: &str = "legacy:shared";

/// A real standard property retained only as a native-mode diagnostic input.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StandardModePlan {
    pub(crate) function: String,
    pub(crate) binding: PropertyBinding,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SharedFanConsumer {
    pub(crate) function: String,
    pub(crate) channel: SharedFanChannel,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RelatedScope {
    All,
    Temperature,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub(crate) struct RelatedInput {
    pub(crate) function: String,
    pub(crate) binding: PropertyBinding,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct RelatedRefreshPlan {
    all: Vec<RelatedInput>,
    temperature: Vec<RelatedInput>,
    shared: bool,
}
impl RelatedRefreshPlan {
    pub(crate) fn inputs(&self, scope: RelatedScope) -> &[RelatedInput] {
        match scope {
            RelatedScope::All => &self.all,
            RelatedScope::Temperature => &self.temperature,
        }
    }
    pub(crate) fn includes_shared(&self) -> bool {
        self.shared
    }
}

/// One physical shared legacy feedback group consumed by several functions.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SharedFeedbackPlan {
    pub(crate) fans: Vec<SharedFanConsumer>,
    pub(crate) heater_temperature: Option<String>,
}
/// Transport-facing facts derived from validated native bindings; they never
/// become synthetic properties or model checks in the device/Matter layers.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct AdaptationPlan {
    standard_mode: Option<StandardModePlan>,
    shared_feedback: Option<SharedFeedbackPlan>,
    related: Option<RelatedRefreshPlan>,
}
impl AdaptationPlan {
    fn from_device(device: &ParsedDevice) -> Self {
        let standard_mode = device.functions().iter().find_map(|function| {
            function.heater().and_then(|heater| {
                heater.diagnostic_mode().map(|binding| StandardModePlan {
                    function: function.id().to_owned(),
                    binding,
                })
            })
        });
        let fans = device
            .functions()
            .iter()
            .filter_map(|function| {
                function
                    .fan()
                    .and_then(|fan| fan.shared_speed())
                    .map(|speed| SharedFanConsumer {
                        function: function.id().to_owned(),
                        channel: speed.channel,
                    })
            })
            .collect::<Vec<_>>();
        let shared_target = patch::bhf::shared_feedback_target(device);
        let shared_feedback =
            (shared_target.is_some() || !fans.is_empty()).then(|| SharedFeedbackPlan {
                fans,
                heater_temperature: shared_target.map(str::to_owned),
            });
        let related_enabled = shared_feedback.is_some() || standard_mode.is_some();
        let mut all = Vec::new();
        let mut temperature = Vec::new();
        if related_enabled && let Some(shared) = &shared_feedback {
            for consumer in &shared.fans {
                if let Some(function) = device
                    .functions()
                    .iter()
                    .find(|function| function.id() == consumer.function)
                    && let Some(fan) = function.fan()
                {
                    all.push(RelatedInput {
                        function: consumer.function.clone(),
                        binding: fan.power(),
                    });
                }
            }
        }
        for function in device.functions().iter().filter(|_| related_enabled) {
            if let Some(heater) = function.heater() {
                let power = RelatedInput {
                    function: function.id().to_owned(),
                    binding: heater.power(),
                };
                let target = RelatedInput {
                    function: function.id().to_owned(),
                    binding: heater.temperature().property(),
                };
                all.push(power);
                all.push(target.clone());
                temperature.push(target);
            }
        }
        if related_enabled && let Some(mode) = &standard_mode {
            all.push(RelatedInput {
                function: mode.function.clone(),
                binding: mode.binding,
            });
        }
        all.sort();
        all.dedup();
        temperature.sort();
        temperature.dedup();
        let related = related_enabled.then(|| RelatedRefreshPlan {
            all,
            temperature,
            shared: shared_feedback.is_some(),
        });
        Self {
            standard_mode,
            shared_feedback,
            related,
        }
    }
    pub(crate) fn standard_mode(&self) -> Option<&StandardModePlan> {
        self.standard_mode.as_ref()
    }
    pub(crate) fn shared_feedback(&self) -> Option<&SharedFeedbackPlan> {
        self.shared_feedback.as_ref()
    }
    pub(crate) fn related(&self) -> Option<&RelatedRefreshPlan> {
        self.related.as_ref()
    }
    /// Every v13 input shares the existing short polling interval.
    pub(crate) fn fast_poll(&self) -> bool {
        self.standard_mode.is_some() || self.shared_feedback.is_some()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NativeDescription {
    model: String,
    instance_type: String,
    device: ParsedDevice,
    legacy_profile: Option<LegacyProfile>,
    plan: AdaptationPlan,
}

impl NativeDescription {
    pub(crate) fn new(
        model: String,
        instance_type: String,
        device: ParsedDevice,
    ) -> Result<Self, CapabilityError> {
        if [&model, &instance_type]
            .iter()
            .any(|value| value.is_empty() || value.chars().any(char::is_control))
        {
            return Err(CapabilityError::InvalidDescription(
                "native model or instance type",
            ));
        }
        let urn = Urn::parse(&instance_type)
            .map_err(|_| CapabilityError::InvalidDescription("native instance type"))?;
        if urn.entity != "device" {
            return Err(CapabilityError::InvalidDescription("native instance type"));
        }
        patch::validate_native(&device, &model, &urn)
            .map_err(CapabilityError::InvalidDescription)?;
        let legacy_profile = patch::mcn02::profile(&model, &device);
        let plan = AdaptationPlan::from_device(&device);
        Ok(Self {
            model,
            instance_type,
            device,
            legacy_profile,
            plan,
        })
    }

    pub(crate) fn model(&self) -> &str {
        &self.model
    }
    pub(crate) fn instance_type(&self) -> &str {
        &self.instance_type
    }
    pub(crate) fn device(&self) -> &ParsedDevice {
        &self.device
    }

    pub(crate) fn legacy_profile(&self) -> Option<&LegacyProfile> {
        self.legacy_profile.as_ref()
    }

    pub(crate) fn plan(&self) -> &AdaptationPlan {
        &self.plan
    }

    pub(crate) fn physical_description(
        &self,
        label: impl Into<String>,
    ) -> PhysicalDeviceDescription {
        self.device.physical_description(label)
    }
}
