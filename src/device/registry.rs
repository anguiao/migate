use super::group::Group;
use super::{
    Category, Composition, ConfigurationScope, DeviceKey, Function, GroupConfiguration,
    GroupDescription, GroupSnapshot, PhysicalDevice, derive_group_capabilities,
};
use event_listener::Event;
use std::{
    collections::HashSet,
    fmt,
    sync::{Arc, Mutex},
};

/// Latest confirmed relation set waiting for the configuration apply boundary. Pending
/// relations never affect the applied composition, group command snapshots or
/// aggregates; the storage commit and applied update happen together later.
#[derive(Clone)]
pub struct PendingConfiguration {
    /// The account/home this candidate belongs to. The generic
    /// Store/DeviceRegistry apply path commits without source context.
    pub scope: ConfigurationScope,
    /// Complete candidate relation set with its derived shape.
    pub groups: Vec<GroupConfiguration>,
    /// Confirmed deletions not yet part of the applied version.
    pub removed: Vec<DeviceKey>,
    /// Standalone physical devices in the candidate publication set. A new
    /// admission must form a candidate even when group relations do not change.
    pub devices: Vec<PhysicalDevice>,
    /// Member device instances the candidate groups bind to, aligned with
    /// `groups`. A same-key replacement forms a candidate for the next rebuild.
    instances: Vec<Vec<PhysicalDevice>>,
}

impl fmt::Debug for PendingConfiguration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PendingConfiguration")
            .field("scope", &self.scope)
            .field("groups", &self.groups)
            .field("removed", &self.removed)
            .field(
                "devices",
                &self
                    .devices
                    .iter()
                    .map(PhysicalDevice::key)
                    .collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

/// One applied group: the committed relation and derived capability shape plus
/// a stable aggregate handle. The handle survives membership updates so the
/// process-local all-off brightness memory is preserved.
#[derive(Clone)]
pub struct AppliedGroup {
    pub key: DeviceKey,
    pub label: String,
    pub members: Vec<DeviceKey>,
    handle: Group,
    /// The member device instances the handle currently binds. A same-key
    /// replacement must rebind even when keys, members and caps are equal.
    instances: Vec<PhysicalDevice>,
}

impl AppliedGroup {
    fn composition(&self) -> Composition {
        Composition {
            key: self.key.clone(),
            label: self.label.clone(),
            members: self.members.clone(),
        }
    }

    pub fn description(&self) -> GroupDescription {
        self.handle.description()
    }

    /// Aggregate state of the applied member set. Members whose control path
    /// was revoked keep their old handle and only report unavailable/unknown.
    pub fn snapshot(&self) -> GroupSnapshot {
        self.handle.snapshot()
    }

    pub fn handle(&self) -> &Group {
        &self.handle
    }

    pub fn functions(&self) -> Vec<Function> {
        self.handle.functions()
    }
}

impl fmt::Debug for AppliedGroup {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AppliedGroup")
            .field("key", &self.key)
            .field("label", &self.label)
            .field("members", &self.members)
            .finish_non_exhaustive()
    }
}

/// The applied publication set: visible group endpoints and standalone
/// physical device handles. [`RegistrySnapshot::devices`] stays the real source
/// list; this is what a publication consumer may expose.
#[derive(Clone, Default)]
pub struct AppliedConfiguration {
    /// Storage-confirmed version of the applied publication set.
    pub confirmed: Option<u64>,
    pub groups: Vec<AppliedGroup>,
    pub devices: Vec<PhysicalDevice>,
}

impl fmt::Debug for AppliedConfiguration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AppliedConfiguration")
            .field("confirmed", &self.confirmed)
            .field("groups", &self.groups)
            .field(
                "devices",
                &self
                    .devices
                    .iter()
                    .map(PhysicalDevice::key)
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

#[derive(Clone)]
pub struct RegistrySnapshot {
    pub revision: u64,
    /// Real source devices currently holding control paths.
    pub devices: Vec<PhysicalDevice>,
    /// Applied publication set sharing stable aggregate handles.
    pub applied: AppliedConfiguration,
    /// Latest confirmed configuration staged for the apply boundary.
    pub pending: Option<PendingConfiguration>,
}

/// A rejected configuration update. Rejection leaves the registry unchanged.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CompositionError {
    EmptyGroup(DeviceKey),
    DuplicateGroup(DeviceKey),
    GroupKeyConflict(DeviceKey),
    MissingMember(DeviceKey),
    UnsupportedMember(DeviceKey),
    SelfMember(DeviceKey),
    DuplicateMember { group: DeviceKey, member: DeviceKey },
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
    apply_error: Mutex<Option<String>>,
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
                applied: AppliedConfiguration::default(),
                pending: None,
            }),
            changed: Event::new(),
            apply_error: Mutex::new(None),
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
                .applied
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
    /// Close and detach a source device immediately, leaving the applied
    /// relations and group handles untouched. An applied group keeps the old
    /// handle, which can only report unavailable/unknown afterwards.
    pub fn revoke(&self, key: &DeviceKey) -> Option<PhysicalDevice> {
        let mut state = self.0.snapshot.lock().unwrap();
        let index = state
            .devices
            .iter()
            .position(|device| device.key() == key)?;
        let removed = state.devices.remove(index);
        removed.close();
        state.revision = state.revision.wrapping_add(1);
        self.0.changed.notify(usize::MAX);
        Some(removed)
    }
    pub fn remove(&self, key: &DeviceKey) -> Option<PhysicalDevice> {
        let mut state = self.0.snapshot.lock().unwrap();
        let index = state
            .devices
            .iter()
            .position(|device| device.key() == key)?;
        let removed = state.devices.remove(index);
        removed.close();
        let groups = state.applied.groups.len();
        state
            .applied
            .groups
            .retain(|group| &group.key != key && !group.members.contains(key));
        state.applied.devices.retain(|device| device.key() != key);
        if state.applied.groups.len() != groups {
            // The stored confirmation no longer describes the visible relations.
            state.applied.confirmed = None;
        }
        state.pending = None;
        state.revision = state.revision.wrapping_add(1);
        self.0.changed.notify(usize::MAX);
        Some(removed)
    }

    /// Stage the latest confirmed relation set for the configuration apply boundary.
    /// The applied relations, group handles and confirmed version stay
    /// unchanged. Removals accumulate across consecutive confirmations so a
    /// later apply cannot resurrect a deleted member.
    pub fn stage_configuration(
        &self,
        scope: ConfigurationScope,
        compositions: Vec<Composition>,
        removed: Vec<DeviceKey>,
    ) -> Result<(), CompositionError> {
        let mut state = self.0.snapshot.lock().unwrap();
        let mut removed = removed;
        if let Some(pending) = &state.pending {
            // A deletion stays pending only while the key is still absent. A
            // later re-admission of the same key clears the stale removal.
            for key in &pending.removed {
                if !state.devices.iter().any(|device| device.key() == key)
                    && !compositions.iter().any(|group| &group.key == key)
                {
                    removed.push(key.clone());
                }
            }
        }
        removed.sort_by(key_order);
        removed.dedup();
        let compositions = normalize_compositions(compositions);
        let surviving = state
            .devices
            .iter()
            .filter(|device| !removed.contains(device.key()))
            .cloned()
            .collect::<Vec<_>>();
        let resolved = resolve_compositions(&compositions, &surviving)?;
        let (groups, instances): (Vec<_>, Vec<_>) = compositions
            .into_iter()
            .zip(resolved)
            .map(|(group, instances)| {
                let label = state
                    .applied
                    .groups
                    .iter()
                    .find(|present| present.key == group.key)
                    .map_or(group.label, |present| present.label.clone());
                (
                    GroupConfiguration {
                        key: group.key,
                        label,
                        members: group.members,
                        capabilities: derive_group_capabilities(
                            instances
                                .iter()
                                .map(|device| device.functions()[0].description()),
                        ),
                    },
                    instances,
                )
            })
            .unzip();
        // Standalone devices are every surviving source that is not hidden
        // behind one of the candidate groups.
        let members = groups
            .iter()
            .flat_map(|group| group.members.iter())
            .collect::<HashSet<_>>();
        let mut devices = surviving
            .iter()
            .filter(|device| !members.contains(device.key()))
            .cloned()
            .collect::<Vec<_>>();
        devices.sort_by(|left, right| key_order(left.key(), right.key()));
        let unchanged = state.applied.confirmed.is_some()
            && removed.is_empty()
            && groups_match_applied(&groups, &instances, &state.applied.groups)
            && same_instances(&devices, &state.applied.devices);
        let pending = (!unchanged).then_some(PendingConfiguration {
            scope,
            groups,
            removed,
            devices,
            instances,
        });
        if same_pending(state.pending.as_ref(), pending.as_ref()) {
            return Ok(());
        }
        match (&state.pending, &pending) {
            (_, Some(candidate)) => log::info!(
                "Directory configuration confirmed: {} group(s), {} standalone device(s), {} removal(s); candidate staged for application",
                candidate.groups.len(),
                candidate.devices.len(),
                candidate.removed.len()
            ),
            (Some(_), None) => log::debug!(
                "Directory configuration returned to the applied set; no candidate pending"
            ),
            (None, None) => {}
        }
        state.pending = pending;
        state.revision = state.revision.wrapping_add(1);
        self.0.changed.notify(usize::MAX);
        Ok(())
    }

    /// The latest confirmed relation set, pending first. Used by the source
    /// reconciler as its retention baseline; never an applied view.
    pub fn desired_composition(&self) -> Vec<Composition> {
        let state = self.0.snapshot.lock().unwrap();
        match &state.pending {
            Some(pending) => pending
                .groups
                .iter()
                .map(|group| Composition {
                    key: group.key.clone(),
                    label: group.label.clone(),
                    members: group.members.clone(),
                })
                .collect(),
            None => state
                .applied
                .groups
                .iter()
                .map(AppliedGroup::composition)
                .collect(),
        }
    }

    pub fn pending_configuration(&self) -> Option<PendingConfiguration> {
        self.0.snapshot.lock().unwrap().pending.clone()
    }

    /// Record the latest failed configuration apply for
    /// diagnostics. This deliberately does not touch the snapshot revision or
    /// wake registry observers: a persistent failure must not spin the apply
    /// loop or flood the log.
    pub fn set_apply_error(&self, message: impl Into<String>) {
        *self.0.apply_error.lock().unwrap() = Some(message.into());
    }

    /// Clear the recorded apply failure after a successful configuration
    /// commit (or when no candidate is pending anymore).
    pub fn clear_apply_error(&self) {
        *self.0.apply_error.lock().unwrap() = None;
    }

    /// The latest failed configuration apply, for terminal diagnostics.
    pub fn apply_error(&self) -> Option<String> {
        self.0.apply_error.lock().unwrap().clone()
    }

    /// Apply the latest candidate synchronously while source and directory
    /// changes are excluded. The callback must not re-enter the registry.
    /// Storage succeeds before the infallible applied-view update.
    pub(crate) fn apply_pending<T, E>(
        &self,
        commit: impl FnOnce(&mut PendingConfiguration) -> Result<(T, u64), E>,
    ) -> Result<Option<T>, E> {
        let mut state = self.0.snapshot.lock().unwrap();
        let Some(mut pending) = state.pending.clone() else {
            return Ok(None);
        };
        let (result, version) = commit(&mut pending)?;
        apply_configuration(
            &mut state,
            version,
            pending.groups,
            pending.instances,
            pending.devices,
            &pending.removed,
        );
        self.0.changed.notify(usize::MAX);
        Ok(Some(result))
    }

    /// Restore a stored configuration at the runtime boundary. Validate all
    /// member bindings before changing the applied view.
    pub fn apply_configuration(
        &self,
        version: u64,
        groups: Vec<GroupConfiguration>,
        devices: &[DeviceKey],
        removed: &[DeviceKey],
    ) -> Result<(), CompositionError> {
        let mut state = self.0.snapshot.lock().unwrap();
        let groups = normalize_configurations(groups);
        let compositions = groups
            .iter()
            .map(|group| Composition {
                key: group.key.clone(),
                label: group.label.clone(),
                members: group.members.clone(),
            })
            .collect::<Vec<_>>();
        let surviving = state
            .devices
            .iter()
            .filter(|device| !removed.contains(device.key()))
            .cloned()
            .collect::<Vec<_>>();
        let instances = resolve_compositions(&compositions, &surviving)?;
        let mut devices = devices
            .iter()
            .map(|key| {
                surviving
                    .iter()
                    .find(|device| device.key() == key)
                    .cloned()
                    .ok_or_else(|| CompositionError::MissingMember(key.clone()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        devices.sort_by(|left, right| key_order(left.key(), right.key()));
        if removed.is_empty()
            && groups_match_applied(&groups, &instances, &state.applied.groups)
            && same_instances(&devices, &state.applied.devices)
            && state.applied.confirmed == Some(version)
            && state.pending.is_none()
        {
            return Ok(());
        }
        apply_configuration(&mut state, version, groups, instances, devices, removed);
        self.0.changed.notify(usize::MAX);
        Ok(())
    }

    pub fn subscribe(&self) -> RegistryChanges {
        RegistryChanges {
            registry: self.clone(),
            revision: self.0.snapshot.lock().unwrap().revision,
        }
    }

    /// Update the saved label of one applied composition. The relation set and
    /// its confirmed version stay unchanged; the revision wakes snapshot
    /// observers so runtime readers do not keep a stale name.
    pub fn set_composition_label(&self, key: &DeviceKey, label: impl Into<String>) -> bool {
        let mut state = self.0.snapshot.lock().unwrap();
        let label = label.into();
        let Some(group) = state
            .applied
            .groups
            .iter_mut()
            .find(|group| &group.key == key)
        else {
            return false;
        };
        if group.label == label {
            return true;
        }
        group.label = label.clone();
        group.handle.set_label(label);
        state.revision = state.revision.wrapping_add(1);
        self.0.changed.notify(usize::MAX);
        true
    }
}

fn apply_configuration(
    state: &mut RegistrySnapshot,
    version: u64,
    groups: Vec<GroupConfiguration>,
    instances: Vec<Vec<PhysicalDevice>>,
    devices: Vec<PhysicalDevice>,
    removed: &[DeviceKey],
) {
    state.devices.retain(|device| {
        if removed.contains(device.key()) {
            device.close();
            false
        } else {
            true
        }
    });
    let mut applied = Vec::with_capacity(groups.len());
    for (group, instances) in groups.into_iter().zip(instances) {
        let functions = instances
            .iter()
            .map(|device| device.functions()[0].clone())
            .collect();
        let handle = match state
            .applied
            .groups
            .iter()
            .find(|present| present.key == group.key)
            .map(|present| present.handle.clone())
        {
            Some(handle) => {
                handle.replace_validated(group.label.clone(), functions, group.capabilities);
                handle
            }
            None => Group::from_validated(
                group.key.clone(),
                group.label.clone(),
                functions,
                group.capabilities,
            ),
        };
        applied.push(AppliedGroup {
            key: group.key,
            label: group.label,
            members: group.members,
            handle,
            instances,
        });
    }
    state.applied = AppliedConfiguration {
        groups: applied,
        devices,
        confirmed: Some(version),
    };
    state.pending = None;
    state.revision = state.revision.wrapping_add(1);
}

fn groups_match_applied(
    groups: &[GroupConfiguration],
    instances: &[Vec<PhysicalDevice>],
    applied: &[AppliedGroup],
) -> bool {
    groups.len() == applied.len()
        && groups.iter().zip(instances).all(|(group, instances)| {
            applied.iter().any(|present| {
                present.key == group.key
                    && present.label == group.label
                    && present.members == group.members
                    && present.handle.has_capabilities(&group.capabilities)
                    && same_instances(&present.instances, instances)
            })
        })
}

fn same_instances(left: &[PhysicalDevice], right: &[PhysicalDevice]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| left.same_instance(right))
}

fn same_pending(left: Option<&PendingConfiguration>, right: Option<&PendingConfiguration>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => {
            left.scope == right.scope
                && left.groups == right.groups
                && left.removed == right.removed
                && same_instances(&left.devices, &right.devices)
                && left
                    .instances
                    .iter()
                    .zip(&right.instances)
                    .all(|(left, right)| same_instances(left, right))
        }
        _ => false,
    }
}

fn resolve_compositions(
    compositions: &[Composition],
    devices: &[PhysicalDevice],
) -> Result<Vec<Vec<PhysicalDevice>>, CompositionError> {
    validate_composition_structure(compositions)?;
    if let Some(group) = compositions
        .iter()
        .find(|group| devices.iter().any(|device| device.key() == &group.key))
    {
        return Err(CompositionError::GroupKeyConflict(group.key.clone()));
    }
    compositions
        .iter()
        .map(|group| {
            group
                .members
                .iter()
                .map(|member| {
                    let device = devices
                        .iter()
                        .find(|device| device.key() == member)
                        .ok_or_else(|| CompositionError::MissingMember(member.clone()))?;
                    let [function] = device.functions() else {
                        return Err(CompositionError::UnsupportedMember(member.clone()));
                    };
                    let description = function.description();
                    if description.id != "main"
                        || description.category != Category::Light
                        || !description.valid_capabilities()
                    {
                        return Err(CompositionError::UnsupportedMember(member.clone()));
                    }
                    Ok(device.clone())
                })
                .collect::<Result<Vec<_>, _>>()
        })
        .collect()
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
        }
    }
    Ok(())
}

fn normalize_compositions(mut compositions: Vec<Composition>) -> Vec<Composition> {
    for group in &mut compositions {
        group.members.sort_by(key_order);
    }
    compositions.sort_by(|left, right| key_order(&left.key, &right.key));
    compositions
}

fn normalize_configurations(mut groups: Vec<GroupConfiguration>) -> Vec<GroupConfiguration> {
    for group in &mut groups {
        group.members.sort_by(key_order);
    }
    groups.sort_by(|left, right| key_order(&left.key, &right.key));
    groups
}

fn key_order(left: &DeviceKey, right: &DeviceKey) -> std::cmp::Ordering {
    (left.source.as_str(), left.id.as_str()).cmp(&(right.source.as_str(), right.id.as_str()))
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
