use super::library::{
    device::{
        CommandReceiver, Composition, ConfigurationScope, DeviceKey, DeviceRegistry, PhysicalDevice,
    },
    storage::{CachedScope, CompositionCommitError, DeviceStore},
};
use std::collections::HashSet;

/// Build the requested sources and stage through the real directory boundary.
pub fn stage_configuration(
    store: &DeviceStore,
    scope: &CachedScope,
    groups: &[Composition],
    removed: &[DeviceKey],
    standalone: &[DeviceKey],
) -> Result<(DeviceRegistry, Vec<CommandReceiver>), CompositionCommitError> {
    let registry = DeviceRegistry::new();
    let mut receivers = Vec::new();
    let sources = groups
        .iter()
        .flat_map(|group| &group.members)
        .chain(standalone)
        .collect::<HashSet<_>>();
    for key in sources {
        if let Some(stored) = store.load_published(key)? {
            let (device, commands) = PhysicalDevice::new(stored.description());
            registry.insert(device);
            receivers.extend(commands);
        }
    }
    registry
        .stage_configuration(
            ConfigurationScope {
                uid: scope.uid.clone(),
                home_id: scope.home_id.clone(),
                group_id: scope.group_id.clone(),
            },
            groups.to_vec(),
            removed.to_vec(),
        )
        .map_err(CompositionCommitError::Invalid)?;
    Ok((registry, receivers))
}
