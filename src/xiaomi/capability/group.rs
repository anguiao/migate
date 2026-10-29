//! Native Xiaomi light-group capability parsing.
//!
//! Groups are identified by confirmed catalog membership, never by model or
//! name. The group's own MIoT instance is parsed exactly like a light
//! instance so only real, controllable native bindings become capabilities;
//! member capabilities and RGB repair stay outside this boundary.

use super::{CapabilityError, ParsedDevice, description::Description, parse_device_for_model};
use crate::device::DeviceKey;

/// Parse a native group's own MIoT instance into a standard light profile.
/// The instance must be a standard light device; the mode property alone never
/// creates a color capability.
pub(crate) fn parse_group(
    key: DeviceKey,
    model: &str,
    instance: &serde_json::Value,
) -> Result<ParsedDevice, CapabilityError> {
    let description = Description::parse(instance)?;
    if !description.urn.standard("device", "light", 0x0000a001) {
        return Err(CapabilityError::InvalidGroup(
            "group instance is not a standard light device",
        ));
    }
    let device = parse_device_for_model(key, model, instance)?;
    if device.unique_light().is_none() {
        return Err(CapabilityError::InvalidGroup(
            "group light is missing or ambiguous",
        ));
    }
    Ok(device)
}
