use super::{
    commands::LightWriteGate,
    model::AdapterError,
    observations::Observations,
    refresh::RefreshState,
    state::{Context, DeviceRecord, Lifetime},
};
use crate::device::{ActionToken, LightDimension};
use crate::xiaomi::{capability::PropertyBinding, central::SessionHandle};
use event_listener::Event;
use futures_lite::future;
use std::{
    cell::{Cell, RefCell},
    collections::BTreeSet,
    rc::Rc,
    time::Duration,
};
/// One committed repaired-group member handle, so an action keeps the member
/// universe captured at its first frame.
#[derive(Clone)]
pub(super) struct RepairMemberRef {
    pub(super) record: Rc<DeviceRecord>,
    pub(super) work: Rc<DeviceWork>,
}

struct FrameOwnership {
    token: ActionToken,
    epoch: u64,
    members: Rc<Vec<RepairMemberRef>>,
}

/// The newest RGB frame on a group function. A slow member checks ownership
/// before submitting so a newer frame, Stop or takeover retires queued writes
/// without cancelling a successful final acceptance.
#[derive(Default)]
pub(super) struct LightFrames {
    light: Option<FrameOwnership>,
}
impl LightFrames {
    pub(super) fn next_light(
        &mut self,
        token: ActionToken,
        snapshot: impl FnOnce() -> Vec<RepairMemberRef>,
    ) -> (u64, Rc<Vec<RepairMemberRef>>) {
        if let Some(frame) = self.light.as_mut()
            && frame.token == token
        {
            frame.epoch += 1;
            return (frame.epoch, frame.members.clone());
        }
        let members = Rc::new(snapshot());
        let epoch = 1;
        self.light = Some(FrameOwnership {
            token,
            epoch,
            members: members.clone(),
        });
        (epoch, members)
    }
    pub(super) fn current_light(&self, token: ActionToken, epoch: u64) -> bool {
        matches!(self.light, Some(ref frame) if frame.token == token && frame.epoch == epoch)
    }
    pub(super) fn cancel_light(&mut self, token: ActionToken, epoch: u64) {
        if self.current_light(token, epoch) {
            self.light = None;
        }
    }
    pub(super) fn cancel_dimension(&mut self, dimension: LightDimension) {
        if dimension == LightDimension::Color {
            self.light = None;
        }
    }

    /// A committed rebuild or runtime reset retires every queued frame.
    pub(super) fn cancel_all(&mut self) {
        self.light = None;
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
    pub(super) refresh: RefCell<RefreshState>,
    pub(super) refresh_changed: Rc<Event>,
    pub(super) light_frames: RefCell<LightFrames>,
    changed: Event,
    subscribe_problem: RefCell<Option<String>>,
}
impl DeviceWork {
    pub(super) fn new(
        record: Rc<DeviceRecord>,
        refresh_changed: Rc<Event>,
        light_write: Option<Rc<LightWriteGate>>,
    ) -> Self {
        Self {
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
            refresh: RefCell::new(RefreshState::default()),
            refresh_changed,
            light_frames: RefCell::new(LightFrames::default()),
            changed: Event::new(),
            subscribe_problem: RefCell::new(None),
        }
    }
    pub(super) fn local_connected(&self) {
        self.local_disconnected();
        self.subscribe_requested.set(false);
        self.changed.notify(usize::MAX);
    }
    pub(super) fn source_changed(&self) {
        self.observations.source_changed();
        self.record.physical.clear_light_state();
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
        self.notify_value(binding, &serde_json::Value::Bool(value), false, true);
    }
    pub(super) fn notify_value(
        &self,
        binding: PropertyBinding,
        value: &serde_json::Value,
        cached_report: bool,
        takeover: bool,
    ) -> bool {
        if self.life.is_closed() {
            return false;
        }
        let cached = self.record.cached.borrow();
        if cached.device.refresh_trigger() == Some(binding) && value.as_bool().is_some() {
            drop(cached);
            self.refresh();
            return false;
        }
        let Some(function) = cached
            .device
            .functions()
            .iter()
            .find(|function| super::lighting::bindings(function).contains(&binding))
        else {
            return false;
        };
        let id = function.id().to_owned();
        let light = function.lighting().is_some();
        drop(cached);
        if light {
            let origin = if cached_report {
                super::lighting::Origin::Cache
            } else {
                super::lighting::Origin::Notification
            };
            match super::lighting::observe(self, &id, binding, value, origin, takeover) {
                Ok(()) => true,
                Err(error) => {
                    log::warn!("Ignoring invalid light property: {error}");
                    false
                }
            }
        } else {
            if let Some(value) = value.as_bool() {
                self.observations.notify(&self.record, &id, value);
            }
            false
        }
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
        let cached = self.record.cached.borrow();
        for function in cached.device.functions() {
            for binding in super::lighting::bindings(function) {
                self.refresh_property(function.id(), binding);
            }
        }
    }
    pub(super) fn refresh_property(&self, id: &str, binding: PropertyBinding) {
        self.observations.refresh(id, binding);
        self.refresh.borrow_mut().request(id, binding);
        self.refresh_changed.notify(usize::MAX);
    }
    /// Changes during an in-flight read coalesce into exactly one follow-up.
    pub(super) fn request_dirty_read(&self, id: &str, binding: PropertyBinding) {
        if !self.life.is_closed() && self.refresh.borrow_mut().request_dirty(id, binding) {
            self.observations.refresh(id, binding);
            self.refresh_changed.notify(usize::MAX);
        }
    }
    pub(super) fn ensure_property_read(&self, id: &str, binding: PropertyBinding) {
        if !self.refresh.borrow().contains(id, binding) {
            self.refresh_property(id, binding);
        }
    }
    pub(super) fn contains(&self, id: &str, binding: PropertyBinding) -> bool {
        self.record
            .cached
            .borrow()
            .device
            .functions()
            .iter()
            .any(|function| {
                function.id() == id && super::lighting::bindings(function).contains(&binding)
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
            .functions()
            .iter()
            .find(|function| function.id() == id)
            .unwrap()
            .lighting()
            .is_some();
        if light {
            super::lighting::observe(
                self,
                id,
                binding,
                value,
                super::lighting::Origin::Read,
                false,
            )?;
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
            .functions()
            .iter()
            .flat_map(super::lighting::bindings)
            .collect::<Vec<_>>();
        if let Some(trigger) = self.record.cached.borrow().device.refresh_trigger() {
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
            .chain(self.refresh.borrow().problems())
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
        let problem = blocked.or_else(|| self.diagnostic());
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
            self.record.cached.borrow().device.key().id,
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
    let did = work.record.cached.borrow().device.key().id.clone();
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
