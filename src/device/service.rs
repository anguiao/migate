use super::{
    CommandValidationError, DeviceCommand, FeatureCapabilities, FeatureIdentity, Property,
    PropertyState, StateReport, StateSnapshot,
};
use event_listener::Event;
use futures_util::future::LocalBoxFuture;
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, VecDeque},
    rc::Rc,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommandOutcome {
    Accepted,
    Unavailable,
    Unsupported,
    Expired,
    Cancelled,
    Superseded,
    Rejected(i64),
    Ambiguous,
}

pub const MAX_COMMAND_BATCH: usize = 8;

pub trait DeviceCommandSink {
    fn submit(
        &self,
        feature: FeatureIdentity,
        commands: Vec<DeviceCommand>,
    ) -> LocalBoxFuture<'static, CommandOutcome>;

    fn stop_adjustment(&self, feature: &FeatureIdentity, property: Property);
}

#[derive(Clone, Debug, PartialEq)]
pub struct Feature {
    pub identity: FeatureIdentity,
    pub name: String,
    pub capabilities: FeatureCapabilities,
    pub admitted: bool,
}
#[derive(Clone, Debug, PartialEq)]
pub enum DeviceChange {
    Resync,
    FeaturePublished(FeatureIdentity),
    FeatureUpdated(FeatureIdentity),
    FeatureRemoved(FeatureIdentity),
    AvailabilityChanged {
        feature: FeatureIdentity,
        available: bool,
    },
    StateChanged {
        feature: FeatureIdentity,
        properties: Vec<Property>,
    },
}

#[derive(Clone)]
pub struct DeviceService {
    inner: Rc<RefCell<ServiceState>>,
    event: Rc<Event>,
}
struct ServiceState {
    features: BTreeMap<FeatureIdentity, Feature>,
    snapshots: BTreeMap<FeatureIdentity, StateSnapshot>,
    changes: VecDeque<(u64, DeviceChange)>,
    change_version: u64,
    next_report_version: Cell<u64>,
    command_sink: Option<Rc<dyn DeviceCommandSink>>,
    state_availability: BTreeMap<FeatureIdentity, bool>,
}
impl Default for DeviceService {
    fn default() -> Self {
        Self {
            inner: Rc::new(RefCell::new(ServiceState::default())),
            event: Rc::new(Event::new()),
        }
    }
}
impl Default for ServiceState {
    fn default() -> Self {
        Self {
            features: BTreeMap::new(),
            snapshots: BTreeMap::new(),
            changes: VecDeque::new(),
            change_version: 0,
            next_report_version: Cell::new(0),
            command_sink: None,
            state_availability: BTreeMap::new(),
        }
    }
}
pub struct DeviceSubscription {
    inner: Rc<RefCell<ServiceState>>,
    event: Rc<Event>,
    cursor: u64,
}
impl DeviceSubscription {
    pub fn drain(&mut self) -> Vec<DeviceChange> {
        let state = self.inner.borrow();
        if state
            .changes
            .front()
            .is_some_and(|(version, _)| *version > self.cursor.saturating_add(1))
        {
            self.cursor = state.change_version;
            return vec![DeviceChange::Resync];
        }
        let changes = state
            .changes
            .iter()
            .filter(|(version, _)| *version > self.cursor)
            .map(|(_, change)| change.clone())
            .collect();
        self.cursor = state.change_version;
        changes
    }
    pub async fn changed(&mut self) -> Vec<DeviceChange> {
        loop {
            let listener = self.event.listen();
            let changes = self.drain();
            if !changes.is_empty() {
                return changes;
            }
            listener.await;
        }
    }
}
impl DeviceService {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn subscribe(&self) -> DeviceSubscription {
        DeviceSubscription {
            inner: self.inner.clone(),
            event: self.event.clone(),
            cursor: self.inner.borrow().change_version,
        }
    }
    pub fn publish(&self, id: FeatureIdentity, name: impl Into<String>, caps: FeatureCapabilities) {
        self.set_feature(id, name.into(), caps, true)
    }
    pub fn restore(&self, id: FeatureIdentity, name: impl Into<String>, caps: FeatureCapabilities) {
        self.set_feature(id, name.into(), caps, false)
    }
    fn set_feature(
        &self,
        id: FeatureIdentity,
        name: String,
        caps: FeatureCapabilities,
        admitted: bool,
    ) {
        let mut s = self.inner.borrow_mut();
        let feature = Feature {
            identity: id.clone(),
            name,
            capabilities: caps,
            admitted,
        };
        let previous = s.features.insert(id.clone(), feature.clone());
        s.snapshots.entry(id.clone()).or_default();
        match previous {
            None => notify(&mut s, &self.event, DeviceChange::FeaturePublished(id)),
            Some(previous) if previous != feature => {
                notify(&mut s, &self.event, DeviceChange::FeatureUpdated(id))
            }
            Some(_) => {}
        }
    }
    pub fn feature(&self, id: &FeatureIdentity) -> Option<Feature> {
        self.inner.borrow().features.get(id).cloned()
    }
    pub fn features(&self) -> Vec<Feature> {
        self.inner.borrow().features.values().cloned().collect()
    }
    pub fn can_control(&self, id: &FeatureIdentity) -> bool {
        let state = self.inner.borrow();
        state.features.get(id).is_some_and(|f| f.admitted)
            && state.state_availability.get(id).copied().unwrap_or(true)
    }
    pub fn is_available(&self, id: &FeatureIdentity) -> bool {
        self.can_control(id)
    }
    pub fn set_state_availability(&self, id: &FeatureIdentity, available: bool) {
        let mut state = self.inner.borrow_mut();
        if !state.features.contains_key(id) {
            return;
        }
        let previous = state.state_availability.insert(id.clone(), available);
        if previous != Some(available) {
            let available = available
                && state
                    .features
                    .get(id)
                    .is_some_and(|feature| feature.admitted);
            notify(
                &mut state,
                &self.event,
                DeviceChange::AvailabilityChanged {
                    feature: id.clone(),
                    available,
                },
            );
        }
    }
    pub fn validate_command(
        &self,
        id: &FeatureIdentity,
        command: &DeviceCommand,
    ) -> Result<(), ServiceCommandError> {
        let state = self.inner.borrow();
        let feature = state
            .features
            .get(id)
            .ok_or(ServiceCommandError::UnknownFeature)?;
        if !feature.admitted
            || state
                .state_availability
                .get(id)
                .is_some_and(|available| !available)
        {
            return Err(ServiceCommandError::Unavailable);
        }
        feature
            .capabilities
            .validate(command)
            .map_err(ServiceCommandError::Invalid)
    }
    pub fn set_command_sink(&self, sink: Rc<dyn DeviceCommandSink>) {
        self.inner.borrow_mut().command_sink = Some(sink);
    }
    pub fn command(
        &self,
        id: &FeatureIdentity,
        command: DeviceCommand,
    ) -> LocalBoxFuture<'static, CommandOutcome> {
        self.command_batch(id, vec![command])
    }
    pub fn command_batch(
        &self,
        id: &FeatureIdentity,
        commands: Vec<DeviceCommand>,
    ) -> LocalBoxFuture<'static, CommandOutcome> {
        if commands.is_empty() || commands.len() > MAX_COMMAND_BATCH {
            return Box::pin(async { CommandOutcome::Unsupported });
        }
        for command in &commands {
            if let Err(error) = self.validate_command(id, command) {
                let outcome = match error {
                    ServiceCommandError::Unavailable => CommandOutcome::Unavailable,
                    ServiceCommandError::UnknownFeature | ServiceCommandError::Invalid(_) => {
                        CommandOutcome::Unsupported
                    }
                };
                return Box::pin(async move { outcome });
            }
        }
        let sink = self.inner.borrow().command_sink.clone();
        let id = id.clone();
        match sink {
            Some(sink) => sink.submit(id, commands),
            None => Box::pin(async { CommandOutcome::Unavailable }),
        }
    }
    pub fn stop_adjustment(&self, id: &FeatureIdentity, property: Property) {
        if let Some(sink) = self.inner.borrow().command_sink.clone() {
            sink.stop_adjustment(id, property);
        }
    }
    pub fn admit(&self, id: &FeatureIdentity) {
        let mut s = self.inner.borrow_mut();
        let Some(f) = s.features.get_mut(id) else {
            return;
        };
        if !f.admitted {
            f.admitted = true;
            let available = s.state_availability.get(id).copied().unwrap_or(true);
            notify(
                &mut s,
                &self.event,
                DeviceChange::AvailabilityChanged {
                    feature: id.clone(),
                    available,
                },
            );
        }
    }
    pub fn revoke(&self, id: &FeatureIdentity) {
        let mut state = self.inner.borrow_mut();
        let Some(feature) = state.features.get_mut(id) else {
            return;
        };
        if feature.admitted {
            feature.admitted = false;
            notify(
                &mut state,
                &self.event,
                DeviceChange::AvailabilityChanged {
                    feature: id.clone(),
                    available: false,
                },
            );
        }
    }
    pub fn remove(&self, id: &FeatureIdentity) {
        let mut s = self.inner.borrow_mut();
        if s.features.remove(id).is_some() {
            s.state_availability.remove(id);
            s.snapshots.remove(id);
            notify(
                &mut s,
                &self.event,
                DeviceChange::FeatureRemoved(id.clone()),
            );
        }
    }
    pub fn logout(&self) {
        let mut s = self.inner.borrow_mut();
        let ids: Vec<_> = s.features.keys().cloned().collect();
        for id in ids {
            if s.features.get_mut(&id).is_some_and(|feature| {
                let was_admitted = feature.admitted;
                feature.admitted = false;
                was_admitted
            }) {
                notify(
                    &mut s,
                    &self.event,
                    DeviceChange::AvailabilityChanged {
                        feature: id,
                        available: false,
                    },
                );
            }
        }
    }
    pub fn begin_query(&self, id: &FeatureIdentity, p: Property) -> u64 {
        let s = self.inner.borrow_mut();
        let current = s
            .snapshots
            .get(id)
            .and_then(|x| x.property(p))
            .map_or(0, PropertyState::report_version);
        let next = s.next_report_version.get().max(current).saturating_add(1);
        s.next_report_version.set(next);
        next
    }
    pub fn next_report_version(&self) -> u64 {
        let state = self.inner.borrow();
        let next = state.next_report_version.get().saturating_add(1);
        state.next_report_version.set(next);
        next
    }
    pub fn apply_report(&self, r: StateReport) -> bool {
        let mut s = self.inner.borrow_mut();
        if !s.features.contains_key(&r.feature) {
            return false;
        }
        s.next_report_version
            .set(s.next_report_version.get().max(r.report_version));
        let snapshot = s.snapshots.entry(r.feature.clone()).or_default();
        let mut changed = Vec::new();
        for (p, v) in r.values {
            if snapshot
                .properties
                .get(&p)
                .is_some_and(|old| old.report_version() > r.report_version)
            {
                continue;
            }
            snapshot.properties.insert(
                p,
                PropertyState::Current {
                    value: v,
                    source: r.source,
                    observed_at: r.observed_at,
                    report_version: r.report_version,
                },
            );
            changed.push(p);
        }
        let applied = !changed.is_empty();
        if applied {
            notify(
                &mut s,
                &self.event,
                DeviceChange::StateChanged {
                    feature: r.feature,
                    properties: changed,
                },
            );
        }
        applied
    }
    pub fn apply_unknown(&self, id: &FeatureIdentity, p: Property, version: u64) -> bool {
        let mut s = self.inner.borrow_mut();
        if !s.features.contains_key(id) {
            return false;
        }
        s.next_report_version
            .set(s.next_report_version.get().max(version));
        let snapshot = s.snapshots.entry(id.clone()).or_default();
        if snapshot
            .properties
            .get(&p)
            .is_some_and(|old| old.report_version() > version)
        {
            return false;
        }
        let last_known = snapshot
            .properties
            .get(&p)
            .and_then(PropertyState::last_known);
        snapshot.properties.insert(
            p,
            PropertyState::Unknown {
                last_known,
                report_version: version,
            },
        );
        notify(
            &mut s,
            &self.event,
            DeviceChange::StateChanged {
                feature: id.clone(),
                properties: vec![p],
            },
        );
        true
    }
    pub fn restore_last_known(&self, report: StateReport) {
        let mut state = self.inner.borrow_mut();
        if !state.features.contains_key(&report.feature) {
            return;
        }
        state
            .next_report_version
            .set(state.next_report_version.get().max(report.report_version));
        let snapshot = state.snapshots.entry(report.feature.clone()).or_default();
        for (property, value) in report.values {
            snapshot.properties.insert(
                property,
                PropertyState::LastKnown {
                    value,
                    source: report.source,
                    observed_at: report.observed_at,
                    report_version: report.report_version,
                },
            );
        }
    }
    pub fn mark_unconfirmed(&self, id: &FeatureIdentity) {
        let mut s = self.inner.borrow_mut();
        let Some(snapshot) = s.snapshots.get_mut(id) else {
            return;
        };
        let mut changed = Vec::new();
        for (p, state) in &mut snapshot.properties {
            if let PropertyState::Current {
                value,
                source,
                observed_at,
                report_version,
            } = state.clone()
            {
                *state = PropertyState::LastKnown {
                    value,
                    source,
                    observed_at,
                    report_version,
                };
                changed.push(*p);
            }
        }
        if !changed.is_empty() {
            notify(
                &mut s,
                &self.event,
                DeviceChange::StateChanged {
                    feature: id.clone(),
                    properties: changed,
                },
            );
        }
    }
    pub fn snapshot(&self, id: &FeatureIdentity) -> Option<StateSnapshot> {
        self.inner.borrow().snapshots.get(id).cloned()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServiceCommandError {
    UnknownFeature,
    Unavailable,
    Invalid(CommandValidationError),
}
impl std::fmt::Display for ServiceCommandError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownFeature => formatter.write_str("feature is unknown"),
            Self::Unavailable => formatter.write_str("feature is unavailable"),
            Self::Invalid(error) => error.fmt(formatter),
        }
    }
}
impl std::error::Error for ServiceCommandError {}
fn notify(s: &mut ServiceState, event: &Event, change: DeviceChange) {
    s.change_version = s.change_version.saturating_add(1);
    s.changes.push_back((s.change_version, change));
    if s.changes.len() > 256 {
        s.changes.pop_front();
    }
    event.notify(usize::MAX);
}
