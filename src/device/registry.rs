use super::{Category, Composition, DeviceKey, PhysicalDevice};
use event_listener::Event;
use std::{
    collections::HashSet,
    fmt,
    sync::{Arc, Mutex},
};

/// The whole visible composition relation set and its own revision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompositionSnapshot {
    pub revision: u64,
    /// Storage-confirmed relation version, when published from storage.
    pub confirmed: Option<u64>,
    pub groups: Vec<Composition>,
}

#[derive(Clone)]
pub struct RegistrySnapshot {
    pub revision: u64,
    pub devices: Vec<PhysicalDevice>,
    pub composition: CompositionSnapshot,
}

/// A rejected composition update. Rejection leaves the registry unchanged.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CompositionError {
    EmptyGroup(DeviceKey),
    DuplicateGroup(DeviceKey),
    GroupKeyConflict(DeviceKey),
    MissingMember(DeviceKey),
    UnsupportedMember(DeviceKey),
    SelfMember(DeviceKey),
    DuplicateMember { group: DeviceKey, member: DeviceKey },
    OverlappingMember(DeviceKey),
    NestedGroup(DeviceKey),
}

impl fmt::Display for CompositionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyGroup(key) => write!(formatter, "composition {} has no members", key.id),
            Self::DuplicateGroup(key) => write!(formatter, "composition {} is repeated", key.id),
            Self::GroupKeyConflict(key) => {
                write!(formatter, "composition {} collides with a device", key.id)
            }
            Self::MissingMember(key) => {
                write!(formatter, "composition member {} does not exist", key.id)
            }
            Self::UnsupportedMember(key) => write!(
                formatter,
                "composition member {} is not a supported single light",
                key.id
            ),
            Self::SelfMember(key) => write!(formatter, "composition {} contains itself", key.id),
            Self::DuplicateMember { group, member } => write!(
                formatter,
                "composition {} repeats member {}",
                group.id, member.id
            ),
            Self::OverlappingMember(key) => write!(
                formatter,
                "composition member {} appears in multiple groups",
                key.id
            ),
            Self::NestedGroup(key) => write!(
                formatter,
                "composition {} is nested in another group",
                key.id
            ),
        }
    }
}

impl std::error::Error for CompositionError {}

#[derive(Clone)]
pub struct DeviceRegistry(Arc<Inner>);
struct Inner {
    snapshot: Mutex<RegistrySnapshot>,
    changed: Event,
}
impl Default for DeviceRegistry {
    fn default() -> Self {
        Self::new()
    }
}
impl DeviceRegistry {
    pub fn new() -> Self {
        Self(Arc::new(Inner {
            snapshot: Mutex::new(RegistrySnapshot {
                revision: 0,
                devices: Vec::new(),
                composition: CompositionSnapshot {
                    revision: 0,
                    confirmed: None,
                    groups: Vec::new(),
                },
            }),
            changed: Event::new(),
        }))
    }
    pub fn get(&self, key: &DeviceKey) -> Option<PhysicalDevice> {
        self.0
            .snapshot
            .lock()
            .unwrap()
            .devices
            .iter()
            .find(|device| device.key() == key)
            .cloned()
    }
    pub fn list(&self) -> Vec<PhysicalDevice> {
        self.0.snapshot.lock().unwrap().devices.clone()
    }
    pub fn snapshot(&self) -> RegistrySnapshot {
        self.0.snapshot.lock().unwrap().clone()
    }
    pub fn insert(&self, device: PhysicalDevice) {
        let mut state = self.0.snapshot.lock().unwrap();
        if state
            .devices
            .iter()
            .any(|present| present.key() == device.key())
            || state
                .composition
                .groups
                .iter()
                .any(|group| &group.key == device.key())
        {
            return;
        }
        state.devices.push(device);
        state.revision = state.revision.wrapping_add(1);
        self.0.changed.notify(usize::MAX);
    }
    /// Atomically substitute a new physical instance under an existing key.
    /// The published composition must stay valid for the replacement; invalid
    /// replacements leave the registry untouched. A same-instance replacement
    /// is a no-op and never closes the installed device.
    pub fn replace(
        &self,
        device: PhysicalDevice,
    ) -> Result<Option<PhysicalDevice>, CompositionError> {
        let mut state = self.0.snapshot.lock().unwrap();
        let Some(index) = state
            .devices
            .iter()
            .position(|present| present.key() == device.key())
        else {
            if state
                .composition
                .groups
                .iter()
                .any(|group| &group.key == device.key())
            {
                return Err(CompositionError::GroupKeyConflict(device.key().clone()));
            }
            state.devices.push(device);
            state.revision = state.revision.wrapping_add(1);
            self.0.changed.notify(usize::MAX);
            return Ok(None);
        };
        if state.devices[index].same_instance(&device) {
            return Ok(None);
        }
        let mut surviving = state.devices.clone();
        surviving[index] = device.clone();
        validate_compositions(&state.composition.groups, &surviving)?;
        let previous = std::mem::replace(&mut state.devices[index], device);
        previous.close();
        state.revision = state.revision.wrapping_add(1);
        self.0.changed.notify(usize::MAX);
        Ok(Some(previous))
    }
    pub fn remove(&self, key: &DeviceKey) -> Option<PhysicalDevice> {
        let mut state = self.0.snapshot.lock().unwrap();
        let index = state
            .devices
            .iter()
            .position(|device| device.key() == key)?;
        let removed = state.devices.remove(index);
        removed.close();
        let groups = state.composition.groups.len();
        state
            .composition
            .groups
            .retain(|group| &group.key != key && !group.members.contains(key));
        if state.composition.groups.len() != groups {
            state.composition.revision = state.composition.revision.wrapping_add(1);
            // The stored confirmation no longer describes the visible relations.
            state.composition.confirmed = None;
        }
        state.revision = state.revision.wrapping_add(1);
        self.0.changed.notify(usize::MAX);
        Some(removed)
    }
    /// Atomically replace the whole relation set and remove confirmed-deleted
    /// members. A confirmed deletion revokes any group that references it; the
    /// caller must submit the complete surviving relation set. Invalid updates
    /// leave the registry unchanged.
    pub fn update_composition(
        &self,
        compositions: Vec<Composition>,
        removed: &[DeviceKey],
    ) -> Result<(), CompositionError> {
        self.update_composition_inner(None, compositions, removed)
    }

    /// Publish a relation set together with the storage-confirmed version so
    /// the same confirmation stays identifiable in the snapshot.
    pub fn update_composition_versioned(
        &self,
        version: u64,
        compositions: Vec<Composition>,
        removed: &[DeviceKey],
    ) -> Result<(), CompositionError> {
        self.update_composition_inner(Some(version), compositions, removed)
    }

    fn update_composition_inner(
        &self,
        confirmed: Option<u64>,
        compositions: Vec<Composition>,
        removed: &[DeviceKey],
    ) -> Result<(), CompositionError> {
        let mut state = self.0.snapshot.lock().unwrap();
        let surviving = state
            .devices
            .iter()
            .filter(|device| !removed.contains(device.key()))
            .cloned()
            .collect::<Vec<_>>();
        validate_compositions(&compositions, &surviving)?;
        let compositions = normalize_compositions(compositions);
        let devices_changed = removed
            .iter()
            .any(|key| state.devices.iter().any(|device| device.key() == key));
        let composition_changed = compositions != state.composition.groups;
        let confirmed_changed = state.composition.confirmed != confirmed;
        if !devices_changed && !composition_changed && !confirmed_changed {
            return Ok(());
        }
        if devices_changed {
            for key in removed {
                if let Some(index) = state.devices.iter().position(|device| device.key() == key) {
                    state.devices.remove(index).close();
                }
            }
        }
        if composition_changed {
            state.composition.groups = compositions;
            state.composition.revision = state.composition.revision.wrapping_add(1);
        }
        state.composition.confirmed = confirmed;
        state.revision = state.revision.wrapping_add(1);
        self.0.changed.notify(usize::MAX);
        Ok(())
    }
    pub fn subscribe(&self) -> RegistryChanges {
        RegistryChanges {
            registry: self.clone(),
            revision: self.0.snapshot.lock().unwrap().revision,
        }
    }
}

fn validate_compositions(
    compositions: &[Composition],
    devices: &[PhysicalDevice],
) -> Result<(), CompositionError> {
    validate_composition_structure(compositions)?;
    if let Some(group) = compositions
        .iter()
        .find(|group| devices.iter().any(|device| device.key() == &group.key))
    {
        return Err(CompositionError::GroupKeyConflict(group.key.clone()));
    }
    for group in compositions {
        for member in &group.members {
            let Some(device) = devices.iter().find(|device| device.key() == member) else {
                return Err(CompositionError::MissingMember(member.clone()));
            };
            let functions = device.functions();
            let supported = functions.len() == 1 && {
                let function = functions[0].description();
                function.id == "main"
                    && function.category == Category::Light
                    && function.valid_capabilities()
            };
            if !supported {
                return Err(CompositionError::UnsupportedMember(member.clone()));
            }
        }
    }
    Ok(())
}

/// Validate a proposed relation set's structure independent of device presence.
/// Device-dependent checks (key collisions, member support) remain at publication.
pub(crate) fn validate_composition_structure(
    compositions: &[Composition],
) -> Result<(), CompositionError> {
    let mut group_keys = HashSet::new();
    for group in compositions {
        if group.members.is_empty() {
            return Err(CompositionError::EmptyGroup(group.key.clone()));
        }
        if !group_keys.insert(group.key.clone()) {
            return Err(CompositionError::DuplicateGroup(group.key.clone()));
        }
    }
    let mut seen = HashSet::new();
    for group in compositions {
        let mut members = HashSet::new();
        for member in &group.members {
            if member == &group.key {
                return Err(CompositionError::SelfMember(member.clone()));
            }
            if group_keys.contains(member) {
                return Err(CompositionError::NestedGroup(member.clone()));
            }
            if !members.insert(member.clone()) {
                return Err(CompositionError::DuplicateMember {
                    group: group.key.clone(),
                    member: member.clone(),
                });
            }
            if !seen.insert(member.clone()) {
                return Err(CompositionError::OverlappingMember(member.clone()));
            }
        }
    }
    Ok(())
}

fn normalize_compositions(mut compositions: Vec<Composition>) -> Vec<Composition> {
    let order = |left: &DeviceKey, right: &DeviceKey| {
        (left.source.as_str(), left.id.as_str()).cmp(&(right.source.as_str(), right.id.as_str()))
    };
    for group in &mut compositions {
        group.members.sort_by(order);
    }
    compositions.sort_by(|left, right| order(&left.key, &right.key));
    compositions
}
pub struct RegistryChanges {
    registry: DeviceRegistry,
    revision: u64,
}
impl RegistryChanges {
    pub async fn changed(&mut self) -> RegistrySnapshot {
        loop {
            let listener = self.registry.0.changed.listen();
            let snapshot = self.registry.0.snapshot.lock().unwrap().clone();
            if snapshot.revision != self.revision {
                self.revision = snapshot.revision;
                return snapshot;
            }
            listener.await;
        }
    }
}
