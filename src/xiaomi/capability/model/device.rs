use super::super::StateMapping;
use super::{BatteryBinding, CapabilityError, MeasurementUnit, ParsedFunction, PropertyBinding};
use crate::device::{DeviceKey, FunctionKind, PhysicalDeviceDescription};

/// A parsed device description with non-empty, uniquely keyed functions and
/// uniquely owned real bindings. Lights use the single `main` function layout.
#[derive(Clone, Debug)]
pub struct ParsedDevice {
    key: DeviceKey,
    functions: Vec<ParsedFunction>,
    refresh_trigger: Option<PropertyBinding>,
    battery: Option<BatteryBinding>,
    battery_issue: Option<String>,
}
impl PartialEq for ParsedDevice {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
            && self.functions == other.functions
            && self.refresh_trigger == other.refresh_trigger
            && self.battery == other.battery
    }
}
impl Eq for ParsedDevice {}
impl ParsedDevice {
    pub(crate) fn new(
        key: DeviceKey,
        functions: Vec<ParsedFunction>,
        refresh_trigger: Option<PropertyBinding>,
    ) -> Result<Self, CapabilityError> {
        Self::new_with_battery(key, functions, refresh_trigger, None)
    }
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
        let controls = functions
            .iter()
            .filter(|function| function.kind().is_control())
            .collect::<Vec<_>>();
        let measurements = functions
            .iter()
            .filter(|function| function.measurement().is_some())
            .collect::<Vec<_>>();
        for function in &measurements {
            let measurement = function.measurement().unwrap();
            if !matches!(
                (function.id(), function.kind(), measurement.unit()),
                (
                    "temperature",
                    FunctionKind::Temperature(_),
                    MeasurementUnit::Celsius
                ) | (
                    "humidity",
                    FunctionKind::Humidity(_),
                    MeasurementUnit::Percentage
                ) | ("co2", FunctionKind::CarbonDioxide(_), MeasurementUnit::Ppm)
                    | (
                        "illuminance",
                        FunctionKind::Illuminance(_),
                        MeasurementUnit::Lux
                    )
            ) {
                return Err(CapabilityError::InvalidDescription("sensor topology"));
            }
        }
        for function in functions
            .iter()
            .filter(|function| function.kind().is_state_sensor())
        {
            let valid = matches!(
                (
                    function.id(),
                    function.kind(),
                    function.state().unwrap().mapping(),
                ),
                (
                    "contact",
                    FunctionKind::Contact,
                    StateMapping::ContactBoolean | StateMapping::Enumerated { .. }
                ) | (
                    "motion",
                    FunctionKind::Motion,
                    StateMapping::NoMotionDuration { .. }
                ) | (
                    "occupancy",
                    FunctionKind::Occupancy,
                    StateMapping::Enumerated { .. }
                )
            );
            if !valid {
                return Err(CapabilityError::InvalidDescription("state topology"));
            }
        }
        if functions
            .iter()
            .all(|function| function.kind().is_control())
            && battery.is_some()
        {
            return Err(CapabilityError::InvalidDescription("battery topology"));
        }
        if controls
            .iter()
            .any(|function| function.lighting().is_some())
        {
            if controls.len() != 1 || controls[0].id() != "main" || refresh_trigger.is_some() {
                return Err(CapabilityError::InvalidDescription("light topology"));
            }
        } else if !controls.is_empty() {
            let ids = controls
                .iter()
                .map(|function| function.id())
                .collect::<Vec<_>>();
            let single_main = ids == ["main"] && refresh_trigger.is_none();
            let cp7pd = ids == ["outlet_1", "outlet_2", "usb"] && refresh_trigger.is_some();
            if !(single_main || cp7pd) {
                return Err(CapabilityError::InvalidDescription("outlet topology"));
            }
        } else if refresh_trigger.is_some() {
            return Err(CapabilityError::InvalidDescription("sensor topology"));
        }
        Ok(Self {
            key,
            functions,
            refresh_trigger,
            battery,
            battery_issue,
        })
    }
    pub fn key(&self) -> &DeviceKey {
        &self.key
    }
    pub fn functions(&self) -> &[ParsedFunction] {
        &self.functions
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
