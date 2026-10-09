use super::{
    bootstrap::Clock,
    model::{AdapterError, ChannelCommands, CommandStatus, CommandTarget, PathKind},
    routing::ControlPath,
    state::{Context, DeviceRecord},
    work::DeviceWork,
};
use crate::{
    device::{CommandError, CommandReceiver, CommandRequest},
    xiaomi::{
        capability::PropertyBinding,
        central::{CentralError, RequestStage, SendDecision, SendHooks, SessionHandle},
    },
};
use event_listener::Event;
use futures_lite::future;
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
    sync::Arc,
};

mod feedback;
use feedback::LightFeedback;

/// Receipts and recent numeric feedback retain their gate across device replacements.
#[derive(Default)]
pub(super) struct LightWriteGates {
    gates: RefCell<BTreeMap<String, Rc<LightWriteGate>>>,
}
impl LightWriteGates {
    pub(super) fn for_device(&self, did: &str, clock: Arc<dyn Clock>) -> Rc<LightWriteGate> {
        let mut gates = self.gates.borrow_mut();
        gates.retain(|_, gate| {
            Rc::strong_count(gate) > 1 || !gate.feedback.borrow_mut().is_empty(clock.now())
        });
        gates
            .entry(did.into())
            .or_insert_with(|| Rc::new(LightWriteGate::new(clock)))
            .clone()
    }
}
pub(super) struct LightWriteGate {
    busy: Cell<bool>,
    changed: Event,
    clock: Arc<dyn Clock>,
    feedback: RefCell<LightFeedback>,
}
impl LightWriteGate {
    fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            busy: Cell::new(false),
            changed: Event::new(),
            clock,
            feedback: RefCell::default(),
        }
    }
    async fn acquire(self: &Rc<Self>) -> LightWritePermit {
        loop {
            let changed = self.changed.listen();
            if !self.busy.replace(true) {
                return LightWritePermit(self.clone());
            }
            changed.await;
        }
    }
    fn register_feedback(&self, properties: &[(PropertyBinding, serde_json::Value)]) {
        self.feedback.borrow_mut().sent(properties);
    }
    fn finish_feedback<E>(
        &self,
        properties: &[(PropertyBinding, serde_json::Value)],
        results: &crate::xiaomi::property::PropertyResults<(), E>,
        history_bindings: &[PropertyBinding],
    ) {
        let accepted =
            properties
                .iter()
                .zip(&results.items)
                .filter_map(|((binding, _), result)| {
                    (result.is_ok() && history_bindings.contains(binding)).then_some(*binding)
                });
        self.feedback
            .borrow_mut()
            .complete(accepted, self.clock.now());
    }
    fn cancel_feedback(&self) {
        self.feedback.borrow_mut().cancel();
    }
    pub(super) fn is_feedback(&self, binding: PropertyBinding, value: &serde_json::Value) -> bool {
        self.feedback
            .borrow_mut()
            .matches(binding, value, self.clock.now())
    }
}
struct LightWritePermit(Rc<LightWriteGate>);
impl Drop for LightWritePermit {
    fn drop(&mut self) {
        self.0.cancel_feedback();
        self.0.busy.set(false);
        self.0.changed.notify(usize::MAX);
    }
}

/// Diagnostics retain one current and one completed operation per channel, never a history.
#[derive(Default)]
pub(super) struct Commands {
    next: Cell<u64>,
    records: RefCell<BTreeMap<(String, String), ChannelCommands>>,
    pub(super) changed: Event,
}
impl Commands {
    fn begin(
        self: &Rc<Self>,
        did: &str,
        channel: &str,
        binding: PropertyBinding,
        target: bool,
    ) -> CommandLease {
        self.begin_operation(
            did,
            channel,
            binding,
            CommandTarget::Power(target),
            vec![(binding, serde_json::Value::Bool(target))],
        )
    }
    fn begin_operation(
        self: &Rc<Self>,
        did: &str,
        channel: &str,
        binding: PropertyBinding,
        target: CommandTarget,
        properties: Vec<(PropertyBinding, serde_json::Value)>,
    ) -> CommandLease {
        let id = self.next.get() + 1;
        self.next.set(id);
        let status = CommandStatus {
            id,
            binding,
            target,
            properties,
            actual_path: None,
            result: None,
        };
        let mut records = self.records.borrow_mut();
        let entry = records
            .entry((did.into(), channel.into()))
            .or_insert_with(|| ChannelCommands {
                light_actions: None,
                channel: channel.into(),
                inflight: None,
                last: None,
            });
        entry.inflight = Some(status.clone());
        drop(records);
        self.changed.notify(usize::MAX);
        CommandLease(Rc::new(CommandOperation {
            commands: self.clone(),
            did: did.into(),
            channel: channel.into(),
            status: RefCell::new(status),
        }))
    }
    pub(super) fn snapshot(
        &self,
        did: &str,
        channels: impl Iterator<Item = String>,
    ) -> Vec<ChannelCommands> {
        let records = self.records.borrow();
        channels
            .map(|channel| {
                records
                    .get(&(did.into(), channel.clone()))
                    .cloned()
                    .unwrap_or(ChannelCommands {
                        light_actions: None,
                        channel,
                        inflight: None,
                        last: None,
                    })
            })
            .collect()
    }
    pub(super) fn remove(&self, did: &str) {
        self.records
            .borrow_mut()
            .retain(|(device, _), _| device != did);
        self.changed.notify(usize::MAX);
    }
}
struct CommandOperation {
    commands: Rc<Commands>,
    did: String,
    channel: String,
    status: RefCell<CommandStatus>,
}
impl CommandOperation {
    fn properties(&self, properties: Vec<(PropertyBinding, serde_json::Value)>) {
        let mut status = self.status.borrow_mut();
        if status.actual_path.is_some() {
            return;
        }
        if let Some((binding, _)) = properties.first() {
            status.binding = *binding;
        }
        status.properties = properties;
        if let Some(entry) = self
            .commands
            .records
            .borrow_mut()
            .get_mut(&(self.did.clone(), self.channel.clone()))
        {
            entry.inflight = Some(status.clone());
        }
        self.commands.changed.notify(usize::MAX);
    }

    fn submitted(&self, path: PathKind) {
        let status = {
            let mut status = self.status.borrow_mut();
            if status.result.is_some() || status.actual_path.is_some() {
                return;
            }
            status.actual_path = Some(path);
            status.clone()
        };
        if let Some(entry) = self
            .commands
            .records
            .borrow_mut()
            .get_mut(&(self.did.clone(), self.channel.clone()))
            && entry
                .inflight
                .as_ref()
                .is_some_and(|current| current.id == status.id)
        {
            entry.inflight = Some(status.clone());
        }
        log::info!(
            "Xiaomi command start op={} did={} channel={} property={} target={} path={} result={}",
            status.id,
            self.did,
            self.channel,
            status.properties_text(),
            status.target,
            path,
            status.result_text(),
        );
        self.commands.changed.notify(usize::MAX);
    }
    fn reject(&self, request: CommandRequest, error: CommandError) {
        self.finish(Err(error.clone()));
        request.reject(error);
    }
    fn finish(&self, result: Result<(), CommandError>) {
        let status = {
            let mut status = self.status.borrow_mut();
            if status.result.is_some() {
                return;
            }
            status.result = Some(result.clone());
            status.clone()
        };
        if let Some(entry) = self
            .commands
            .records
            .borrow_mut()
            .get_mut(&(self.did.clone(), self.channel.clone()))
            && entry
                .inflight
                .as_ref()
                .is_some_and(|current| current.id == status.id)
        {
            entry.inflight = None;
            entry.last = Some(status.clone());
        }
        let path = status.actual_path.map_or("none", |path| match path {
            PathKind::Local => "local",
            PathKind::Cloud => "cloud",
        });
        let result = status.result_text();
        let level = if status.result.as_ref().unwrap().is_ok() {
            log::Level::Info
        } else {
            log::Level::Warn
        };
        log::log!(
            level,
            "Xiaomi command result op={} did={} channel={} property={} target={} path={} result={}",
            status.id,
            self.did,
            self.channel,
            status.properties_text(),
            status.target,
            path,
            result
        );
        self.commands.changed.notify(usize::MAX);
    }
}
/// Only the consumer owns this lease; a transport callback cannot extend its lifetime.
struct CommandLease(Rc<CommandOperation>);
impl Drop for CommandLease {
    fn drop(&mut self) {
        self.0.finish(Err(CommandError::Cancelled));
    }
}

fn command_error(error: CentralError) -> CommandError {
    match error {
        CentralError::Timeout => CommandError::Timeout,
        CentralError::Rejected(code) => CommandError::Rejected(format!("gateway code {code}")),
        CentralError::Cancelled => CommandError::Cancelled,
        error => CommandError::SendFailed(error.to_string()),
    }
}
/// Return ownership only when the request was withdrawn before transmission.
async fn send_local_write(
    context: &Rc<Context>,
    handle: SessionHandle,
    work: Rc<DeviceWork>,
    id: &str,
    binding: PropertyBinding,
    request: CommandRequest,
    operation: Rc<CommandOperation>,
) -> Option<CommandRequest> {
    let generation = work.record.binding_generation.get();
    let did = work.record.physical.key().id.clone();
    let target = request.target;
    let pending = Rc::new(RefCell::new(Some(request)));
    let completion = pending.clone();
    let observing = work.clone();
    let id = id.to_owned();
    let stamp = Rc::new(RefCell::new(None));
    let capture_stamp = stamp.clone();
    let capture_work = work.clone();
    let capture_id = id.clone();
    let guard_work = work.clone();
    let guard_context = context.clone();
    let guard_handle = handle.clone();
    let guard_did = did.clone();
    let result_context = context.clone();
    let result_handle = handle.clone();
    let submitted = operation.clone();
    let hooks = SendHooks::new(
        move || {
            if guard_work.life.is_closed()
                || guard_work.record.life.is_closed()
                || guard_work.record.binding_generation.get() != generation
                || !guard_context.current_local(&guard_handle)
            {
                SendDecision::Cancel
            } else if matches!(guard_context.control_path(&guard_did), Some(ControlPath::Local(selected)) if selected.same_connection(&guard_handle))
            {
                SendDecision::Send
            } else {
                SendDecision::Withdraw
            }
        },
        move || {
            submitted.submitted(super::model::PathKind::Local);
            *capture_stamp.borrow_mut() = Some(capture_work.observations.write_stamp(
                &capture_work.record,
                &capture_id,
                binding,
            ));
        },
    );
    handle
        .set_property_guarded(&did, binding, target, hooks, move |outcome| {
            result_context.local_device_outcome(&result_handle, &observing, generation, &outcome);
            if outcome.result == Err(CentralError::Withdrawn)
                && outcome.stage == RequestStage::NotSent
            {
                return;
            }
            let request = completion.borrow_mut().take().unwrap();
            match outcome.result {
                Ok(()) => {
                    if let Some(stamp) = stamp.borrow_mut().take() {
                        operation.finish(observing.observations.accept_write(
                            &observing.record,
                            &id,
                            &stamp,
                            request,
                        ));
                    } else {
                        let error =
                            CommandError::SendFailed("gateway accepted an unsent write".into());
                        operation.reject(request, error);
                    }
                }
                Err(error) => {
                    operation.reject(request, command_error(error));
                }
            }
        })
        .await;
    pending.borrow_mut().take()
}

pub(super) async fn consume(
    mut receiver: CommandReceiver,
    context: Rc<Context>,
    record: Rc<DeviceRecord>,
    function_id: String,
) -> Result<(), AdapterError> {
    future::or(
        async {
            record.life.ended().await;
            Ok(())
        },
        async {
            let did = record.physical.key().id.clone();
            while let Some(request) = receiver.next_operation().await {
                let request = match request {
                    crate::device::DeviceRequest::Light(request) => {
                        lighting::send_owned(
                            crate::device::DeviceRequest::Light(request),
                            context.clone(),
                            record.clone(),
                            &function_id,
                        )
                        .await?;
                        continue;
                    }
                    crate::device::DeviceRequest::Power(request)
                        if record
                            .cached
                            .borrow()
                            .device
                            .functions
                            .iter()
                            .find(|function| function.description.id == function_id)
                            .unwrap()
                            .lighting
                            .is_some() =>
                    {
                        lighting::send_owned(
                            crate::device::DeviceRequest::Power(request),
                            context.clone(),
                            record.clone(),
                            &function_id,
                        )
                        .await?;
                        continue;
                    }
                    crate::device::DeviceRequest::Power(request) => request,
                };
                let binding = record.binding(&function_id);
                let target = request.target;
                let command = context.commands.begin(&did, &function_id, binding, target);
                let work = context
                    .live
                    .borrow()
                    .devices
                    .get(&did)
                    .and_then(|slot| slot.work.clone());
                let Some(work) = work else {
                    command.0.reject(
                        request,
                        CommandError::Unavailable("no usable control path".into()),
                    );
                    continue;
                };
                let generation = record.binding_generation.get();
                let mut request = request;
                loop {
                    if work.life.is_closed()
                        || record.life.is_closed()
                        || record.binding_generation.get() != generation
                    {
                        command.0.reject(request, CommandError::Cancelled);
                        break;
                    }
                    let Some(path) = context.control_path(&did) else {
                        command.0.reject(
                            request,
                            CommandError::Unavailable("no usable control path".into()),
                        );
                        break;
                    };
                    match path {
                        ControlPath::Local(handle) => {
                            let Some(withdrawn) = send_local_write(
                                &context,
                                handle,
                                work.clone(),
                                &function_id,
                                binding,
                                request,
                                command.0.clone(),
                            )
                            .await
                            else {
                                break;
                            };
                            request = withdrawn;
                        }
                        ControlPath::Cloud(credentials) => {
                            let stamp =
                                work.observations
                                    .write_stamp(&record, &function_id, binding);
                            command.0.submitted(super::model::PathKind::Cloud);
                            match context
                                .cloud
                                .set_property(&credentials.access_token, &did, binding, target)
                                .await
                            {
                                Ok(()) => command.0.finish(work.observations.accept_write(
                                    &record,
                                    &function_id,
                                    &stamp,
                                    request,
                                )),
                                Err(error) => {
                                    let command_error = match error.kind() {
                                        crate::xiaomi::cloud::CloudErrorKind::Timeout => {
                                            CommandError::Timeout
                                        }
                                        crate::xiaomi::cloud::CloudErrorKind::Business(code)
                                        | crate::xiaomi::cloud::CloudErrorKind::PropertyRejected(
                                            code,
                                        ) => CommandError::Rejected(format!("cloud code {code}")),
                                        _ => CommandError::SendFailed(error.to_string()),
                                    };
                                    command.0.reject(request, command_error);
                                    let current = !work.life.is_closed()
                                        && !record.life.is_closed()
                                        && record.binding_generation.get() == generation;
                                    // Account failures remain relevant after a binding is replaced.
                                    context.cloud_error(
                                        &credentials.access_token,
                                        current.then_some(did.as_str()),
                                        &error,
                                    )?;
                                }
                            }
                            break;
                        }
                    }
                }
            }
            Ok(())
        },
    )
    .await
}

#[cfg(test)]
mod tests;

mod lighting;
