use super::super::StateMapping;
use super::{BatteryBinding, CapabilityError, MeasurementUnit, ParsedFunction, PropertyBinding};
use crate::device::{DeviceKey, FunctionKind, PhysicalDeviceDescription};

/// Source recognition facts are persisted for explanation, but never decide routing.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecognitionMetadata {
    pub(crate) category: String,
    pub(crate) core: Vec<String>,
    pub(crate) patches: Vec<AppliedPatch>,
    pub(crate) diagnostics: Vec<RecognitionDiagnostic>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppliedPatch {
    pub(crate) id: String,
    pub(crate) target: String,
    pub(crate) stage: String,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecognitionDiagnostic {
    pub(crate) target: String,
    pub(crate) reason: String,
}

impl RecognitionMetadata {
    pub fn category(&self) -> &str {
        &self.category
    }
    pub fn core(&self) -> &[String] {
        &self.core
    }
    pub fn patches(&self) -> &[AppliedPatch] {
        &self.patches
    }
    pub fn diagnostics(&self) -> &[RecognitionDiagnostic] {
        &self.diagnostics
    }
}
impl AppliedPatch {
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn target(&self) -> &str {
        &self.target
    }
    pub fn stage(&self) -> &str {
        &self.stage
    }
}
impl RecognitionDiagnostic {
    pub fn target(&self) -> &str {
        &self.target
    }
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

/// A parsed device description with non-empty, uniquely keyed functions and
/// uniquely owned real bindings and source-anchored identities.
#[derive(Clone, Debug)]
pub struct ParsedDevice {
    key: DeviceKey,
    functions: Vec<ParsedFunction>,
    refresh_trigger: Option<PropertyBinding>,
    battery: Option<BatteryBinding>,
    battery_issue: Option<String>,
    recognition: Option<RecognitionMetadata>,
}
impl PartialEq for ParsedDevice {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
            && self.functions == other.functions
            && self.refresh_trigger == other.refresh_trigger
            && self.battery == other.battery
            && self.battery_issue == other.battery_issue
            && self.recognition == other.recognition
    }
}
impl Eq for ParsedDevice {}
impl ParsedDevice {
    #[cfg(test)]
    pub(crate) fn new(
        key: DeviceKey,
        functions: Vec<ParsedFunction>,
        refresh_trigger: Option<PropertyBinding>,
    ) -> Result<Self, CapabilityError> {
        Self::new_with_battery(key, functions, refresh_trigger, None)
    }
    #[cfg(test)]
    pub(crate) fn new_with_battery(
        key: DeviceKey,
        functions: Vec<ParsedFunction>,
        refresh_trigger: Option<PropertyBinding>,
        battery: Option<BatteryBinding>,
    ) -> Result<Self, CapabilityError> {
        Self::build(key, functions, refresh_trigger, battery, None)
    }
    pub(crate) fn new_with_battery_diagnostic(
        key: DeviceKey,
        functions: Vec<ParsedFunction>,
        refresh_trigger: Option<PropertyBinding>,
        battery: Option<BatteryBinding>,
        battery_issue: Option<String>,
    ) -> Result<Self, CapabilityError> {
        Self::build(key, functions, refresh_trigger, battery, battery_issue)
    }
    fn build(
        key: DeviceKey,
        functions: Vec<ParsedFunction>,
        refresh_trigger: Option<PropertyBinding>,
        battery: Option<BatteryBinding>,
        battery_issue: Option<String>,
    ) -> Result<Self, CapabilityError> {
        super::super::validate_source_identity(&key)?;
        if functions.is_empty() {
            return Err(CapabilityError::InvalidDescription("functions"));
        }
        if battery_issue
            .as_ref()
            .is_some_and(|issue| issue.is_empty() || issue.chars().any(char::is_control))
            || (battery.is_some() && battery_issue.is_some())
        {
            return Err(CapabilityError::InvalidDescription("battery diagnostic"));
        }
        let mut ids = std::collections::HashSet::new();
        if functions.iter().any(|function| !ids.insert(function.id())) {
            return Err(CapabilityError::InvalidDescription(
                "duplicate function key",
            ));
        }
        let mut bindings = std::collections::HashSet::new();
        for binding in functions
            .iter()
            .flat_map(ParsedFunction::bindings)
            .chain(refresh_trigger)
            .chain(battery.as_ref().map(BatteryBinding::property))
        {
            if binding.siid == 0 || binding.piid == 0 || !bindings.insert(binding) {
                return Err(CapabilityError::InvalidDescription("function bindings"));
            }
        }
        for function in &functions {
            if function.id() != function.source_id() {
                return Err(CapabilityError::InvalidDescription(
                    "function source identity",
                ));
            }
            if let Some(lighting) = function.lighting() {
                let service = function.power().unwrap().siid;
                if lighting
                    .brightness()
                    .is_some_and(|binding| binding.property().siid != service)
                    || lighting
                        .temperature()
                        .is_some_and(|binding| binding.property().siid != service)
                    || lighting.mode().is_none()
                        && lighting
                            .color()
                            .is_some_and(|binding| binding.property().siid != service)
                {
                    return Err(CapabilityError::InvalidDescription(
                        "light binding ownership",
                    ));
                }
            }
            if let Some(curtain) = function.curtain() {
                let service = curtain.motor().property().siid;
                if curtain
                    .bindings()
                    .iter()
                    .any(|binding| binding.siid != service)
                {
                    return Err(CapabilityError::InvalidDescription(
                        "curtain binding ownership",
                    ));
                }
            }
            if let Some(fan) = function.fan() {
                let service = fan.power().siid;
                if fan.swing().is_some_and(|binding| binding.siid != service)
                    || fan
                        .mode()
                        .is_some_and(|binding| binding.property().siid != service)
                    || fan.mode().is_none() && fan.speed().property().siid != service
                {
                    return Err(CapabilityError::InvalidDescription("fan binding ownership"));
                }
            }
            if let Some(measurement) = function.measurement()
                && !matches!(
                    (function.kind(), measurement.unit()),
                    (FunctionKind::Temperature(_), MeasurementUnit::Celsius)
                        | (FunctionKind::Humidity(_), MeasurementUnit::Percentage)
                        | (FunctionKind::CarbonDioxide(_), MeasurementUnit::Ppm)
                        | (FunctionKind::Illuminance(_), MeasurementUnit::Lux)
                )
            {
                return Err(CapabilityError::InvalidDescription("sensor topology"));
            }
            if let Some(state) = function.state()
                && !matches!(
                    (function.kind(), state.mapping()),
                    (
                        FunctionKind::Contact,
                        StateMapping::ContactBoolean | StateMapping::Enumerated { .. }
                    ) | (FunctionKind::Motion, StateMapping::NoMotionDuration { .. })
                        | (FunctionKind::Occupancy, StateMapping::Enumerated { .. })
                )
            {
                return Err(CapabilityError::InvalidDescription("state topology"));
            }
        }
        Ok(Self {
            key,
            functions,
            refresh_trigger,
            battery,
            battery_issue,
            recognition: None,
        })
    }
    pub(crate) fn with_recognition(
        mut self,
        metadata: RecognitionMetadata,
    ) -> Result<Self, CapabilityError> {
        let valid = |value: &str| !value.is_empty() && !value.chars().any(char::is_control);
        if !valid(&metadata.category)
            || metadata
                .core
                .iter()
                .any(|id| !self.functions.iter().any(|f| f.id() == id))
            || metadata.patches.iter().any(|patch| {
                !valid(&patch.id)
                    || !valid(&patch.stage)
                    || !self.functions.iter().any(|f| f.id() == patch.target)
            })
            || metadata
                .diagnostics
                .iter()
                .any(|issue| !valid(&issue.target) || !valid(&issue.reason))
        {
            return Err(CapabilityError::InvalidDescription("recognition metadata"));
        }
        self.recognition = Some(metadata);
        Ok(self)
    }
    pub fn recognition(&self) -> Option<&RecognitionMetadata> {
        self.recognition.as_ref()
    }
    pub fn key(&self) -> &DeviceKey {
        &self.key
    }
    pub fn functions(&self) -> &[ParsedFunction] {
        &self.functions
    }
    /// A group relation without a specific function can address only one light.
    pub(crate) fn unique_light(&self) -> Option<&ParsedFunction> {
        let mut lights = self
            .functions
            .iter()
            .filter(|function| function.lighting().is_some());
        let light = lights.next()?;
        lights.next().is_none().then_some(light)
    }
    pub fn refresh_trigger(&self) -> Option<PropertyBinding> {
        self.refresh_trigger
    }
    pub fn battery(&self) -> Option<&BatteryBinding> {
        self.battery.as_ref()
    }
    pub fn battery_issue(&self) -> Option<&str> {
        self.battery_issue.as_deref()
    }
    pub fn is_read_only(&self) -> bool {
        !self.functions.is_empty()
            && self
                .functions
                .iter()
                .all(|function| function.kind().is_read_only())
    }
    pub(crate) fn physical_description(
        &self,
        label: impl Into<String>,
    ) -> PhysicalDeviceDescription {
        let description = PhysicalDeviceDescription::new(
            self.key.clone(),
            label,
            self.functions
                .iter()
                .map(ParsedFunction::description)
                .collect(),
        )
        .expect("a parsed device keeps a valid description");
        if self.battery.is_some() {
            description.with_battery()
        } else {
            description
        }
    }
}
