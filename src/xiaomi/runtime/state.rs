use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet, HashSet, VecDeque},
    rc::{Rc, Weak},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use crate::{
    device::{
        CommandOutcome, DeviceCommand, DeviceService, FeatureIdentity, FeatureRole,
        PhysicalDeviceId, Property, PropertyState, StateReport, StateSource,
    },
    xiaomi::catalog::{FeatureDescriptor, PropertyClass, WireValue},
};
use async_io::Timer;
use event_listener::Event;
use futures_lite::future;
use futures_util::{FutureExt, StreamExt, future::LocalBoxFuture, stream::FuturesUnordered};

use super::{
    AdmissionSnapshot, AdmissionStatus, CommandCompletionSubscription, ControlPath,
    DeviceExecutionGate, OperationPaths, RuntimeFeature,
};
use crate::xiaomi::catalog::WireOperation;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum PushSource {
    Gateway(u64),
    Lan,
    Cloud,
}

impl PushSource {
    fn state_source(self) -> StateSource {
        match self {
            Self::Gateway(_) => StateSource::Gateway,
            Self::Lan => StateSource::Lan,
            Self::Cloud => StateSource::Cloud,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubscriptionToken {
    device: PhysicalDeviceId,
    source: PushSource,
    generation: u64,
    session_id: u64,
    wire_generation: u64,
    auth_generation: crate::storage::AuthSessionGeneration,
    runtime_nonce: u64,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ReadTarget {
    pub siid: u32,
    pub piid: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StateReadFailure {
    Unavailable,
    Malformed,
    Timeout,
    Unauthorized,
    Rejected(i64),
}

#[derive(Clone, Debug, PartialEq)]
pub struct StateReadResult {
    pub siid: u32,
    pub piid: u32,
    pub value: Option<WireValue>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StateReadRequest {
    pub device: PhysicalDeviceId,
    pub path: ControlPath,
    pub targets: Vec<ReadTarget>,
}

pub trait StateReadTransport {
    fn available_paths(&self, _device: &PhysicalDeviceId) -> OperationPaths {
        OperationPaths {
            gateway: true,
            lan: true,
            cloud: true,
        }
    }

    fn read(
        &self,
        request: StateReadRequest,
        timeout: Duration,
        guard: StateReadGuard,
    ) -> LocalBoxFuture<'static, Result<Vec<StateReadResult>, StateReadFailure>>;
}

#[derive(Clone)]
pub struct StateReadGuard {
    cancelled: Arc<AtomicBool>,
    deadline: Instant,
    check: Rc<dyn Fn() -> bool>,
    event: Rc<Event>,
}

impl StateReadGuard {
    #[cfg(test)]
    pub(crate) fn for_test(deadline: Instant) -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            deadline,
            check: Rc::new(|| true),
            event: Rc::new(Event::new()),
        }
    }

    pub fn permitted(&self) -> bool {
        !self.cancelled.load(Ordering::Acquire) && Instant::now() < self.deadline && (self.check)()
    }

    pub fn revoke(&self) {
        if !self.cancelled.swap(true, Ordering::AcqRel) {
            self.event.notify(usize::MAX);
        }
    }

    pub async fn cancelled(&self) {
        loop {
            let listener = self.event.listen();
            if !self.permitted() {
                return;
            }
            listener.await;
        }
    }

    pub(crate) fn expired(&self) -> bool {
        Instant::now() >= self.deadline
    }
}

#[derive(Clone, Copy, Debug)]
pub struct StateLimits {
    pub queue_capacity: usize,
    pub global_concurrency: usize,
    pub batch_size: usize,
    pub read_timeout: Duration,
    pub poll_interval: Duration,
    pub retry_initial: Duration,
    pub retry_max: Duration,
    pub minimum_read_interval: Duration,
    pub cache_flush_interval: Duration,
    pub battery_interval: Duration,
}

impl Default for StateLimits {
    fn default() -> Self {
        Self {
            queue_capacity: 64,
            global_concurrency: 4,
            batch_size: 16,
            read_timeout: Duration::from_secs(5),
            poll_interval: Duration::from_secs(60),
            retry_initial: Duration::from_secs(1),
            retry_max: Duration::from_secs(60),
            minimum_read_interval: Duration::from_millis(100),
            cache_flush_interval: Duration::from_secs(1),
            battery_interval: Duration::from_secs(15 * 60),
        }
    }
}

#[derive(Clone, Debug)]
pub enum StateDiagnostic {
    StorageFailure(crate::storage::StorageError),
    QueueFull,
    ReadFailure {
        feature: FeatureIdentity,
        path: ControlPath,
        failure: StateReadFailure,
    },
}

impl PartialEq for StateDiagnostic {
    fn eq(&self, other: &Self) -> bool {
        matches!((self, other), (Self::QueueFull, Self::QueueFull))
            || matches!(
                (self, other),
                (Self::StorageFailure(_), Self::StorageFailure(_))
            )
            || matches!(
                (self, other),
                (
                    Self::ReadFailure { feature: left_feature, path: left_path, failure: left_failure },
                    Self::ReadFailure { feature: right_feature, path: right_path, failure: right_failure }
                ) if left_feature == right_feature && left_path == right_path && left_failure == right_failure
            )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CloudReachabilityUpdate {
    pub online: bool,
}

#[derive(Clone)]
pub struct StateRuntime {
    inner: Rc<RefCell<StateData>>,
    service: DeviceService,
    store: crate::storage::DeviceStore,
    transport: Rc<dyn StateReadTransport>,
    gate: DeviceExecutionGate,
    completions: Rc<RefCell<CommandCompletionSubscription>>,
    completion_wake: Rc<Event>,
    limits: StateLimits,
    wake: Rc<Event>,
    running: Rc<Cell<bool>>,
    stopped: Rc<Cell<bool>>,
    runtime_nonce: u64,
}

struct StateData {
    features: BTreeMap<FeatureIdentity, RuntimeFeature>,
    admission: BTreeMap<PhysicalDeviceId, DeviceAdmission>,
    selected: BTreeMap<PhysicalDeviceId, SelectedSource>,
    next_selection: u64,
    latest_query: BTreeMap<(FeatureIdentity, Property), u64>,
    pending: BTreeMap<PhysicalDeviceId, VecDeque<ReadJob>>,
    pending_order: VecDeque<PhysicalDeviceId>,
    deferred: BTreeMap<PhysicalDeviceId, VecDeque<ReadJob>>,
    deferred_order: VecDeque<PhysicalDeviceId>,
    active: BTreeMap<PhysicalDeviceId, StateReadGuard>,
    dirty_states: BTreeMap<(FeatureIdentity, Property), crate::storage::PersistedState>,
    retry_attempts: BTreeMap<PhysicalDeviceId, u8>,
    next_due: BTreeMap<PhysicalDeviceId, Instant>,
    next_battery: BTreeMap<PhysicalDeviceId, Instant>,
    next_flush: Option<Instant>,
    next_read_allowed: BTreeMap<PhysicalDeviceId, Instant>,
    diagnostics: VecDeque<StateDiagnostic>,
}

#[derive(Clone, Eq, PartialEq)]
struct DeviceAdmission {
    gateway_push: BTreeSet<u64>,
    lan_push: bool,
}

struct SelectedSource {
    token: SubscriptionToken,
    healthy: bool,
    device_online: Option<bool>,
}

struct ReadJob {
    device: PhysicalDeviceId,
    path: ControlPath,
    targets: Vec<ReadTarget>,
    applications: Vec<ReadApplication>,
    authority: BTreeMap<FeatureIdentity, (u64, crate::storage::AuthSessionGeneration)>,
}

struct ReadApplication {
    target: ReadTarget,
    feature: FeatureIdentity,
    property: Property,
    query: u64,
    synchronize_by: Option<Instant>,
}

impl StateRuntime {
    pub fn new(
        service: DeviceService,
        store: crate::storage::DeviceStore,
        transport: Rc<dyn StateReadTransport>,
        gate: DeviceExecutionGate,
        completions: CommandCompletionSubscription,
    ) -> Self {
        Self::with_limits(
            service,
            store,
            transport,
            gate,
            completions,
            StateLimits::default(),
        )
    }

    pub fn with_limits(
        service: DeviceService,
        store: crate::storage::DeviceStore,
        transport: Rc<dyn StateReadTransport>,
        gate: DeviceExecutionGate,
        completions: CommandCompletionSubscription,
        limits: StateLimits,
    ) -> Self {
        static NEXT_RUNTIME_NONCE: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(1);
        let completion_wake = completions.notifier();
        Self {
            inner: Rc::new(RefCell::new(StateData {
                features: BTreeMap::new(),
                admission: BTreeMap::new(),
                selected: BTreeMap::new(),
                next_selection: 1,
                latest_query: BTreeMap::new(),
                pending: BTreeMap::new(),
                pending_order: VecDeque::new(),
                deferred: BTreeMap::new(),
                deferred_order: VecDeque::new(),
                active: BTreeMap::new(),
                dirty_states: BTreeMap::new(),
                retry_attempts: BTreeMap::new(),
                next_due: BTreeMap::new(),
                next_battery: BTreeMap::new(),
                next_flush: None,
                next_read_allowed: BTreeMap::new(),
                diagnostics: VecDeque::new(),
            })),
            service,
            store,
            transport,
            gate,
            completions: Rc::new(RefCell::new(completions)),
            completion_wake,
            limits,
            wake: Rc::new(Event::new()),
            running: Rc::new(Cell::new(false)),
            stopped: Rc::new(Cell::new(false)),
            runtime_nonce: NEXT_RUNTIME_NONCE.fetch_add(1, Ordering::Relaxed),
        }
    }

    pub fn reconcile(&self, snapshot: &AdmissionSnapshot) {
        if self.stopped.get() {
            return;
        }
        let published_features = self
            .service
            .features()
            .into_iter()
            .map(|feature| feature.identity)
            .collect::<BTreeSet<_>>();
        let mut data = self.inner.borrow_mut();
        let previous = data.features.clone();
        let previous_admission = data.admission.clone();
        let old_devices = data.admission.keys().cloned().collect::<BTreeSet<_>>();
        data.features.clear();
        data.admission.clear();
        if snapshot.status == AdmissionStatus::Active {
            for feature in &snapshot.features {
                data.features
                    .insert(feature.identity.clone(), feature.runtime.clone());
                let admitted = data
                    .admission
                    .entry(feature.identity.physical.clone())
                    .or_insert(DeviceAdmission {
                        gateway_push: BTreeSet::new(),
                        lan_push: false,
                    });
                admitted.gateway_push.extend(
                    feature
                        .gateways
                        .iter()
                        .filter(|gateway| gateway.push)
                        .map(|gateway| gateway.gateway_did),
                );
                admitted.lan_push |= feature.lan_evidence.is_some();
            }
        }
        let current_devices = data.admission.keys().cloned().collect::<BTreeSet<_>>();
        let route_changed = old_devices
            .union(&current_devices)
            .filter(|device| previous_admission.get(*device) != data.admission.get(*device))
            .cloned()
            .collect::<Vec<_>>();
        let invalidated = previous
            .keys()
            .chain(data.features.keys())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .filter(|identity| {
                previous.get(*identity).map(authority_key)
                    != data.features.get(*identity).map(authority_key)
            })
            .cloned()
            .collect::<Vec<_>>();
        let authority_changed_devices = invalidated
            .iter()
            .map(|identity| identity.physical.clone())
            .collect::<BTreeSet<_>>();
        data.dirty_states
            .retain(|(feature, _), _| published_features.contains(feature));
        let work_changed = route_changed
            .iter()
            .cloned()
            .chain(authority_changed_devices.iter().cloned())
            .collect::<BTreeSet<_>>();
        for device in &work_changed {
            if authority_changed_devices.contains(device) {
                revoke_device(&mut data, device);
                continue;
            }
            revoke_read(&mut data, device);
            let selected_allowed = data.selected.get(device).is_some_and(|selected| {
                push_source_allowed(data.admission.get(device), &selected.token.source)
            });
            if !selected_allowed {
                data.selected.remove(device);
            }
        }
        drop(data);
        for identity in invalidated {
            self.service.mark_unconfirmed(&identity);
            self.service.set_state_availability(&identity, false);
        }
        for feature in snapshot.features.iter().map(|feature| &feature.identity) {
            self.update_availability(feature);
        }
        for device in current_devices
            .intersection(&work_changed)
            .cloned()
            .collect::<Vec<_>>()
        {
            self.schedule(&device, true);
        }
    }

    pub fn select_push_source(
        &self,
        device: &PhysicalDeviceId,
        source: PushSource,
        session_id: u64,
        wire_generation: u64,
    ) -> Option<SubscriptionToken> {
        if self.stopped.get() {
            return None;
        }
        let mut data = self.inner.borrow_mut();
        let admission = data.admission.get(device)?;
        let allowed = push_source_allowed(Some(admission), &source);
        if !allowed {
            return None;
        }
        revoke_read(&mut data, device);
        data.selected.remove(device);
        let generation = data.next_selection;
        data.next_selection = data.next_selection.saturating_add(1);
        let auth_generation = data
            .features
            .values()
            .find(|feature| &feature.identity.physical == device)?
            .auth_session_generation;
        let token = SubscriptionToken {
            device: device.clone(),
            source,
            generation,
            session_id,
            wire_generation,
            auth_generation,
            runtime_nonce: self.runtime_nonce,
        };
        data.selected.insert(
            device.clone(),
            SelectedSource {
                token: token.clone(),
                healthy: false,
                device_online: None,
            },
        );
        Some(token)
    }

    pub fn acknowledge(
        &self,
        token: &SubscriptionToken,
        session_id: u64,
        wire_generation: u64,
    ) -> bool {
        if !self.token_fresh(token, session_id, wire_generation) {
            return false;
        }
        let mut data = self.inner.borrow_mut();
        let Some(selected) = data.selected.get_mut(&token.device) else {
            return false;
        };
        if selected.token != *token
            || token.session_id != session_id
            || token.wire_generation != wire_generation
        {
            return false;
        }
        selected.healthy = true;
        let device = token.device.clone();
        let needs_schedule =
            !data.pending.contains_key(&device) && !data.active.contains_key(&device);
        drop(data);
        if needs_schedule {
            self.schedule(&device, true);
        }
        true
    }

    pub fn source_failed(&self, token: &SubscriptionToken) {
        let mut data = self.inner.borrow_mut();
        if data
            .selected
            .get(&token.device)
            .is_some_and(|selected| selected.token == *token)
        {
            data.selected.remove(&token.device);
            drop(data);
            self.schedule(&token.device, false);
            for feature in self
                .service
                .features()
                .into_iter()
                .filter(|feature| feature.identity.physical == token.device)
            {
                self.update_availability(&feature.identity);
            }
        }
    }

    #[cfg(test)]
    pub(super) fn has_healthy_source_for_test(
        &self,
        device: &PhysicalDeviceId,
        source: PushSource,
    ) -> bool {
        self.inner
            .borrow()
            .selected
            .get(device)
            .is_some_and(|selected| selected.healthy && selected.token.source == source)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn apply_property(
        &self,
        token: &SubscriptionToken,
        session_id: u64,
        wire_generation: u64,
        siid: u32,
        piid: u32,
        value: Option<&WireValue>,
        observed_at: i64,
        retained: bool,
    ) -> bool {
        if retained || !self.token_current(token, session_id, wire_generation) {
            return false;
        }
        let decoded = self
            .inner
            .borrow()
            .features
            .values()
            .filter(|feature| feature.identity.physical == token.device)
            .filter_map(|feature| {
                let mapping = feature
                    .descriptor
                    .binding
                    .properties
                    .iter()
                    .find(|mapping| mapping.siid == siid && mapping.piid == piid)?;
                Some((
                    feature.identity.clone(),
                    mapping.property,
                    value
                        .and_then(|value| feature.descriptor.binding.decode(siid, piid, value))
                        .and_then(|(_, value)| value),
                ))
            })
            .collect::<Vec<_>>();
        if decoded.is_empty() {
            return false;
        }
        self.note_cloud_report(token);
        self.apply_decoded(decoded, token.source.state_source(), observed_at);
        true
    }

    #[allow(clippy::too_many_arguments)]
    pub fn apply_positional_event(
        &self,
        token: &SubscriptionToken,
        session_id: u64,
        wire_generation: u64,
        siid: u32,
        eiid: u32,
        arguments: &[WireValue],
        observed_at: i64,
        retained: bool,
    ) -> bool {
        self.apply_event(
            token,
            session_id,
            wire_generation,
            observed_at,
            retained,
            |descriptor| descriptor.binding.decode_event(siid, eiid, arguments),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn apply_keyed_event(
        &self,
        token: &SubscriptionToken,
        session_id: u64,
        wire_generation: u64,
        siid: u32,
        eiid: u32,
        arguments: &[(u32, WireValue)],
        observed_at: i64,
        retained: bool,
    ) -> bool {
        self.apply_event(
            token,
            session_id,
            wire_generation,
            observed_at,
            retained,
            |descriptor| descriptor.binding.decode_keyed_event(siid, eiid, arguments),
        )
    }

    pub fn apply_cloud_online(
        &self,
        token: &SubscriptionToken,
        session_id: u64,
        wire_generation: u64,
        online: bool,
    ) -> Option<CloudReachabilityUpdate> {
        if !matches!(token.source, PushSource::Cloud)
            || !self.token_current(token, session_id, wire_generation)
        {
            return None;
        }
        if let Some(selected) = self.inner.borrow_mut().selected.get_mut(&token.device) {
            selected.device_online = Some(online);
        }
        for feature in self
            .service
            .features()
            .into_iter()
            .filter(|feature| feature.identity.physical == token.device)
        {
            self.update_availability(&feature.identity);
        }
        Some(CloudReachabilityUpdate { online })
    }

    pub fn request_refresh(&self, device: &PhysicalDeviceId) {
        self.schedule_selected(device, true, true, None, None);
    }

    pub async fn run_until_idle(&self) {
        self.drive(true).await;
    }

    pub async fn run(&self) {
        self.drive(false).await;
    }

    pub fn stop(&self) {
        self.stopped.set(true);
        let mut data = self.inner.borrow_mut();
        let features = data.features.keys().cloned().collect::<Vec<_>>();
        for guard in data.active.values() {
            guard.revoke();
        }
        data.active.clear();
        data.pending.clear();
        data.pending_order.clear();
        data.deferred.clear();
        data.deferred_order.clear();
        data.selected.clear();
        data.admission.clear();
        data.next_due.clear();
        data.next_battery.clear();
        drop(data);
        for feature in features {
            self.service.set_state_availability(&feature, false);
        }
        self.flush_cache();
        self.wake.notify(usize::MAX);
    }

    pub fn flush_cache(&self) {
        let states = {
            let data = self.inner.borrow();
            data.dirty_states.values().cloned().collect::<Vec<_>>()
        };
        if states.is_empty() {
            self.inner.borrow_mut().next_flush = None;
            return;
        }
        if let Err(error) = self.store.save_states(&states) {
            let mut data = self.inner.borrow_mut();
            data.next_flush = Some(Instant::now() + self.limits.cache_flush_interval);
            push_diagnostic(&mut data, StateDiagnostic::StorageFailure(error));
            self.wake.notify(usize::MAX);
            return;
        }
        let mut data = self.inner.borrow_mut();
        data.dirty_states.clear();
        data.next_flush = None;
    }

    pub fn drain_diagnostics(&self) -> Vec<StateDiagnostic> {
        self.inner.borrow_mut().diagnostics.drain(..).collect()
    }

    fn schedule(&self, device: &PhysicalDeviceId, ancillary: bool) {
        self.schedule_selected(device, ancillary, false, None, None);
    }

    fn schedule_selected(
        &self,
        device: &PhysicalDeviceId,
        ancillary: bool,
        force: bool,
        selected_targets: Option<&BTreeSet<(u32, u32)>>,
        synchronize_by: Option<Instant>,
    ) {
        let Some(path) = select_path(self.transport.available_paths(device)) else {
            return;
        };
        let mut data = self.inner.borrow_mut();
        if self.stopped.get() || !data.admission.contains_key(device) {
            return;
        }
        let healthy = data
            .selected
            .get(device)
            .is_some_and(|selected| selected.healthy);
        let mut targets = BTreeSet::new();
        let mut mapped = Vec::new();
        for feature in data
            .features
            .values()
            .filter(|feature| &feature.identity.physical == device)
        {
            let snapshot = self.service.snapshot(&feature.identity);
            for mapping in &feature.descriptor.binding.properties {
                if !mapping.readable
                    || (mapping.class == PropertyClass::Ancillary && !ancillary)
                    || selected_targets
                        .is_some_and(|targets| !targets.contains(&(mapping.siid, mapping.piid)))
                {
                    continue;
                }
                let confirmed = snapshot
                    .as_ref()
                    .and_then(|snapshot| snapshot.property(mapping.property))
                    .is_some_and(|state| matches!(state, PropertyState::Current { .. }));
                if !force && healthy && mapping.notify && confirmed {
                    continue;
                }
                let target = ReadTarget {
                    siid: mapping.siid,
                    piid: mapping.piid,
                };
                targets.insert(target);
                mapped.push((feature.identity.clone(), mapping.property, target));
            }
        }
        if targets.is_empty() {
            return;
        }
        let authority = data
            .features
            .values()
            .filter(|feature| &feature.identity.physical == device)
            .map(|feature| {
                (
                    feature.identity.clone(),
                    (
                        feature.authority_generation,
                        feature.auth_session_generation,
                    ),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let was_pending = data.pending.contains_key(device);
        let mut applications = data
            .pending
            .remove(device)
            .into_iter()
            .flatten()
            .chain(data.deferred.remove(device).into_iter().flatten())
            .flat_map(|job| job.applications)
            .map(|application| {
                (
                    (application.feature.clone(), application.property),
                    application,
                )
            })
            .collect::<BTreeMap<_, _>>();
        data.pending_order.retain(|queued| queued != device);
        data.deferred_order.retain(|queued| queued != device);
        for (feature, property, target) in mapped {
            let query = self.service.begin_query(&feature, property);
            data.latest_query.insert((feature.clone(), property), query);
            let key = (feature.clone(), property);
            let application_deadline = applications.get(&key).map_or(synchronize_by, |existing| {
                merge_optional_deadline(existing.synchronize_by, synchronize_by)
            });
            applications.insert(
                key,
                ReadApplication {
                    target,
                    feature,
                    property,
                    query,
                    synchronize_by: application_deadline,
                },
            );
        }
        let targets = applications
            .values()
            .map(|application| application.target)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let mut jobs = VecDeque::new();
        for chunk in targets.chunks(self.limits.batch_size.max(1)) {
            let selected = chunk.iter().copied().collect::<BTreeSet<_>>();
            let chunk_applications = applications
                .values()
                .filter(|application| selected.contains(&application.target))
                .map(|application| ReadApplication {
                    target: application.target,
                    feature: application.feature.clone(),
                    property: application.property,
                    query: application.query,
                    synchronize_by: application.synchronize_by,
                })
                .collect();
            jobs.push_back(ReadJob {
                device: device.clone(),
                path,
                targets: chunk.to_vec(),
                applications: chunk_applications,
                authority: authority.clone(),
            });
        }
        if !was_pending
            && data.pending.len() + data.active.len() >= self.limits.queue_capacity.max(1)
        {
            data.deferred.insert(device.clone(), jobs);
            data.deferred_order.push_back(device.clone());
            push_diagnostic(&mut data, StateDiagnostic::QueueFull);
        } else {
            data.pending.insert(device.clone(), jobs);
            data.pending_order.push_back(device.clone());
        }
        drop(data);
        self.wake.notify(usize::MAX);
    }

    async fn drive(&self, stop_when_idle: bool) {
        if self.stopped.get() || self.running.replace(true) {
            return;
        }
        let _run = StateRunGuard {
            runtime: self.clone(),
            final_stop: !stop_when_idle,
        };
        let mut running = FuturesUnordered::new();
        let mut busy = HashSet::new();
        loop {
            self.collect_completions();
            self.schedule_due();
            self.promote_deferred();
            while running.len() < self.limits.global_concurrency.max(1) {
                let next = {
                    let mut data = self.inner.borrow_mut();
                    let now = Instant::now();
                    let mut device = None;
                    let queued = data.pending_order.len();
                    for _ in 0..queued {
                        let Some(candidate) = data.pending_order.pop_front() else {
                            break;
                        };
                        if data.pending.contains_key(&candidate)
                            && !busy.contains(&candidate)
                            && data
                                .next_read_allowed
                                .get(&candidate)
                                .is_none_or(|deadline| *deadline <= now)
                        {
                            device = Some(candidate);
                            break;
                        }
                        if data.pending.contains_key(&candidate) {
                            data.pending_order.push_back(candidate);
                        }
                    }
                    device.and_then(|device| {
                        let job = data.pending.get_mut(&device)?.pop_front()?;
                        if data.pending.get(&device).is_some_and(VecDeque::is_empty) {
                            data.pending.remove(&device);
                        } else {
                            data.pending_order.push_back(device.clone());
                        }
                        data.next_read_allowed
                            .insert(device.clone(), now + self.limits.minimum_read_interval);
                        Some((device, job))
                    })
                };
                let Some((device, job)) = next else {
                    break;
                };
                busy.insert(device.clone());
                running.push(async move { (device, self.execute(job).await) }.boxed_local());
            }
            if running.is_empty() {
                let work_empty = {
                    let data = self.inner.borrow();
                    data.pending.is_empty() && data.deferred.is_empty()
                };
                if (stop_when_idle && work_empty) || self.stopped.get() {
                    return;
                }
                let listener = self.wake.listen();
                let completion_listener = self.completion_wake.listen();
                if let Some(deadline) = self.next_deadline() {
                    future::race(future::race(listener, completion_listener), async {
                        Timer::at(deadline).await;
                    })
                    .await;
                } else {
                    future::race(listener, completion_listener).await;
                }
                continue;
            }
            let listener = self.wake.listen();
            let completion_listener = self.completion_wake.listen();
            let deadline = self.next_deadline();
            match future::race(running.next().map(StateDriveEvent::Completed), async {
                future::race(future::race(listener, completion_listener), async {
                    if let Some(deadline) = deadline {
                        Timer::at(deadline).await;
                    } else {
                        future::pending::<()>().await;
                    }
                })
                .await;
                StateDriveEvent::Woken
            })
            .await
            {
                StateDriveEvent::Completed(Some((device, ()))) => {
                    busy.remove(&device);
                }
                StateDriveEvent::Completed(None) | StateDriveEvent::Woken => {}
            }
        }
    }

    async fn execute(&self, mut job: ReadJob) {
        let now = Instant::now();
        job.applications.retain(|application| {
            application
                .synchronize_by
                .is_none_or(|deadline| now < deadline)
        });
        job.targets = job
            .applications
            .iter()
            .map(|application| application.target)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if job.targets.is_empty() {
            return;
        }
        let synchronization_deadline = job
            .applications
            .iter()
            .map(|application| application.synchronize_by)
            .try_fold(Instant::now(), |latest, deadline| {
                deadline.map(|deadline| latest.max(deadline))
            });
        let deadline = synchronization_deadline
            .map_or(now + self.limits.read_timeout, |synchronize_by| {
                synchronize_by.min(now + self.limits.read_timeout)
            });
        let guard = self.guard(&job, deadline);
        self.inner
            .borrow_mut()
            .active
            .insert(job.device.clone(), guard.clone());
        let lease = future::race(
            self.gate.acquire(job.device.clone(), deadline).map(Some),
            async {
                guard.cancelled().await;
                None
            },
        )
        .await;
        let Some(_lease) = lease.flatten() else {
            self.inner.borrow_mut().active.remove(&job.device);
            return;
        };
        if !guard.permitted() {
            self.inner.borrow_mut().active.remove(&job.device);
            return;
        }
        let now = Instant::now();
        job.applications.retain(|application| {
            application
                .synchronize_by
                .is_none_or(|deadline| now < deadline)
        });
        job.targets = job
            .applications
            .iter()
            .map(|application| application.target)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if job.targets.is_empty() {
            self.inner.borrow_mut().active.remove(&job.device);
            return;
        }
        let request = StateReadRequest {
            device: job.device.clone(),
            path: job.path,
            targets: job.targets.clone(),
        };
        let result = future::race(
            future::race(
                self.transport
                    .read(request, self.limits.read_timeout, guard.clone())
                    .map(Some),
                async {
                    Timer::at(deadline).await;
                    None
                },
            )
            .map(ReadRace::Finished),
            async {
                guard.cancelled().await;
                ReadRace::Revoked
            },
        )
        .await;
        self.inner.borrow_mut().active.remove(&job.device);
        match result {
            ReadRace::Finished(Some(Ok(results))) if guard.permitted() => {
                let device = job.device.clone();
                let battery_attempted = job
                    .applications
                    .iter()
                    .any(|application| application.property == Property::Battery);
                self.apply_read_results(&job, results);
                let unresolved_notify = self.has_unconfirmed_notify(&job);
                let mut data = self.inner.borrow_mut();
                if battery_attempted {
                    data.next_battery.insert(
                        device.clone(),
                        Instant::now() + self.limits.battery_interval,
                    );
                }
                if unresolved_notify {
                    drop(data);
                    self.schedule_retry(device);
                    return;
                }
                data.retry_attempts.remove(&device);
                let has_uncovered = data.features.values().any(|feature| {
                    feature.identity.physical == device
                        && feature.descriptor.binding.properties.iter().any(|mapping| {
                            mapping.readable
                                && !mapping.notify
                                && mapping.class != PropertyClass::Ancillary
                        })
                });
                if has_uncovered
                    || !data
                        .selected
                        .get(&device)
                        .is_some_and(|selected| selected.healthy)
                {
                    data.next_due
                        .insert(device.clone(), Instant::now() + self.limits.poll_interval);
                }
            }
            ReadRace::Finished(Some(Err(failure))) => {
                self.record_read_failure(&job, failure);
                self.schedule_retry(job.device);
            }
            ReadRace::Finished(None) => {
                self.record_read_failure(&job, StateReadFailure::Timeout);
                self.schedule_retry(job.device);
            }
            ReadRace::Finished(Some(Ok(_))) if guard.expired() => {
                self.record_read_failure(&job, StateReadFailure::Timeout);
                self.schedule_retry(job.device);
            }
            ReadRace::Finished(Some(Ok(_))) => {}
            ReadRace::Revoked => {}
        }
    }

    fn record_read_failure(&self, job: &ReadJob, failure: StateReadFailure) {
        let features = job
            .applications
            .iter()
            .map(|application| application.feature.clone())
            .collect::<BTreeSet<_>>();
        let mut data = self.inner.borrow_mut();
        for feature in features {
            push_diagnostic(
                &mut data,
                StateDiagnostic::ReadFailure {
                    feature,
                    path: job.path,
                    failure,
                },
            );
        }
    }

    fn apply_read_results(&self, job: &ReadJob, results: Vec<StateReadResult>) {
        let source = match job.path {
            ControlPath::Gateway => StateSource::Gateway,
            ControlPath::Lan => StateSource::Lan,
            ControlPath::Cloud => StateSource::Cloud,
        };
        for result in results {
            for application in job.applications.iter().filter(|application| {
                application.target.siid == result.siid && application.target.piid == result.piid
            }) {
                let valid = {
                    let data = self.inner.borrow();
                    application
                        .synchronize_by
                        .is_none_or(|deadline| Instant::now() < deadline)
                        && data
                            .latest_query
                            .get(&(application.feature.clone(), application.property))
                            == Some(&application.query)
                        && data
                            .features
                            .get(&application.feature)
                            .is_some_and(|feature| {
                                job.authority.get(&application.feature)
                                    == Some(&(
                                        feature.authority_generation,
                                        feature.auth_session_generation,
                                    ))
                            })
                };
                if !valid {
                    continue;
                }
                let decoded = result.value.as_ref().and_then(|value| {
                    self.inner
                        .borrow()
                        .features
                        .get(&application.feature)
                        .and_then(|feature| {
                            feature
                                .descriptor
                                .binding
                                .decode(result.siid, result.piid, value)
                        })
                        .and_then(|(_, value)| value)
                });
                let version = application.query;
                if let Some(value) = decoded {
                    self.apply_confirmed(
                        application.feature.clone(),
                        application.property,
                        value,
                        version,
                        source,
                        unix_now(),
                    );
                } else {
                    self.service
                        .apply_unknown(&application.feature, application.property, version);
                    self.update_availability(&application.feature);
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_event<F>(
        &self,
        token: &SubscriptionToken,
        session_id: u64,
        wire_generation: u64,
        observed_at: i64,
        retained: bool,
        decode: F,
    ) -> bool
    where
        F: Fn(&FeatureDescriptor) -> Option<Vec<(Property, Option<crate::device::PropertyValue>)>>,
    {
        if retained || !self.token_current(token, session_id, wire_generation) {
            return false;
        }
        let decoded = self
            .inner
            .borrow()
            .features
            .values()
            .filter(|feature| feature.identity.physical == token.device)
            .filter_map(|feature| {
                decode(&feature.descriptor).map(|values| (feature.identity.clone(), values))
            })
            .flat_map(|(feature, values)| {
                values
                    .into_iter()
                    .map(move |(property, value)| (feature.clone(), property, value))
            })
            .collect::<Vec<_>>();
        if decoded.is_empty() {
            return false;
        }
        self.note_cloud_report(token);
        self.apply_decoded(decoded, token.source.state_source(), observed_at);
        true
    }

    fn apply_decoded(
        &self,
        decoded: Vec<(
            FeatureIdentity,
            Property,
            Option<crate::device::PropertyValue>,
        )>,
        source: StateSource,
        observed_at: i64,
    ) {
        let version = self.service.next_report_version();
        for (feature, property, value) in decoded {
            if let Some(value) = value {
                self.apply_confirmed(feature, property, value, version, source, observed_at);
            } else {
                self.service.apply_unknown(&feature, property, version);
                self.update_availability(&feature);
            }
        }
    }

    fn apply_confirmed(
        &self,
        feature: FeatureIdentity,
        property: Property,
        value: crate::device::PropertyValue,
        version: u64,
        source: StateSource,
        observed_at: i64,
    ) {
        if !self.service.apply_report(StateReport::new(
            feature.clone(),
            version,
            source,
            observed_at,
            [(property, value.clone())],
        )) {
            return;
        }
        let dirty_key = (feature.clone(), property);
        let mut data = self.inner.borrow_mut();
        data.dirty_states.insert(
            dirty_key,
            crate::storage::PersistedState {
                feature: feature.clone(),
                property,
                value,
                source,
                observed_at,
                report_version: version,
            },
        );
        data.next_flush
            .get_or_insert(Instant::now() + self.limits.cache_flush_interval);
        drop(data);
        self.wake.notify(usize::MAX);
        self.update_availability(&feature);
        self.note_report_success(&feature.physical);
    }

    fn note_report_success(&self, device: &PhysicalDeviceId) {
        let mut data = self.inner.borrow_mut();
        let reset_backoff = data.retry_attempts.remove(device).is_some();
        let has_uncovered = data.features.values().any(|feature| {
            &feature.identity.physical == device
                && feature.descriptor.binding.properties.iter().any(|mapping| {
                    mapping.readable && !mapping.notify && mapping.class != PropertyClass::Ancillary
                })
        });
        if has_uncovered {
            let deadline = Instant::now() + self.limits.poll_interval;
            if reset_backoff {
                data.next_due.insert(device.clone(), deadline);
            } else {
                data.next_due
                    .entry(device.clone())
                    .and_modify(|current| *current = (*current).min(deadline))
                    .or_insert(deadline);
            }
        } else {
            data.next_due.remove(device);
        }
    }

    fn schedule_retry(&self, device: PhysicalDeviceId) {
        let mut data = self.inner.borrow_mut();
        let attempt = data.retry_attempts.entry(device.clone()).or_default();
        let multiplier = 1_u32
            .checked_shl(u32::from((*attempt).min(8)))
            .unwrap_or(u32::MAX);
        let delay = self
            .limits
            .retry_initial
            .saturating_mul(multiplier)
            .min(self.limits.retry_max);
        *attempt = attempt.saturating_add(1);
        data.next_due.insert(device, Instant::now() + delay);
    }

    fn has_unconfirmed_notify(&self, job: &ReadJob) -> bool {
        let data = self.inner.borrow();
        job.applications.iter().any(|application| {
            data.features
                .get(&application.feature)
                .and_then(|feature| {
                    feature
                        .descriptor
                        .binding
                        .properties
                        .iter()
                        .find(|mapping| {
                            mapping.property == application.property
                                && mapping.siid == application.target.siid
                                && mapping.piid == application.target.piid
                        })
                })
                .is_some_and(|mapping| {
                    mapping.notify
                        && mapping.class != PropertyClass::Ancillary
                        && !self
                            .service
                            .snapshot(&application.feature)
                            .and_then(|snapshot| snapshot.property(application.property).cloned())
                            .is_some_and(|state| matches!(state, PropertyState::Current { .. }))
                })
        })
    }

    fn guard(&self, job: &ReadJob, deadline: Instant) -> StateReadGuard {
        let inner: Weak<RefCell<StateData>> = Rc::downgrade(&self.inner);
        let device = job.device.clone();
        let authority = job.authority.clone();
        StateReadGuard {
            cancelled: Arc::new(AtomicBool::new(false)),
            deadline,
            check: Rc::new(move || {
                inner.upgrade().is_some_and(|inner| {
                    let data = inner.borrow();
                    data.admission.contains_key(&device)
                        && authority.iter().all(|(identity, expected)| {
                            data.features.get(identity).is_some_and(|feature| {
                                &(
                                    feature.authority_generation,
                                    feature.auth_session_generation,
                                ) == expected
                            })
                        })
                })
            }),
            event: Rc::new(Event::new()),
        }
    }

    fn token_current(
        &self,
        token: &SubscriptionToken,
        session_id: u64,
        wire_generation: u64,
    ) -> bool {
        if !self.token_fresh(token, session_id, wire_generation) {
            return false;
        }
        self.inner
            .borrow()
            .selected
            .get(&token.device)
            .is_some_and(|selected| selected.healthy)
    }

    fn token_fresh(
        &self,
        token: &SubscriptionToken,
        session_id: u64,
        wire_generation: u64,
    ) -> bool {
        if self.stopped.get() {
            return false;
        }
        let data = self.inner.borrow();
        token.runtime_nonce == self.runtime_nonce
            && data.selected.get(&token.device).is_some_and(|selected| {
                selected.token == *token
                    && token.session_id == session_id
                    && token.wire_generation == wire_generation
                    && data.features.values().any(|feature| {
                        feature.identity.physical == token.device
                            && feature.auth_session_generation == token.auth_generation
                    })
            })
    }

    fn collect_completions(&self) {
        let (completions, lagged) = {
            let mut subscription = self.completions.borrow_mut();
            let completions = subscription.drain();
            let lagged = subscription.take_lagged();
            (completions, lagged)
        };
        if lagged {
            let synchronize_by = Instant::now() + Duration::from_secs(5);
            let devices = self
                .inner
                .borrow()
                .admission
                .keys()
                .cloned()
                .collect::<Vec<_>>();
            for device in devices {
                self.schedule_selected(&device, false, true, None, Some(synchronize_by));
            }
        }
        for completion in completions {
            if matches!(
                completion.outcome,
                CommandOutcome::Accepted | CommandOutcome::Ambiguous
            ) && Instant::now() < completion.synchronize_by
                && self
                    .inner
                    .borrow()
                    .features
                    .get(&completion.feature)
                    .is_some_and(|feature| {
                        feature.authority_generation == completion.authority_generation
                            && feature.auth_session_generation == completion.auth_session_generation
                    })
            {
                let targets = self.readback_targets(&completion);
                self.schedule_selected(
                    &completion.feature.physical,
                    false,
                    true,
                    Some(&targets),
                    Some(completion.synchronize_by),
                );
            }
        }
    }

    fn update_availability(&self, identity: &FeatureIdentity) {
        let data = self.inner.borrow();
        let Some(feature) = data.features.get(identity) else {
            self.service.set_state_availability(identity, false);
            return;
        };
        let snapshot = self.service.snapshot(identity);
        let current = |property| {
            snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.property(property))
                .is_some_and(|state| matches!(state, PropertyState::Current { .. }))
        };
        let critical: &[Property] = match identity.role {
            FeatureRole::Light
            | FeatureRole::Load
            | FeatureRole::Fan
            | FeatureRole::BathHeaterLight
            | FeatureRole::BathHeaterSupplyFan
            | FeatureRole::BathHeaterExhaustFan => &[Property::Power],
            FeatureRole::Climate | FeatureRole::BathHeaterClimate => {
                if feature
                    .descriptor
                    .binding
                    .properties
                    .iter()
                    .any(|mapping| mapping.property == Property::HvacMode)
                {
                    &[Property::Power, Property::HvacMode]
                } else {
                    &[Property::Power]
                }
            }
            FeatureRole::Curtain => &[Property::CurtainPosition],
            FeatureRole::TemperatureSensor => &[Property::Temperature],
            FeatureRole::HumiditySensor => &[Property::Humidity],
            FeatureRole::IlluminanceSensor => &[Property::Illuminance],
            FeatureRole::MotionSensor => &[Property::Motion],
            FeatureRole::OccupancySensor => &[Property::Occupancy],
            FeatureRole::ContactSensor => &[Property::Contact],
            FeatureRole::Vacuum => &[Property::VacuumOperationalState],
        };
        let control_role = matches!(
            identity.role,
            FeatureRole::Light
                | FeatureRole::Load
                | FeatureRole::Climate
                | FeatureRole::Curtain
                | FeatureRole::Fan
                | FeatureRole::Vacuum
                | FeatureRole::BathHeaterLight
                | FeatureRole::BathHeaterSupplyFan
                | FeatureRole::BathHeaterExhaustFan
                | FeatureRole::BathHeaterClimate
        );
        let paths = self.transport.available_paths(&identity.physical);
        let explicitly_offline = data
            .selected
            .get(&identity.physical)
            .is_some_and(|selected| {
                matches!(selected.token.source, PushSource::Cloud)
                    && selected.device_online == Some(false)
            });
        let reachable = data
            .admission
            .get(&identity.physical)
            .is_some_and(|admission| {
                !explicitly_offline
                    && (paths.gateway
                        || paths.lan
                        || paths.cloud
                        || data
                            .selected
                            .get(&identity.physical)
                            .is_some_and(|selected| {
                                selected.healthy
                                    && match &selected.token.source {
                                        PushSource::Gateway(gateway) => {
                                            admission.gateway_push.contains(gateway)
                                        }
                                        PushSource::Lan => admission.lan_push,
                                        PushSource::Cloud => selected.device_online == Some(true),
                                    }
                            }))
            });
        let requires_confirmation = |property| {
            !control_role
                || feature.descriptor.binding.properties.iter().any(|mapping| {
                    mapping.property == property && (mapping.readable || mapping.notify)
                })
        };
        self.service.set_state_availability(
            identity,
            critical
                .iter()
                .all(|property| !requires_confirmation(*property) || current(*property))
                && reachable
                && (!control_role || paths.gateway || paths.lan || paths.cloud),
        );
    }

    fn note_cloud_report(&self, token: &SubscriptionToken) {
        if !matches!(token.source, PushSource::Cloud) {
            return;
        }
        if let Some(selected) = self.inner.borrow_mut().selected.get_mut(&token.device)
            && selected.token == *token
        {
            selected.device_online = Some(true);
        }
    }

    fn schedule_due(&self) {
        let now = Instant::now();
        let due = self
            .inner
            .borrow()
            .next_due
            .iter()
            .filter(|(_, deadline)| **deadline <= now)
            .map(|(device, _)| device.clone())
            .collect::<Vec<_>>();
        let defer_due = !self.inner.borrow().deferred.is_empty();
        for device in due {
            if defer_due {
                self.inner.borrow_mut().next_due.insert(
                    device,
                    Instant::now()
                        + self
                            .limits
                            .minimum_read_interval
                            .max(Duration::from_millis(1)),
                );
                continue;
            }
            self.inner.borrow_mut().next_due.remove(&device);
            self.schedule(&device, false);
        }
        let battery_due = self
            .inner
            .borrow()
            .next_battery
            .iter()
            .filter(|(_, deadline)| **deadline <= now)
            .map(|(device, _)| device.clone())
            .collect::<Vec<_>>();
        let defer_battery = !self.inner.borrow().deferred.is_empty();
        for device in battery_due {
            if defer_battery {
                self.inner.borrow_mut().next_battery.insert(
                    device,
                    Instant::now()
                        + self
                            .limits
                            .minimum_read_interval
                            .max(Duration::from_millis(1)),
                );
                continue;
            }
            self.inner.borrow_mut().next_battery.remove(&device);
            self.schedule(&device, true);
        }
        if self
            .inner
            .borrow()
            .next_flush
            .is_some_and(|deadline| deadline <= now)
        {
            self.flush_cache();
        }
    }

    fn promote_deferred(&self) {
        loop {
            let deferred = {
                let mut data = self.inner.borrow_mut();
                if data.pending.len() + data.active.len() >= self.limits.queue_capacity.max(1) {
                    None
                } else {
                    let device = data.deferred_order.pop_front();
                    device
                        .and_then(|device| data.deferred.remove(&device).map(|jobs| (device, jobs)))
                }
            };
            let Some((device, jobs)) = deferred else {
                break;
            };
            let mut data = self.inner.borrow_mut();
            data.pending.insert(device.clone(), jobs);
            data.pending_order.push_back(device);
            drop(data);
            self.wake.notify(usize::MAX);
        }
    }

    fn next_deadline(&self) -> Option<Instant> {
        let data = self.inner.borrow();
        let now = Instant::now();
        data.next_due
            .values()
            .copied()
            .chain(data.next_battery.values().copied())
            .chain(data.next_flush)
            .chain(
                data.pending
                    .keys()
                    .filter_map(|device| data.next_read_allowed.get(device).copied())
                    .filter(|deadline| *deadline > now),
            )
            .min()
    }

    fn readback_targets(&self, completion: &super::CommandCompletion) -> BTreeSet<(u32, u32)> {
        match completion.operation {
            WireOperation::SetProperty { siid, piid, .. } => BTreeSet::from([(siid, piid)]),
            WireOperation::InvokeAction { .. } => {
                let properties: &[Property] = match completion.command {
                    DeviceCommand::StopCurtain => {
                        &[Property::CurtainPosition, Property::CurtainMovement]
                    }
                    DeviceCommand::StartVacuum
                    | DeviceCommand::StopVacuum
                    | DeviceCommand::ReturnVacuumToDock => {
                        &[Property::VacuumOperationalState, Property::VacuumFault]
                    }
                    _ => &[],
                };
                self.inner
                    .borrow()
                    .features
                    .get(&completion.feature)
                    .into_iter()
                    .flat_map(|feature| &feature.descriptor.binding.properties)
                    .filter(|mapping| properties.contains(&mapping.property))
                    .map(|mapping| (mapping.siid, mapping.piid))
                    .collect()
            }
        }
    }
}

fn merge_optional_deadline(
    existing: Option<Instant>,
    additional: Option<Instant>,
) -> Option<Instant> {
    match (existing, additional) {
        (None, _) | (_, None) => None,
        (Some(existing), Some(additional)) => Some(existing.max(additional)),
    }
}

enum StateDriveEvent {
    Completed(Option<(PhysicalDeviceId, ())>),
    Woken,
}

enum ReadRace {
    Finished(Option<Result<Vec<StateReadResult>, StateReadFailure>>),
    Revoked,
}

struct StateRunGuard {
    runtime: StateRuntime,
    final_stop: bool,
}

impl Drop for StateRunGuard {
    fn drop(&mut self) {
        self.runtime.running.set(false);
        if self.final_stop {
            self.runtime.stopped.set(true);
            let mut data = self.runtime.inner.borrow_mut();
            let features = data.features.keys().cloned().collect::<Vec<_>>();
            for guard in data.active.values() {
                guard.revoke();
            }
            data.active.clear();
            data.pending.clear();
            data.pending_order.clear();
            data.deferred.clear();
            data.deferred_order.clear();
            data.selected.clear();
            data.admission.clear();
            data.next_due.clear();
            data.next_battery.clear();
            drop(data);
            for feature in features {
                self.runtime.service.set_state_availability(&feature, false);
            }
            self.runtime.flush_cache();
        }
    }
}

fn authority_key(
    feature: &RuntimeFeature,
) -> (FeatureIdentity, u64, crate::storage::AuthSessionGeneration) {
    (
        feature.identity.clone(),
        feature.authority_generation,
        feature.auth_session_generation,
    )
}

fn revoke_read(data: &mut StateData, device: &PhysicalDeviceId) {
    if let Some(guard) = data.active.remove(device) {
        guard.revoke();
    }
    data.pending.remove(device);
    data.pending_order.retain(|queued| queued != device);
    data.deferred.remove(device);
    data.deferred_order.retain(|queued| queued != device);
}

fn revoke_device(data: &mut StateData, device: &PhysicalDeviceId) {
    revoke_read(data, device);
    data.selected.remove(device);
    data.latest_query
        .retain(|(feature, _), _| &feature.physical != device);
    data.retry_attempts.remove(device);
    data.next_due.remove(device);
    data.next_battery.remove(device);
}

fn push_source_allowed(admission: Option<&DeviceAdmission>, source: &PushSource) -> bool {
    admission.is_some_and(|admission| match source {
        PushSource::Gateway(gateway) => admission.gateway_push.contains(gateway),
        PushSource::Lan => admission.lan_push,
        PushSource::Cloud => true,
    })
}

fn select_path(paths: OperationPaths) -> Option<ControlPath> {
    if paths.gateway {
        Some(ControlPath::Gateway)
    } else if paths.lan {
        Some(ControlPath::Lan)
    } else if paths.cloud {
        Some(ControlPath::Cloud)
    } else {
        None
    }
}

fn push_diagnostic(data: &mut StateData, diagnostic: StateDiagnostic) {
    if !data.diagnostics.contains(&diagnostic) {
        if data.diagnostics.len() == 32 {
            data.diagnostics.pop_front();
        }
        data.diagnostics.push_back(diagnostic);
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() as i64)
}

#[cfg(test)]
mod tests;
