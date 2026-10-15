use super::{
    commands::LightWriteGate,
    model::AdapterError,
    observations::Observations,
    refresh::{ReadFlight, Refresh},
    state::{Context, DeviceRecord, Lifetime},
};
use crate::device::{ActionToken, LightDimension, LightMode, LightPatch, Power, Rgb};
use crate::xiaomi::{capability::PropertyBinding, central::SessionHandle};
use event_listener::Event;
use futures_lite::future;
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet, VecDeque},
    rc::Rc,
    time::Duration,
};
/// The last actual member observation used for repaired state and auxiliary
/// decisions. Optimistic accepted targets never enter this record.
#[derive(Clone, Copy, Default, PartialEq)]
pub(super) struct ConfirmedLight {
    pub(super) power: Option<Power>,
    pub(super) brightness_percent: Option<f64>,
    pub(super) temperature_kelvin: Option<u32>,
    pub(super) color: Option<Rgb>,
    pub(super) mode: Option<LightMode>,
}
impl ConfirmedLight {
    fn merge(&mut self, patch: LightPatch) {
        self.power = patch.power.map(Power::from).or(self.power);
        self.brightness_percent = patch.brightness_percent.or(self.brightness_percent);
        self.temperature_kelvin = patch.temperature_kelvin.or(self.temperature_kelvin);
        self.color = patch.color.or(self.color);
        self.mode = patch.mode.or(self.mode);
    }
    pub(super) fn power_value(&self) -> Option<bool> {
        self.power.and_then(Power::value)
    }
}

/// One committed repaired-group member handle, so an action keeps the member
/// universe captured at its first frame.
#[derive(Clone)]
pub(super) struct RepairMemberRef {
    pub(super) record: Rc<DeviceRecord>,
    pub(super) work: Rc<DeviceWork>,
}

struct FrameOwnership {
    token: Option<ActionToken>,
    epoch: u64,
    members: Rc<Vec<RepairMemberRef>>,
    missing: usize,
    accepted_final: bool,
}

/// The newest frame ownership per light dimension on a group function. A slow
/// member task checks this before submitting so it cannot replay an obsolete
/// fade step after a newer frame replaced it, and so Stop/takeover retires
/// queued writes without cancelling a successful final acceptance.
#[derive(Default)]
pub(super) struct LightFrames {
    light: [Option<FrameOwnership>; 2],
    power: Option<FrameOwnership>,
}
impl LightFrames {
    fn index(dimension: LightDimension) -> usize {
        match dimension {
            LightDimension::Brightness => 0,
            LightDimension::Color => 1,
        }
    }
    pub(super) fn next_light(
        &mut self,
        token: ActionToken,
        snapshot: impl FnOnce() -> (Vec<RepairMemberRef>, usize),
    ) -> (u64, Rc<Vec<RepairMemberRef>>, usize) {
        let index = Self::index(token.dimension());
        if let Some(frame) = self.light[index].as_mut()
            && frame.token == Some(token)
        {
            frame.epoch += 1;
            frame.accepted_final = false;
            return (frame.epoch, frame.members.clone(), frame.missing);
        }
        let (members, missing) = snapshot();
        let members = Rc::new(members);
        let epoch = 1;
        self.light[index] = Some(FrameOwnership {
            token: Some(token),
            epoch,
            members: members.clone(),
            missing,
            accepted_final: false,
        });
        (epoch, members, missing)
    }
    pub(super) fn next_power(
        &mut self,
        snapshot: impl FnOnce() -> (Vec<RepairMemberRef>, usize),
    ) -> (u64, Rc<Vec<RepairMemberRef>>, usize) {
        let (members, missing) = snapshot();
        let members = Rc::new(members);
        let epoch = self.power.as_ref().map_or(1, |frame| frame.epoch + 1);
        self.power = Some(FrameOwnership {
            token: None,
            epoch,
            members: members.clone(),
            missing,
            accepted_final: false,
        });
        (epoch, members, missing)
    }
    pub(super) fn current_light(&self, token: ActionToken, epoch: u64) -> bool {
        matches!(self.light[Self::index(token.dimension())], Some(ref frame) if frame.token == Some(token) && frame.epoch == epoch)
    }
    pub(super) fn accepted_final_light(&self, token: ActionToken, epoch: u64) -> bool {
        matches!(self.light[Self::index(token.dimension())], Some(ref frame) if frame.token == Some(token) && frame.epoch == epoch && frame.accepted_final)
    }
    pub(super) fn current_power(&self, epoch: u64) -> bool {
        self.power
            .as_ref()
            .is_some_and(|frame| frame.epoch == epoch)
    }
    pub(super) fn settle_light(&mut self, token: ActionToken, epoch: u64, final_frame: bool) {
        if let Some(frame) = self.light[Self::index(token.dimension())].as_mut()
            && frame.token == Some(token)
            && frame.epoch == epoch
        {
            frame.accepted_final = final_frame;
        }
    }
    pub(super) fn cancel_light(&mut self, token: ActionToken, epoch: u64) {
        if self.current_light(token, epoch) {
            self.light[Self::index(token.dimension())] = None;
        }
    }
    pub(super) fn cancel_power(&mut self, epoch: u64) {
        if self.current_power(epoch) {
            self.power = None;
        }
    }
    pub(super) fn cancel_dimension(&mut self, dimension: LightDimension) {
        self.light[Self::index(dimension)] = None;
    }

    /// A committed rebuild or runtime reset retires every queued frame.
    pub(super) fn cancel_all(&mut self) {
        self.light = [None, None];
        self.power = None;
    }
}

pub(super) struct DeviceWork {
    // Binding work ends on incompatibility, while the physical device keeps its consumers.
    pub(super) life: Lifetime,
    pub(super) observations: Observations,
    pub(super) light_write: Option<Rc<LightWriteGate>>,
    pub(super) record: Rc<DeviceRecord>,
    pub(super) subscribed: RefCell<BTreeSet<(u32, u32)>>,
    pub(super) active: RefCell<Option<super::notifications::NotificationSource>>,
    pub(super) local_generation: Cell<u64>,
    pub(super) local_exhausted: Cell<bool>,
    pub(super) cloud_subscribed: RefCell<BTreeSet<(u32, u32)>>,
    pub(super) cloud_generation: Cell<u64>,
    pub(super) cloud_running: Cell<bool>,
    pub(super) cloud_exhausted: Cell<bool>,
    pub(super) cloud_problem: RefCell<Option<String>>,
    pub(super) condition_generation: Cell<u64>,
    subscribe_requested: Cell<bool>,
    pub(super) subscribe_running: Cell<bool>,
    pub(super) refresh_pending: RefCell<VecDeque<Refresh>>,
    pub(super) refresh_inflight: RefCell<BTreeMap<super::lighting::PropertyKey, Rc<ReadFlight>>>,
    /// Reads that must be repeated after the current in-flight read completes.
    /// A dirty member event sets this instead of invalidating the current read.
    pub(super) dirty_again: RefCell<BTreeSet<super::lighting::PropertyKey>>,
    pub(super) refresh_changed: Rc<Event>,
    /// Confirmed observations keyed by the binding generation that produced them.
    pub(super) confirmed: RefCell<Option<(u64, ConfirmedLight)>>,
    pub(super) light_frames: RefCell<LightFrames>,
    changed: Event,
    subscribe_problem: RefCell<Option<String>>,
    pub(super) read_problems: RefCell<BTreeMap<super::lighting::PropertyKey, String>>,
}
impl DeviceWork {
    pub(super) fn new(
        record: Rc<DeviceRecord>,
        refresh_changed: Rc<Event>,
        light_write: Option<Rc<LightWriteGate>>,
    ) -> Self {
        let work = Self {
            life: Lifetime::default(),
            observations: Observations::default(),
            light_write,
            record,
            subscribed: RefCell::new(BTreeSet::new()),
            active: RefCell::new(None),
            local_generation: Cell::new(0),
            local_exhausted: Cell::new(false),
            cloud_subscribed: RefCell::new(BTreeSet::new()),
            cloud_generation: Cell::new(0),
            cloud_running: Cell::new(false),
            cloud_exhausted: Cell::new(false),
            cloud_problem: RefCell::new(None),
            condition_generation: Cell::new(0),
            subscribe_requested: Cell::new(false),
            subscribe_running: Cell::new(false),
            refresh_pending: RefCell::new(VecDeque::new()),
            refresh_inflight: RefCell::new(BTreeMap::new()),
            dirty_again: RefCell::new(BTreeSet::new()),
            refresh_changed,
            confirmed: RefCell::new(None),
            light_frames: RefCell::new(LightFrames::default()),
            changed: Event::new(),
            subscribe_problem: RefCell::new(None),
            read_problems: RefCell::new(BTreeMap::new()),
        };
        work.refresh();
        work
    }
    pub(super) fn local_connected(&self) {
        self.local_disconnected();
        self.subscribe_requested.set(false);
        self.changed.notify(usize::MAX);
    }
    pub(super) fn source_changed(&self) {
        self.observations.source_changed();
        self.clear_confirmed();
        super::refresh::cancel(self);
        self.refresh();
    }
    pub(super) fn local_disconnected(&self) {
        if matches!(
            *self.active.borrow(),
            Some(super::notifications::NotificationSource::Local(_))
        ) {
            self.active.borrow_mut().take();
            self.source_changed();
        }
        self.local_generation.set(self.local_generation.get() + 1);
        self.local_exhausted.set(false);
        self.subscribed.borrow_mut().clear();
        self.subscribe_running.set(false);
        *self.subscribe_problem.borrow_mut() = None;
    }
    pub(super) fn cloud_disconnected(&self) {
        if matches!(
            *self.active.borrow(),
            Some(super::notifications::NotificationSource::Cloud(_))
        ) {
            self.active.borrow_mut().take();
            self.source_changed();
        }
        self.cloud_generation.set(self.cloud_generation.get() + 1);
        self.cloud_subscribed.borrow_mut().clear();
        self.cloud_running.set(false);
        self.cloud_exhausted.set(false);
        self.cloud_problem.borrow_mut().take();
    }
    #[cfg(test)]
    pub(super) fn notify(&self, binding: PropertyBinding, value: bool) {
        self.notify_value(binding, &serde_json::Value::Bool(value), false);
    }
    pub(super) fn notify_value(
        &self,
        binding: PropertyBinding,
        value: &serde_json::Value,
        cached_report: bool,
    ) -> Option<super::lighting::ObservedLight> {
        if self.life.is_closed() {
            return None;
        }
        let cached = self.record.cached.borrow();
        if cached.device.refresh_trigger == Some(binding) && value.as_bool().is_some() {
            drop(cached);
            self.refresh();
            return None;
        }
        let function = cached
            .device
            .functions
            .iter()
            .find(|function| super::lighting::bindings(function).contains(&binding))?;
        let id = function.description.id.clone();
        let light = function.lighting.is_some();
        drop(cached);
        if light {
            let origin = if cached_report {
                super::lighting::Origin::Cache
            } else {
                super::lighting::Origin::Notification
            };
            match self.observe_report(&id, binding, value, origin) {
                Ok(observed) => Some(observed),
                Err(error) => {
                    log::warn!("Ignoring invalid light property: {error}");
                    None
                }
            }
        } else {
            if let Some(value) = value.as_bool() {
                self.observations.notify(&self.record, &id, value);
            }
            None
        }
    }
    /// Validate and apply one actual light observation, keeping the confirmed
    /// record used by repaired groups separate from optimistic targets.
    pub(super) fn observe_report(
        &self,
        id: &str,
        binding: PropertyBinding,
        value: &serde_json::Value,
        origin: super::lighting::Origin,
    ) -> Result<super::lighting::ObservedLight, crate::xiaomi::capability::CapabilityError> {
        let observed = super::lighting::observe(self, id, binding, value, origin)?;
        {
            let mut confirmed = self.confirmed.borrow_mut();
            let generation = self.record.binding_generation.get();
            match confirmed.as_mut() {
                Some((known, _)) if *known == generation => {}
                _ => {
                    *confirmed = Some((generation, ConfirmedLight::default()));
                }
            }
            confirmed.as_mut().unwrap().1.merge(observed.patch);
        }
        super::lighting::apply_report(self, id, binding, observed.patch, observed.external, origin);
        Ok(observed)
    }
    /// The confirmed observation for the current binding authority, if any.
    pub(super) fn confirmed_light(&self) -> Option<ConfirmedLight> {
        match &*self.confirmed.borrow() {
            Some((generation, confirmed))
                if *generation == self.record.binding_generation.get()
                    && !self.record.life.is_closed() =>
            {
                Some(*confirmed)
            }
            _ => None,
        }
    }
    pub(super) fn clear_confirmed(&self) {
        self.confirmed.borrow_mut().take();
    }
    pub(super) fn request_local_subscriptions(&self) {
        if !self.subscribe_running.get() && !self.local_exhausted.get() {
            self.subscribe_requested.set(true);
            self.changed.notify(usize::MAX);
        }
    }
    pub(super) fn retry_subscriptions(&self) {
        self.condition_generation
            .set(self.condition_generation.get() + 1);
        self.local_exhausted.set(false);
        self.cloud_exhausted.set(false);
        self.changed.notify(usize::MAX);
    }
    pub(super) fn refresh(&self) {
        for function in &self.record.cached.borrow().device.functions {
            for binding in super::lighting::bindings(function) {
                self.refresh_property(&function.description.id, binding);
            }
        }
    }
    pub(super) fn refresh_property(&self, id: &str, binding: PropertyBinding) {
        self.observations.refresh(id, binding);
        let mut pending = self.refresh_pending.borrow_mut();
        if !pending
            .iter()
            .any(|read| read.id == id && read.binding == binding)
        {
            pending.push_back(Refresh {
                id: id.into(),
                binding,
                attempts: 0,
                ready_at: Duration::ZERO,
            });
        }
        self.refresh_changed.notify(usize::MAX);
    }
    /// Queue one read without invalidating an in-flight read of the same
    /// property. The refresh counter is advanced only for the queued read, so
    /// the read that is already in flight still applies its result.
    pub(super) fn queue_member_read(&self, id: &str, binding: PropertyBinding) {
        self.observations.refresh(id, binding);
        let mut pending = self.refresh_pending.borrow_mut();
        if !pending
            .iter()
            .any(|read| read.id == id && read.binding == binding)
        {
            pending.push_back(Refresh {
                id: id.into(),
                binding,
                attempts: 0,
                ready_at: Duration::ZERO,
            });
        }
        self.refresh_changed.notify(usize::MAX);
    }
    /// A member event marks the group dirty. Changes arriving while a read is
    /// in flight are coalesced into exactly one follow-up read; the current
    /// read is never repeatedly invalidated by an event stream.
    pub(super) fn request_member_read(&self) {
        if self.life.is_closed() {
            return;
        }
        let pairs = self
            .record
            .cached
            .borrow()
            .device
            .functions
            .iter()
            .flat_map(|function| {
                super::lighting::bindings(function)
                    .into_iter()
                    .map(|binding| (function.description.id.clone(), binding))
            })
            .collect::<Vec<_>>();
        for (id, binding) in pairs {
            self.request_dirty_read(&id, binding);
        }
    }
    /// Queue one real read for a binding without dropping an in-flight read:
    /// a demand arriving during a read coalesces into exactly one follow-up.
    pub(super) fn request_dirty_read(&self, id: &str, binding: PropertyBinding) {
        if self.life.is_closed() {
            return;
        }
        let key = super::lighting::key(id, binding);
        if self.refresh_inflight.borrow().contains_key(&key) {
            self.dirty_again.borrow_mut().insert(key);
        } else if !self
            .refresh_pending
            .borrow()
            .iter()
            .any(|read| read.id == id && read.binding == binding)
        {
            self.queue_member_read(id, binding);
        }
    }
    /// Called after a read finishes. A dirty-again marker produces the single
    /// follow-up read that covers every change seen during the finished read.
    pub(super) fn finish_dirty_read(&self, id: &str, binding: PropertyBinding) {
        let key = super::lighting::key(id, binding);
        if self.dirty_again.borrow_mut().remove(&key) {
            self.queue_member_read(id, binding);
        }
    }
    pub(super) fn ensure_property_read(&self, id: &str, binding: PropertyBinding) {
        if self
            .refresh_pending
            .borrow()
            .iter()
            .any(|read| read.id == id && read.binding == binding)
            || self
                .refresh_inflight
                .borrow()
                .contains_key(&super::lighting::key(id, binding))
        {
            return;
        }
        self.refresh_property(id, binding);
    }
    pub(super) fn contains(&self, id: &str, binding: PropertyBinding) -> bool {
        self.record
            .cached
            .borrow()
            .device
            .functions
            .iter()
            .any(|function| {
                function.description.id == id
                    && super::lighting::bindings(function).contains(&binding)
            })
    }
    pub(super) fn observe_read(
        &self,
        id: &str,
        binding: PropertyBinding,
        stamp: &super::observations::ReadStamp,
        value: &serde_json::Value,
    ) -> Result<(), crate::xiaomi::capability::CapabilityError> {
        if !self
            .observations
            .read_is_current(&self.record, id, binding, stamp)
        {
            return Ok(());
        }
        let light = self
            .record
            .cached
            .borrow()
            .device
            .functions
            .iter()
            .find(|function| function.description.id == id)
            .unwrap()
            .lighting
            .is_some();
        if light {
            let _ = self.observe_report(id, binding, value, super::lighting::Origin::Read)?;
        } else {
            let value =
                value
                    .as_bool()
                    .ok_or(crate::xiaomi::capability::CapabilityError::InvalidValue(
                        "boolean",
                    ))?;
            self.observations
                .observe_read(&self.record, id, stamp, value);
        }
        Ok(())
    }
    pub(super) fn bindings(&self) -> Vec<PropertyBinding> {
        let mut values = self
            .record
            .cached
            .borrow()
            .device
            .functions
            .iter()
            .flat_map(super::lighting::bindings)
            .collect::<Vec<_>>();
        if let Some(trigger) = self.record.cached.borrow().device.refresh_trigger {
            values.push(trigger);
        }
        values
    }
    pub(super) fn diagnostic(&self) -> Option<String> {
        let values = self
            .subscribe_problem
            .borrow()
            .clone()
            .into_iter()
            .chain(self.cloud_problem.borrow().clone())
            .chain(self.read_problems.borrow().values().cloned())
            .collect::<Vec<_>>();
        (!values.is_empty()).then(|| values.join("; "))
    }
    pub(super) fn subscription_problem(&self) -> Option<String> {
        self.subscribe_problem
            .borrow()
            .clone()
            .or_else(|| self.cloud_problem.borrow().clone())
    }
    pub(super) fn report(&self, context: &Context, did: &str) {
        if self.life.is_closed() {
            return;
        }
        let blocked = context.live.borrow().devices[did].blocked.clone();
        // Group diagnostics derive from the actual per-DID watch state map, so
        // one member's success cannot hide another member's failure.
        let watch = context
            .is_group(did)
            .then(|| context.watch_problem(did))
            .flatten();
        let values = watch
            .into_iter()
            .chain(self.diagnostic())
            .collect::<Vec<_>>();
        let problem = blocked.or_else(|| (!values.is_empty()).then(|| values.join("; ")));
        context.problem(did, problem);
    }
    pub(super) fn problem(
        &self,
        operation: &str,
        binding: PropertyBinding,
        error: &impl std::fmt::Display,
    ) -> String {
        let message = format!(
            "{} {operation} {}.{}: {error}",
            self.record.cached.borrow().device.key.id,
            binding.siid,
            binding.piid
        );
        log::warn!("{message}");
        message
    }
}
pub(super) async fn run(
    work: Rc<DeviceWork>,
    handle: SessionHandle,
    context: Rc<Context>,
) -> Result<(), AdapterError> {
    // Poll cancellation first so removed work cannot submit another request.
    future::or(
        async {
            work.life.ended().await;
            Ok(())
        },
        subscriptions(&work, &handle, &context),
    )
    .await
}
async fn subscriptions(
    work: &DeviceWork,
    handle: &SessionHandle,
    context: &Context,
) -> Result<(), AdapterError> {
    let did = work.record.cached.borrow().device.key.id.clone();
    loop {
        let listener = work.changed.listen();
        if !work.subscribe_requested.replace(false) {
            listener.await;
            continue;
        }
        if !matches!(context.notification_source(&did), Some(super::notifications::NotificationSource::Local(selected)) if selected.same_connection(handle))
        {
            continue;
        }
        work.subscribe_running.set(true);
        let preparation = work.local_generation.get();
        // Retiring the connection, bindings, or preparation advances this generation.
        let current =
            || context.current_local(handle) && work.local_generation.get() == preparation;
        context
            .clock
            .wait_until(context.clock.now() + Duration::from_millis(10))
            .await;
        let mut pending = work
            .bindings()
            .into_iter()
            .filter(|b| !work.subscribed.borrow().contains(&(b.siid, b.piid)))
            .collect::<Vec<_>>();
        'attempts: for attempt in 0..4 {
            let mut failed = Vec::new();
            let mut problem = None;
            for binding in pending {
                if !current()
                    || !matches!(context.notification_source(&did), Some(super::notifications::NotificationSource::Local(selected)) if selected.same_connection(handle))
                {
                    break 'attempts;
                }
                let result = handle.subscribe_property(&did, binding).await;
                if !current() {
                    break 'attempts;
                }
                match result {
                    Ok(()) => {
                        work.subscribed
                            .borrow_mut()
                            .insert((binding.siid, binding.piid));
                    }
                    Err(error) => {
                        problem = Some(work.problem("subscribe", binding, &error));
                        failed.push(binding);
                    }
                }
            }
            if failed.is_empty() || attempt == 3 {
                work.local_exhausted.set(problem.is_some());
                *work.subscribe_problem.borrow_mut() = problem;
                work.report(context, &did);
                break;
            }
            pending = failed;
            context
                .clock
                .wait_until(context.clock.now() + Duration::from_millis(100))
                .await;
        }
        work.subscribe_running.set(false);
        context.changed.notify(usize::MAX);
    }
}
