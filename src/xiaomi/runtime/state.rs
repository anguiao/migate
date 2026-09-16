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
                    .properties
                    .iter()
                    .find(|mapping| mapping.siid == siid && mapping.piid == piid)?;
                Some((
                    feature.identity.clone(),
                    mapping.property,
                    value
                        .and_then(|value| feature.descriptor.decode(siid, piid, value))
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
            |descriptor| descriptor.decode_event(siid, eiid, arguments),
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
            |descriptor| descriptor.decode_keyed_event(siid, eiid, arguments),
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
            for mapping in &feature.descriptor.properties {
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
                        && feature.descriptor.properties.iter().any(|mapping| {
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
                            feature.descriptor.decode(result.siid, result.piid, value)
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
                && feature.descriptor.properties.iter().any(|mapping| {
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
                    feature.descriptor.properties.iter().find(|mapping| {
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
                || feature.descriptor.properties.iter().any(|mapping| {
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
                    .flat_map(|feature| &feature.descriptor.properties)
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
mod tests {
    use super::*;
    use crate::{
        device::{AccountId, DeviceDid, FeatureRole, HomeId, PropertyState},
        storage::{Store, TokenSet, XiaomiRecord},
        xiaomi::catalog::compile_spec,
    };
    use flume::{Receiver, Sender};
    use tempfile::TempDir;

    use super::super::{
        AdmissionFeature, CommandRuntime, CommandTransport, GatewayPathEvidence, TransportCommand,
        TransportFailure,
    };

    type ReadReply = Sender<Result<Vec<StateReadResult>, StateReadFailure>>;
    type ReadCall = (StateReadRequest, ReadReply);
    type ActorHarness = (
        TempDir,
        DeviceService,
        StateRuntime,
        CommandRuntime,
        Receiver<ReadCall>,
        FeatureIdentity,
        FeatureDescriptor,
    );

    thread_local! {
        static COMMAND_SENDS: Cell<usize> = const { Cell::new(0) };
    }

    struct NoopCommandTransport;

    impl CommandTransport for NoopCommandTransport {
        fn send(
            &self,
            _path: ControlPath,
            _command: TransportCommand,
            _timeout: Duration,
            guard: super::super::SendGuard,
        ) -> LocalBoxFuture<'static, Result<(), TransportFailure>> {
            async move {
                if !guard.permitted() {
                    return Err(TransportFailure::Unavailable);
                }
                COMMAND_SENDS.with(|sends| sends.set(sends.get() + 1));
                guard.shared_state().mark_sent();
                Ok(())
            }
            .boxed_local()
        }
    }

    struct BlockingReadTransport {
        calls: Sender<ReadCall>,
    }

    impl StateReadTransport for BlockingReadTransport {
        fn read(
            &self,
            request: StateReadRequest,
            _timeout: Duration,
            guard: StateReadGuard,
        ) -> LocalBoxFuture<'static, Result<Vec<StateReadResult>, StateReadFailure>> {
            let (reply, receiver) = flume::bounded(1);
            self.calls.send((request, reply)).unwrap();
            async move {
                if !guard.permitted() {
                    return Err(StateReadFailure::Unavailable);
                }
                receiver.recv_async().await.unwrap()
            }
            .boxed_local()
        }
    }

    fn credentials() -> XiaomiRecord {
        XiaomiRecord {
            uid: "10001".into(),
            region: "cn".into(),
            oauth_client_uuid: "123e4567-e89b-12d3-a456-426614174000".into(),
            redirect_uri: "http://127.0.0.1/callback".into(),
            tokens: TokenSet {
                access_token: "access".into(),
                refresh_token: "refresh".into(),
                expires_at: 10_000,
                refresh_at: 5_000,
            },
            virtual_did: "123456789012345".into(),
            private_key_pem: "key".into(),
            certificate_pem: "cert".into(),
        }
    }

    fn actor() -> ActorHarness {
        actor_with_limits(StateLimits::default())
    }

    fn actor_with_limits(limits: StateLimits) -> ActorHarness {
        actor_with_descriptor(limits, descriptor())
    }

    fn actor_with_descriptor(
        mut limits: StateLimits,
        descriptor: FeatureDescriptor,
    ) -> ActorHarness {
        if limits.minimum_read_interval == StateLimits::default().minimum_read_interval {
            limits.minimum_read_interval = Duration::ZERO;
        }
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        store.xiaomi().replace(&credentials()).unwrap();
        let service = DeviceService::new();
        let feature = identity();
        store.devices().allocate_feature(&feature).unwrap();
        service.publish(feature.clone(), "Light", descriptor.capabilities.clone());
        let command = CommandRuntime::new(service.clone(), Rc::new(NoopCommandTransport));
        let (calls, receiver) = flume::bounded(8);
        let runtime = StateRuntime::with_limits(
            service.clone(),
            store.devices(),
            Rc::new(BlockingReadTransport { calls }),
            command.execution_gate(),
            command.subscribe_completions(),
            limits,
        );
        let auth_generation = store.xiaomi().snapshot().unwrap().session_generation;
        let registered = RuntimeFeature {
            identity: feature.clone(),
            descriptor: descriptor.clone(),
            authority_generation: 1,
            auth_session_generation: auth_generation,
        };
        command.register(registered.clone());
        runtime.reconcile(&AdmissionSnapshot {
            binding: None,
            status: AdmissionStatus::Active,
            epoch: crate::xiaomi::discovery::NetworkEpoch::new(7),
            features: vec![AdmissionFeature {
                identity: feature.clone(),
                runtime: registered,
                paths: OperationPaths {
                    gateway: true,
                    ..OperationPaths::default()
                },
                gateways: vec![GatewayPathEvidence {
                    gateway_did: 99,
                    access: true,
                    push: true,
                    online: Some(true),
                }],
                lan_evidence: None,
            }],
        });
        (
            directory, service, runtime, command, receiver, feature, descriptor,
        )
    }

    #[test]
    fn admitted_write_only_power_can_be_controlled_without_inventing_a_read() {
        COMMAND_SENDS.with(|sends| sends.set(0));
        let mut specification: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/miot_specs/yeelink.light.ml9.json"
        ))
        .unwrap();
        let power = specification["services"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|service| service["iid"] == 2)
            .unwrap()["properties"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|property| property["iid"] == 1)
            .unwrap();
        power["access"] = serde_json::json!(["write"]);
        let descriptor = compile_spec("yeelink.light.ml9", &specification.to_string())
            .unwrap()
            .features
            .into_iter()
            .next()
            .unwrap();
        let power = descriptor
            .properties
            .iter()
            .find(|mapping| mapping.property == Property::Power)
            .unwrap();
        assert!(!power.readable);
        assert!(!power.notify);
        let (_directory, service, _runtime, command, _calls, feature, _descriptor) =
            actor_with_descriptor(StateLimits::default(), descriptor);

        assert!(
            service
                .snapshot(&feature)
                .unwrap()
                .property(Property::Power)
                .is_none()
        );
        assert!(service.is_available(&feature));
        future::block_on(async {
            let ticket = service.command(&feature, DeviceCommand::SetPower(true));
            command.run_until_idle().await;
            assert_eq!(ticket.await, CommandOutcome::Accepted);
        });
        assert_eq!(COMMAND_SENDS.with(Cell::get), 1);
        assert!(
            service
                .snapshot(&feature)
                .unwrap()
                .property(Property::Power)
                .is_none()
        );

        service.revoke(&feature);
        future::block_on(async {
            let ticket = service.command(&feature, DeviceCommand::SetPower(false));
            command.run_until_idle().await;
            assert_eq!(ticket.await, CommandOutcome::Unavailable);
        });
        assert_eq!(COMMAND_SENDS.with(Cell::get), 1);
    }

    #[test]
    fn failed_transport_read_records_feature_path_and_safe_failure() {
        let (_directory, _service, runtime, _command, calls, feature, _descriptor) = actor();
        future::block_on(async {
            let run = runtime.run_until_idle();
            futures_util::pin_mut!(run);
            assert!(future::poll_once(&mut run).await.is_none());
            let (request, reply) = calls.recv_async().await.unwrap();
            assert_eq!(request.path, ControlPath::Gateway);
            reply.send(Err(StateReadFailure::Unavailable)).unwrap();
            run.await;
        });
        assert!(runtime.drain_diagnostics().iter().any(|diagnostic| {
            matches!(
                diagnostic,
                StateDiagnostic::ReadFailure {
                    feature: actual,
                    path: ControlPath::Gateway,
                    failure: StateReadFailure::Unavailable,
                } if actual == &feature
            )
        }));
    }

    fn identity() -> FeatureIdentity {
        FeatureIdentity {
            physical: PhysicalDeviceId {
                account: AccountId::new("10001").unwrap(),
                home: HomeId::new("home-a").unwrap(),
                parent_did: DeviceDid::new("device-a").unwrap(),
            },
            service_instance: 2,
            role: FeatureRole::Light,
        }
    }

    fn descriptor() -> FeatureDescriptor {
        compile_spec(
            "yeelink.light.ml9",
            include_str!("../../../tests/fixtures/miot_specs/yeelink.light.ml9.json"),
        )
        .unwrap()
        .features
        .into_iter()
        .next()
        .unwrap()
    }

    #[test]
    fn healthy_subscription_still_reads_unconfirmed_notify_properties() {
        let (_directory, service, runtime, _command, calls, feature, descriptor) = actor();
        assert!(!service.is_available(&feature));
        let token = runtime
            .select_push_source(&feature.physical, PushSource::Gateway(99), 41, 1)
            .unwrap();
        assert!(runtime.acknowledge(&token, 41, 1));
        future::block_on(async {
            let run = runtime.run_until_idle();
            futures_util::pin_mut!(run);
            assert!(future::poll_once(&mut run).await.is_none());
            let (request, reply) = calls.recv_async().await.unwrap();
            assert!(request.targets.iter().any(|target| {
                descriptor.properties.iter().any(|mapping| {
                    mapping.property == Property::Power
                        && mapping.siid == target.siid
                        && mapping.piid == target.piid
                })
            }));
            let power = descriptor
                .properties
                .iter()
                .find(|mapping| mapping.property == Property::Power)
                .unwrap();
            reply
                .send(Ok(vec![StateReadResult {
                    siid: power.siid,
                    piid: power.piid,
                    value: Some(WireValue::Boolean(true)),
                }]))
                .unwrap();
            run.await;
        });
        assert!(matches!(
            service
                .snapshot(&feature)
                .unwrap()
                .property(Property::Power),
            Some(PropertyState::Current { .. })
        ));
        assert!(service.is_available(&feature));
    }

    #[test]
    fn queued_new_query_blocks_older_result_before_second_read_starts() {
        let (_directory, service, runtime, _command, calls, feature, descriptor) = actor();
        future::block_on(async {
            let run = runtime.run_until_idle();
            futures_util::pin_mut!(run);
            assert!(future::poll_once(&mut run).await.is_none());
            let (_first, first_reply) = calls.recv_async().await.unwrap();
            runtime.request_refresh(&feature.physical);
            let power = descriptor
                .properties
                .iter()
                .find(|mapping| mapping.property == Property::Power)
                .unwrap();
            first_reply
                .send(Ok(vec![StateReadResult {
                    siid: power.siid,
                    piid: power.piid,
                    value: Some(WireValue::Boolean(false)),
                }]))
                .unwrap();
            assert!(future::poll_once(&mut run).await.is_none());
            assert!(
                service
                    .snapshot(&feature)
                    .unwrap()
                    .property(Property::Power)
                    .is_none()
            );
            let (_second, second_reply) = calls.recv_async().await.unwrap();
            second_reply
                .send(Ok(vec![StateReadResult {
                    siid: power.siid,
                    piid: power.piid,
                    value: Some(WireValue::Boolean(true)),
                }]))
                .unwrap();
            run.await;
        });
        assert!(matches!(
            service
                .snapshot(&feature)
                .unwrap()
                .property(Property::Power),
            Some(PropertyState::Current {
                value: crate::device::PropertyValue::Power(true),
                ..
            })
        ));
    }

    #[test]
    fn newer_push_wins_over_a_blocked_read_and_old_value_is_not_cached() {
        let (directory, service, runtime, _command, calls, feature, descriptor) = actor();
        let token = runtime
            .select_push_source(&feature.physical, PushSource::Gateway(99), 51, 1)
            .unwrap();
        assert!(runtime.acknowledge(&token, 51, 1));
        future::block_on(async {
            let run = runtime.run_until_idle();
            futures_util::pin_mut!(run);
            assert!(future::poll_once(&mut run).await.is_none());
            let (_request, reply) = calls.recv_async().await.unwrap();
            let power = descriptor
                .properties
                .iter()
                .find(|mapping| mapping.property == Property::Power)
                .unwrap();
            assert!(runtime.apply_property(
                &token,
                51,
                1,
                power.siid,
                power.piid,
                Some(&WireValue::Boolean(true)),
                2,
                false,
            ));
            reply
                .send(Ok(vec![StateReadResult {
                    siid: power.siid,
                    piid: power.piid,
                    value: Some(WireValue::Boolean(false)),
                }]))
                .unwrap();
            run.await;
        });
        assert!(matches!(
            service
                .snapshot(&feature)
                .unwrap()
                .property(Property::Power),
            Some(PropertyState::Current {
                value: crate::device::PropertyValue::Power(true),
                ..
            })
        ));
        let cached = runtime
            .inner
            .borrow()
            .dirty_states
            .values()
            .next()
            .unwrap()
            .clone();
        assert_eq!(cached.value, crate::device::PropertyValue::Power(true));
        runtime.flush_cache();
        let reopened = Store::open(directory.path()).unwrap();
        let states = reopened.devices().load_states().unwrap();
        assert_eq!(states.len(), 1);
        let restored = DeviceService::new();
        restored.restore(feature.clone(), "Light", descriptor.capabilities.clone());
        restored.restore_last_known(StateReport::new(
            feature.clone(),
            states[0].report_version,
            StateSource::Cache,
            states[0].observed_at,
            [(states[0].property, states[0].value.clone())],
        ));
        assert!(matches!(
            restored
                .snapshot(&feature)
                .unwrap()
                .property(Property::Power),
            Some(PropertyState::LastKnown { .. })
        ));
    }

    #[test]
    fn one_flush_persists_more_than_the_old_cache_limit_and_the_latest_value() {
        let (directory, service, runtime, _command, _calls, feature, descriptor) = actor();
        let mut expected = Vec::new();
        for index in 0..300 {
            let mut identity = feature.clone();
            identity.physical.parent_did = DeviceDid::new(format!("device-{index}")).unwrap();
            runtime.store.allocate_feature(&identity).unwrap();
            service.publish(
                identity.clone(),
                format!("Light {index}"),
                descriptor.capabilities.clone(),
            );
            runtime.apply_confirmed(
                identity.clone(),
                Property::Power,
                crate::device::PropertyValue::Power(true),
                runtime.service.next_report_version(),
                StateSource::Gateway,
                10,
            );
            expected.push(identity);
        }
        runtime.apply_confirmed(
            expected[0].clone(),
            Property::Power,
            crate::device::PropertyValue::Power(false),
            runtime.service.next_report_version(),
            StateSource::Gateway,
            11,
        );

        runtime.flush_cache();
        let stored = Store::open(directory.path())
            .unwrap()
            .devices()
            .load_states()
            .unwrap();
        assert_eq!(stored.len(), 300);
        let latest = stored
            .iter()
            .find(|state| state.feature == expected[0])
            .unwrap();
        assert_eq!(latest.value, crate::device::PropertyValue::Power(false));
        assert_eq!(latest.observed_at, 11);
    }

    #[test]
    fn logout_before_flush_preserves_the_latest_value_for_last_known_restore() {
        let (directory, service, runtime, _command, _calls, feature, descriptor) = actor();
        runtime.apply_confirmed(
            feature.clone(),
            Property::Power,
            crate::device::PropertyValue::Power(true),
            runtime.service.next_report_version(),
            StateSource::Cloud,
            42,
        );
        service.logout();
        runtime.reconcile(&AdmissionSnapshot {
            binding: None,
            status: AdmissionStatus::Unbound,
            epoch: crate::xiaomi::discovery::NetworkEpoch::new(7),
            features: vec![],
        });
        runtime.flush_cache();

        let states = Store::open(directory.path())
            .unwrap()
            .devices()
            .load_states()
            .unwrap();
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].value, crate::device::PropertyValue::Power(true));
        let restored = DeviceService::new();
        restored.restore(feature.clone(), "Light", descriptor.capabilities);
        restored.restore_last_known(StateReport::new(
            feature.clone(),
            states[0].report_version,
            StateSource::Cache,
            states[0].observed_at,
            [(states[0].property, states[0].value.clone())],
        ));
        assert!(matches!(
            restored
                .snapshot(&feature)
                .unwrap()
                .property(Property::Power),
            Some(PropertyState::LastKnown {
                value: crate::device::PropertyValue::Power(true),
                ..
            })
        ));
    }

    #[test]
    fn removing_a_feature_discards_its_unflushed_state() {
        let (directory, service, runtime, _command, _calls, feature, _descriptor) = actor();
        runtime.apply_confirmed(
            feature.clone(),
            Property::Power,
            crate::device::PropertyValue::Power(true),
            runtime.service.next_report_version(),
            StateSource::Gateway,
            42,
        );
        service.remove(&feature);
        runtime.reconcile(&AdmissionSnapshot {
            binding: None,
            status: AdmissionStatus::Active,
            epoch: crate::xiaomi::discovery::NetworkEpoch::new(7),
            features: vec![],
        });
        runtime.flush_cache();

        assert!(
            Store::open(directory.path())
                .unwrap()
                .devices()
                .load_states()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn stopped_runtime_and_retained_reports_cannot_update_state() {
        let (_directory, service, runtime, _command, _calls, feature, descriptor) = actor();
        let old = runtime
            .select_push_source(&feature.physical, PushSource::Gateway(99), 61, 1)
            .unwrap();
        assert!(runtime.acknowledge(&old, 61, 1));
        let current = runtime
            .select_push_source(&feature.physical, PushSource::Cloud, 62, 1)
            .unwrap();
        assert!(runtime.acknowledge(&current, 62, 1));
        let power = descriptor
            .properties
            .iter()
            .find(|mapping| mapping.property == Property::Power)
            .unwrap();
        assert!(!runtime.apply_property(
            &old,
            61,
            1,
            power.siid,
            power.piid,
            Some(&WireValue::Boolean(false)),
            2,
            false,
        ));
        assert!(!runtime.apply_property(
            &current,
            62,
            1,
            power.siid,
            power.piid,
            Some(&WireValue::Boolean(false)),
            2,
            true,
        ));
        runtime.stop();
        assert!(!runtime.apply_property(
            &current,
            62,
            1,
            power.siid,
            power.piid,
            Some(&WireValue::Boolean(true)),
            3,
            false,
        ));
        assert!(
            service
                .snapshot(&feature)
                .unwrap()
                .property(Property::Power)
                .is_none()
        );
    }

    #[test]
    fn accepted_encoded_operation_schedules_only_its_actual_readback() {
        let (_directory, service, runtime, command, calls, feature, descriptor) = actor();
        let power = descriptor
            .properties
            .iter()
            .find(|mapping| mapping.property == Property::Power)
            .unwrap();
        future::block_on(async {
            let initial = runtime.run_until_idle();
            futures_util::pin_mut!(initial);
            assert!(future::poll_once(&mut initial).await.is_none());
            let (_request, reply) = calls.recv_async().await.unwrap();
            reply
                .send(Ok(vec![StateReadResult {
                    siid: power.siid,
                    piid: power.piid,
                    value: Some(WireValue::Boolean(true)),
                }]))
                .unwrap();
            initial.await;

            let ticket = service.command(
                &feature,
                DeviceCommand::SetBrightness(crate::device::Percent::new(0.0).unwrap()),
            );
            command.run_until_idle().await;
            assert_eq!(ticket.await, CommandOutcome::Accepted);

            let readback = runtime.run_until_idle();
            futures_util::pin_mut!(readback);
            assert!(future::poll_once(&mut readback).await.is_none());
            let (request, reply) = calls.recv_async().await.unwrap();
            assert_eq!(
                request.targets,
                vec![ReadTarget {
                    siid: power.siid,
                    piid: power.piid,
                }]
            );
            reply.send(Ok(Vec::new())).unwrap();
            readback.await;
        });
    }

    #[test]
    fn read_and_control_share_the_physical_device_execution_gate() {
        let (_directory, service, runtime, command, calls, feature, _descriptor) = actor();
        service.set_state_availability(&feature, true);
        future::block_on(async {
            let read_run = runtime.run_until_idle();
            futures_util::pin_mut!(read_run);
            assert!(future::poll_once(&mut read_run).await.is_none());
            let (_request, read_reply) = calls.recv_async().await.unwrap();

            let ticket = service.command(&feature, DeviceCommand::SetPower(true));
            futures_util::pin_mut!(ticket);
            let command_run = command.run_until_idle();
            futures_util::pin_mut!(command_run);
            assert!(future::poll_once(&mut command_run).await.is_none());
            assert!(future::poll_once(&mut ticket).await.is_none());

            read_reply.send(Ok(Vec::new())).unwrap();
            read_run.await;
            command_run.await;
            assert_eq!(ticket.await, CommandOutcome::Accepted);
        });
    }

    #[test]
    fn stop_revokes_a_blocked_read_and_retires_the_request() {
        let (_directory, _service, runtime, _command, calls, _feature, _descriptor) = actor();
        future::block_on(async {
            let run = runtime.run();
            futures_util::pin_mut!(run);
            assert!(future::poll_once(&mut run).await.is_none());
            let (_request, reply) = calls.recv_async().await.unwrap();
            runtime.stop();
            run.await;
            assert!(reply.send(Ok(Vec::new())).is_err());
            assert!(runtime.inner.borrow().active.is_empty());
            assert!(runtime.inner.borrow().pending.is_empty());
        });
    }

    #[test]
    fn dropping_a_persistent_runner_finally_revokes_state_work() {
        let (_directory, _service, runtime, _command, calls, feature, _descriptor) = actor();
        future::block_on(async {
            let mut run = Box::pin(runtime.run());
            assert!(future::poll_once(&mut run).await.is_none());
            let (_request, reply) = calls.recv_async().await.unwrap();
            drop(run);
            assert!(reply.send(Ok(Vec::new())).is_err());
            assert!(
                runtime
                    .select_push_source(&feature.physical, PushSource::Gateway(99), 81, 1)
                    .is_none()
            );
            assert!(runtime.inner.borrow().active.is_empty());
        });
    }

    #[test]
    fn gate_blocked_readback_expires_without_starting_transport() {
        let (_directory, _service, runtime, command, calls, feature, descriptor) = actor();
        let power = descriptor
            .properties
            .iter()
            .find(|mapping| mapping.property == Property::Power)
            .unwrap();
        let targets = BTreeSet::from([(power.siid, power.piid)]);
        future::block_on(async {
            let lease = command
                .execution_gate()
                .acquire(
                    feature.physical.clone(),
                    Instant::now() + Duration::from_secs(1),
                )
                .await
                .unwrap();
            {
                let mut data = runtime.inner.borrow_mut();
                data.pending.clear();
                data.pending_order.clear();
            }
            runtime.schedule_selected(
                &feature.physical,
                false,
                true,
                Some(&targets),
                Some(Instant::now() + Duration::from_millis(2)),
            );
            runtime.run_until_idle().await;
            assert!(calls.try_recv().is_err());
            drop(lease);
        });
    }

    #[test]
    fn gate_wait_drops_only_expired_targets_from_a_mixed_readback() {
        let (_directory, _service, runtime, command, calls, feature, descriptor) = actor();
        let power = descriptor
            .properties
            .iter()
            .find(|mapping| mapping.property == Property::Power)
            .unwrap();
        let brightness = descriptor
            .properties
            .iter()
            .find(|mapping| mapping.property == Property::Brightness)
            .unwrap();
        future::block_on(async {
            let lease = command
                .execution_gate()
                .acquire(
                    feature.physical.clone(),
                    Instant::now() + Duration::from_secs(1),
                )
                .await
                .unwrap();
            {
                let mut data = runtime.inner.borrow_mut();
                data.pending.clear();
                data.pending_order.clear();
            }
            runtime.schedule_selected(
                &feature.physical,
                false,
                true,
                Some(&BTreeSet::from([(power.siid, power.piid)])),
                Some(Instant::now() + Duration::from_millis(2)),
            );
            runtime.schedule_selected(
                &feature.physical,
                false,
                true,
                Some(&BTreeSet::from([(brightness.siid, brightness.piid)])),
                Some(Instant::now() + Duration::from_millis(100)),
            );
            let run = runtime.run_until_idle();
            futures_util::pin_mut!(run);
            assert!(future::poll_once(&mut run).await.is_none());
            Timer::after(Duration::from_millis(5)).await;
            drop(lease);
            assert!(future::poll_once(&mut run).await.is_none());
            let (request, reply) = calls.recv_async().await.unwrap();
            assert_eq!(
                request.targets,
                vec![ReadTarget {
                    siid: brightness.siid,
                    piid: brightness.piid,
                }]
            );
            reply.send(Ok(Vec::new())).unwrap();
            run.await;
        });
    }

    #[test]
    fn core_success_does_not_postpone_battery_cadence_or_gate_availability() {
        let (_directory, service, runtime, _command, calls, feature, descriptor) = actor();
        let mut battery = descriptor
            .properties
            .iter()
            .find(|mapping| mapping.property == Property::Brightness)
            .unwrap()
            .clone();
        battery.property = Property::Battery;
        battery.piid += 100;
        battery.class = PropertyClass::Ancillary;
        battery.notify = false;
        {
            let mut data = runtime.inner.borrow_mut();
            data.features
                .get_mut(&feature)
                .unwrap()
                .descriptor
                .properties
                .push(battery.clone());
            data.pending.clear();
            data.pending_order.clear();
        }
        runtime.schedule(&feature.physical, true);
        future::block_on(async {
            let first = runtime.run_until_idle();
            futures_util::pin_mut!(first);
            assert!(future::poll_once(&mut first).await.is_none());
            let (request, reply) = calls.recv_async().await.unwrap();
            assert!(request.targets.contains(&ReadTarget {
                siid: battery.siid,
                piid: battery.piid,
            }));
            let power = descriptor
                .properties
                .iter()
                .find(|mapping| mapping.property == Property::Power)
                .unwrap();
            reply
                .send(Ok(vec![
                    StateReadResult {
                        siid: power.siid,
                        piid: power.piid,
                        value: Some(WireValue::Boolean(true)),
                    },
                    StateReadResult {
                        siid: battery.siid,
                        piid: battery.piid,
                        value: None,
                    },
                ]))
                .unwrap();
            first.await;
            assert!(service.is_available(&feature));
            let battery_deadline = runtime
                .inner
                .borrow()
                .next_battery
                .get(&feature.physical)
                .copied()
                .unwrap();

            runtime.schedule(&feature.physical, false);
            let core = runtime.run_until_idle();
            futures_util::pin_mut!(core);
            assert!(future::poll_once(&mut core).await.is_none());
            let (request, reply) = calls.recv_async().await.unwrap();
            assert!(!request.targets.contains(&ReadTarget {
                siid: battery.siid,
                piid: battery.piid,
            }));
            reply.send(Ok(Vec::new())).unwrap();
            core.await;
            assert_eq!(
                runtime.inner.borrow().next_battery.get(&feature.physical),
                Some(&battery_deadline)
            );
        });
    }

    #[test]
    fn healthy_covered_property_is_suppressed_while_uncovered_property_polls() {
        let (_directory, _service, runtime, _command, calls, feature, descriptor) = actor();
        let power = descriptor
            .properties
            .iter()
            .find(|mapping| mapping.property == Property::Power)
            .unwrap()
            .clone();
        let mut brightness = descriptor
            .properties
            .iter()
            .find(|mapping| mapping.property == Property::Brightness)
            .unwrap()
            .clone();
        brightness.notify = false;
        {
            let mut data = runtime.inner.borrow_mut();
            data.features
                .get_mut(&feature)
                .unwrap()
                .descriptor
                .properties = vec![power.clone(), brightness.clone()];
            data.pending.clear();
            data.pending_order.clear();
        }
        let token = runtime
            .select_push_source(&feature.physical, PushSource::Gateway(99), 91, 1)
            .unwrap();
        assert!(runtime.acknowledge(&token, 91, 1));
        {
            let mut data = runtime.inner.borrow_mut();
            data.pending.clear();
            data.pending_order.clear();
        }
        assert!(runtime.apply_property(
            &token,
            91,
            1,
            power.siid,
            power.piid,
            Some(&WireValue::Boolean(true)),
            10,
            false,
        ));
        assert!(runtime.apply_property(
            &token,
            91,
            1,
            brightness.siid,
            brightness.piid,
            Some(&WireValue::Integer(50)),
            10,
            false,
        ));
        let poll_deadline = runtime
            .inner
            .borrow()
            .next_due
            .get(&feature.physical)
            .copied()
            .unwrap();
        assert!(runtime.apply_property(
            &token,
            91,
            1,
            power.siid,
            power.piid,
            Some(&WireValue::Boolean(false)),
            11,
            false,
        ));
        assert_eq!(
            runtime.inner.borrow().next_due.get(&feature.physical),
            Some(&poll_deadline)
        );
        runtime.schedule(&feature.physical, false);
        future::block_on(async {
            let run = runtime.run_until_idle();
            futures_util::pin_mut!(run);
            assert!(future::poll_once(&mut run).await.is_none());
            let (request, reply) = calls.recv_async().await.unwrap();
            assert_eq!(
                request.targets,
                vec![ReadTarget {
                    siid: brightness.siid,
                    piid: brightness.piid,
                }]
            );
            reply.send(Ok(Vec::new())).unwrap();
            run.await;
        });
    }

    #[test]
    fn partial_success_with_a_healthy_source_retries_unconfirmed_notify_values() {
        let limits = StateLimits {
            retry_initial: Duration::from_millis(2),
            retry_max: Duration::from_millis(2),
            ..StateLimits::default()
        };
        let (_directory, _service, runtime, _command, calls, feature, descriptor) =
            actor_with_limits(limits);
        let token = runtime
            .select_push_source(&feature.physical, PushSource::Gateway(99), 101, 1)
            .unwrap();
        assert!(runtime.acknowledge(&token, 101, 1));
        let power = descriptor
            .properties
            .iter()
            .find(|mapping| mapping.property == Property::Power)
            .unwrap();
        future::block_on(async {
            let run = runtime.run();
            futures_util::pin_mut!(run);
            assert!(future::poll_once(&mut run).await.is_none());
            let (_request, reply) = calls.recv_async().await.unwrap();
            reply
                .send(Ok(vec![StateReadResult {
                    siid: power.siid,
                    piid: power.piid,
                    value: Some(WireValue::Boolean(true)),
                }]))
                .unwrap();
            assert!(future::poll_once(&mut run).await.is_none());
            Timer::after(Duration::from_millis(4)).await;
            assert!(future::poll_once(&mut run).await.is_none());
            let (_retry, reply) = calls.recv_async().await.unwrap();
            reply.send(Ok(Vec::new())).unwrap();
            runtime.stop();
            run.await;
        });
    }

    #[test]
    fn batches_smaller_than_the_property_set_eventually_read_every_target() {
        let limits = StateLimits {
            batch_size: 1,
            ..StateLimits::default()
        };
        let (_directory, _service, runtime, _command, calls, _feature, descriptor) =
            actor_with_limits(limits);
        let expected = descriptor
            .properties
            .iter()
            .filter(|mapping| mapping.readable)
            .map(|mapping| (mapping.siid, mapping.piid))
            .collect::<BTreeSet<_>>();
        future::block_on(async {
            let run = runtime.run_until_idle();
            futures_util::pin_mut!(run);
            let mut observed = BTreeSet::new();
            while observed.len() < expected.len() {
                assert!(future::poll_once(&mut run).await.is_none());
                let (request, reply) = calls.recv_async().await.unwrap();
                assert_eq!(request.targets.len(), 1);
                observed.insert((request.targets[0].siid, request.targets[0].piid));
                reply.send(Ok(Vec::new())).unwrap();
            }
            run.await;
            assert_eq!(observed, expected);
        });
    }

    #[test]
    fn failed_read_uses_retry_timer_and_success_resets_backoff() {
        let limits = StateLimits {
            retry_initial: Duration::from_millis(2),
            retry_max: Duration::from_millis(8),
            poll_interval: Duration::from_secs(60),
            ..StateLimits::default()
        };
        let (_directory, _service, runtime, _command, calls, feature, descriptor) =
            actor_with_limits(limits);
        future::block_on(async {
            let run = runtime.run();
            futures_util::pin_mut!(run);
            assert!(future::poll_once(&mut run).await.is_none());
            let (_first, reply) = calls.recv_async().await.unwrap();
            reply.send(Err(StateReadFailure::Unavailable)).unwrap();
            assert!(future::poll_once(&mut run).await.is_none());
            Timer::after(Duration::from_millis(4)).await;
            assert!(future::poll_once(&mut run).await.is_none());
            let (retry, reply) = calls.recv_async().await.unwrap();
            let results = retry
                .targets
                .iter()
                .map(|target| {
                    let property = descriptor
                        .properties
                        .iter()
                        .find(|mapping| mapping.siid == target.siid && mapping.piid == target.piid)
                        .unwrap()
                        .property;
                    let value = match property {
                        Property::Power => WireValue::Boolean(true),
                        Property::Brightness => WireValue::Integer(50),
                        Property::ColorTemperature => WireValue::Integer(4_000),
                        _ => panic!("unexpected light property {property:?}"),
                    };
                    StateReadResult {
                        siid: target.siid,
                        piid: target.piid,
                        value: Some(value),
                    }
                })
                .collect();
            reply.send(Ok(results)).unwrap();
            assert!(future::poll_once(&mut run).await.is_none());
            assert!(
                !runtime
                    .inner
                    .borrow()
                    .retry_attempts
                    .contains_key(&feature.physical)
            );
            runtime.stop();
            run.await;
        });
    }

    #[test]
    fn repeated_refreshes_coalesce_into_one_pending_round() {
        let (_directory, _service, runtime, _command, calls, feature, _descriptor) = actor();
        future::block_on(async {
            let run = runtime.run_until_idle();
            futures_util::pin_mut!(run);
            assert!(future::poll_once(&mut run).await.is_none());
            let (_first, first_reply) = calls.recv_async().await.unwrap();
            for _ in 0..100 {
                runtime.request_refresh(&feature.physical);
            }
            assert_eq!(runtime.inner.borrow().pending.len(), 1);
            first_reply.send(Ok(Vec::new())).unwrap();
            assert!(future::poll_once(&mut run).await.is_none());
            let (_second, second_reply) = calls.recv_async().await.unwrap();
            second_reply.send(Ok(Vec::new())).unwrap();
            run.await;
            assert!(calls.try_recv().is_err());
        });
    }

    #[test]
    fn positional_event_is_normalized_and_retained_action_is_discarded() {
        let (directory, service, runtime, _command, _calls, light, _descriptor) = actor();
        let mut descriptor = compile_spec(
            "xiaomi.motion.pir1",
            include_str!("../../../tests/fixtures/miot_specs/xiaomi.motion.pir1.json"),
        )
        .unwrap()
        .features
        .into_iter()
        .find(|feature| feature.role == FeatureRole::MotionSensor)
        .unwrap();
        descriptor
            .properties
            .retain(|mapping| mapping.property != Property::Motion);
        let feature = FeatureIdentity {
            physical: light.physical.clone(),
            service_instance: descriptor.service_instance,
            role: descriptor.role,
        };
        runtime.store.allocate_feature(&feature).unwrap();
        service.publish(feature.clone(), "Motion", descriptor.capabilities.clone());
        let template = runtime.inner.borrow().features[&light].clone();
        let registered = RuntimeFeature {
            identity: feature.clone(),
            descriptor: descriptor.clone(),
            ..template
        };
        runtime
            .inner
            .borrow_mut()
            .features
            .insert(feature.clone(), registered.clone());
        let token = runtime
            .select_push_source(&feature.physical, PushSource::Gateway(99), 81, 1)
            .unwrap();
        assert!(runtime.acknowledge(&token, 81, 1));
        let event = descriptor
            .events
            .iter()
            .find(|event| event.argument_count == 0)
            .unwrap();
        assert!(!runtime.apply_positional_event(
            &token,
            81,
            1,
            event.siid,
            event.eiid,
            &[],
            1,
            true,
        ));
        assert!(runtime.apply_positional_event(
            &token,
            81,
            1,
            event.siid,
            event.eiid,
            &[],
            2,
            false,
        ));
        assert!(matches!(
            service
                .snapshot(&feature)
                .unwrap()
                .property(Property::Motion),
            Some(PropertyState::Current {
                value: crate::device::PropertyValue::Motion(false),
                ..
            })
        ));
        runtime.reconcile(&AdmissionSnapshot {
            binding: None,
            status: AdmissionStatus::Active,
            epoch: crate::xiaomi::discovery::NetworkEpoch::new(7),
            features: vec![AdmissionFeature {
                identity: feature.clone(),
                runtime: registered,
                paths: OperationPaths {
                    gateway: true,
                    ..OperationPaths::default()
                },
                gateways: vec![GatewayPathEvidence {
                    gateway_did: 99,
                    access: true,
                    push: true,
                    online: Some(true),
                }],
                lan_evidence: None,
            }],
        });
        runtime.flush_cache();
        let stored = Store::open(directory.path())
            .unwrap()
            .devices()
            .load_states()
            .unwrap();
        assert!(stored.iter().any(|state| {
            state.feature == feature
                && state.property == Property::Motion
                && state.value == crate::device::PropertyValue::Motion(false)
        }));
    }
}
