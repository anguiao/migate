use super::{
    bootstrap::Clock,
    model::{
        AdapterError, ChannelCommands, CommandFeedback, CommandProperty, CommandStatus,
        CommandStep, CommandTarget, PathKind, StepReceipt,
    },
    routing::ControlPath,
    state::{Context, LogicalSink},
    work::{DeviceWork, SourceRef},
};
use crate::{
    device::{CommandError, CommandReceiver, CommandRequest, DeviceRequest},
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
    time::Instant,
};

mod air_conditioner;
mod curtain;
mod fan;
mod feedback;
mod properties;
mod steps;
use feedback::LightFeedback;

/// Receipts and recent numeric feedback retain their gate across device replacements.
#[derive(Default)]
pub(super) struct DeviceWriteGates {
    gates: RefCell<BTreeMap<String, Rc<DeviceWriteGate>>>,
}
impl DeviceWriteGates {
    pub(super) fn for_device(&self, did: &str, clock: Arc<dyn Clock>) -> Rc<DeviceWriteGate> {
        let mut gates = self.gates.borrow_mut();
        gates.retain(|_, gate| {
            Rc::strong_count(gate) > 1 || !gate.feedback.borrow_mut().is_empty(clock.now())
        });
        gates
            .entry(did.into())
            .or_insert_with(|| Rc::new(DeviceWriteGate::new(clock)))
            .clone()
    }
    /// A binding-authority replacement retires every sent/history
    /// entry so a late old receipt cannot reintroduce feedback.
    pub(super) fn clear_authority(&self, did: &str) {
        if let Some(gate) = self.gates.borrow().get(did) {
            gate.clear_authority();
        }
    }
}
pub(super) struct DeviceWriteGate {
    busy: Cell<bool>,
    changed: Event,
    clock: Arc<dyn Clock>,
    feedback: RefCell<LightFeedback>,
    /// Binding-authority epoch; a replacement retires all old feedback.
    authority: Cell<u64>,
}
impl DeviceWriteGate {
    fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            busy: Cell::new(false),
            changed: Event::new(),
            clock,
            feedback: RefCell::default(),
            authority: Cell::new(0),
        }
    }
    async fn acquire(self: &Rc<Self>) -> DeviceWritePermit {
        loop {
            let changed = self.changed.listen();
            if !self.busy.replace(true) {
                return DeviceWritePermit(self.clone());
            }
            changed.await;
        }
    }
    pub(super) fn authority(&self) -> u64 {
        self.authority.get()
    }
    fn clear_authority(&self) {
        self.feedback.borrow_mut().clear_all();
        self.authority.set(self.authority.get() + 1);
    }
    fn register_feedback_for(
        &self,
        authority: u64,
        properties: &[(PropertyBinding, serde_json::Value)],
    ) {
        if authority == self.authority.get() {
            self.feedback.borrow_mut().sent(properties);
        }
    }
    fn finish_feedback_for<E>(
        &self,
        authority: u64,
        properties: &[(PropertyBinding, serde_json::Value)],
        results: &crate::xiaomi::property::PropertyResults<(), E>,
        retain_feedback: bool,
    ) {
        if !retain_feedback {
            return;
        }
        if authority != self.authority.get() {
            // Consume the late receipt without writing it into the new authority.
            self.feedback.borrow_mut().cancel();
            return;
        }
        let accepted = properties
            .iter()
            .zip(&results.items)
            .filter_map(|((binding, _), result)| result.is_ok().then_some(*binding));
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
struct DeviceWritePermit(Rc<DeviceWriteGate>);
impl Drop for DeviceWritePermit {
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
        queued_at: Instant,
    ) -> CommandLease {
        self.begin_operation(
            did,
            channel,
            binding,
            CommandTarget::Power(target),
            vec![(binding, serde_json::Value::Bool(target))],
            queued_at,
        )
    }
    fn begin_operation(
        self: &Rc<Self>,
        did: &str,
        channel: &str,
        binding: PropertyBinding,
        target: CommandTarget,
        properties: Vec<(PropertyBinding, serde_json::Value)>,
        queued_at: Instant,
    ) -> CommandLease {
        let id = self.next.get() + 1;
        self.next.set(id);
        let status = CommandStatus {
            id,
            binding,
            target,
            properties: properties.into_iter().map(Into::into).collect(),
            actual_path: None,
            result: None,
            steps: Vec::new(),
            feedback: None,
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
            queued_at,
            sent_at: Cell::new(None),
            pending_step: RefCell::new(None),
            write_stamps: RefCell::new(Vec::new()),
            shared_feedback: RefCell::new(None),
            feedback_enabled: Cell::new(true),
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
struct FeedbackTarget {
    work: Rc<DeviceWork>,
    id: String,
    stamps: BTreeMap<PropertyBinding, super::observations::ReadStamp>,
}

struct FeedbackScope {
    group: SourceRef,
    targets: Vec<(Rc<DeviceWork>, String)>,
    stamps: RefCell<Vec<FeedbackTarget>>,
}

fn notifications_ready(context: &Context, work: &DeviceWork, id: &str) -> bool {
    work.function_observer(id).is_some_and(|source| {
        context.source_valid_for(&work.key.id, id, &source) && work.source_ready(id, &source)
    })
}

fn work_current(context: &Context, work: &Rc<DeviceWork>) -> bool {
    !work.life.is_closed()
        && context
            .live
            .borrow()
            .devices
            .get(&work.key.id)
            .and_then(|slot| slot.admitted.as_ref())
            .is_some_and(|current| Rc::ptr_eq(current, work))
}

pub(super) struct CommandOperation {
    commands: Rc<Commands>,
    did: String,
    channel: String,
    status: RefCell<CommandStatus>,
    queued_at: Instant,
    sent_at: Cell<Option<Instant>>,
    pending_step: RefCell<Option<CommandProperty>>,
    write_stamps: RefCell<Vec<FeedbackTarget>>,
    shared_feedback: RefCell<Option<Rc<FeedbackScope>>>,
    feedback_enabled: Cell<bool>,
}
impl CommandOperation {
    fn pending_step(&self, property: (PropertyBinding, serde_json::Value)) {
        *self.pending_step.borrow_mut() = Some(property.into());
    }
    fn properties(&self, properties: Vec<(PropertyBinding, serde_json::Value)>) {
        let mut status = self.status.borrow_mut();
        if status.actual_path.is_some() {
            return;
        }
        if let Some((binding, _)) = properties.first() {
            status.binding = *binding;
        }
        status.properties = properties.into_iter().map(Into::into).collect();
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
            if status.result.is_some() {
                return;
            }
            if let Some(mut property) = self.pending_step.borrow_mut().take() {
                status.binding = property.binding;
                property.path = Some(path);
                status.steps.push(CommandStep {
                    binding: property.binding,
                    path,
                    receipt: StepReceipt::Awaiting,
                });
                status.properties.push(property);
            } else if status.actual_path.is_some() {
                return;
            } else {
                let steps = status
                    .properties
                    .iter()
                    .map(|property| CommandStep {
                        binding: property.binding,
                        path,
                        receipt: StepReceipt::Awaiting,
                    })
                    .collect::<Vec<_>>();
                status.steps.extend(steps);
                for property in &mut status.properties {
                    property.path = Some(path);
                }
            }
            status.actual_path = Some(path);
            status.clone()
        };
        if self.sent_at.get().is_none() {
            self.sent_at.set(Some(Instant::now()));
        }
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
    fn mark_write(&self, context: &Context, work: &Rc<DeviceWork>) {
        if self.feedback_enabled.get() {
            let shared = self.shared_feedback.borrow();
            let targets = if let Some(shared) = shared.as_ref() {
                shared.targets.clone()
            } else if context.is_virtual_group(&self.did) {
                context.group_member_work(&self.did)
            } else {
                vec![(work.clone(), self.channel.clone())]
            };
            let mut stamps = Vec::new();
            for (work, id) in targets {
                let function = work
                    .native
                    .device()
                    .functions()
                    .iter()
                    .find(|function| function.id() == id)
                    .unwrap();
                let mut readings = BTreeMap::new();
                for binding in function.observation_bindings() {
                    readings.insert(binding, work.observations.read_stamp(&id, binding));
                }
                stamps.push(FeedbackTarget {
                    work,
                    id,
                    stamps: readings,
                });
            }
            if let Some(shared) = shared.as_ref() {
                *shared.stamps.borrow_mut() = stamps;
            } else {
                *self.write_stamps.borrow_mut() = stamps;
            }
        }
    }
    fn feedback_stamp(
        &self,
        work: &DeviceWork,
        id: &str,
        binding: PropertyBinding,
    ) -> Option<super::observations::ReadStamp> {
        let shared = self.shared_feedback.borrow();
        let stamps = if let Some(shared) = shared.as_ref() {
            shared.stamps.borrow()
        } else {
            self.write_stamps.borrow()
        };
        stamps
            .iter()
            .find(|target| std::ptr::eq(target.work.as_ref(), work) && target.id == id)
            .and_then(|target| target.stamps.get(&binding).cloned())
    }
    fn write_feedback(
        &self,
        context: &Context,
        work: &DeviceWork,
        binding: PropertyBinding,
    ) -> super::observations::WriteFeedback {
        use super::observations::WriteFeedback;
        if notifications_ready(context, work, &self.channel) {
            return WriteFeedback::Ordinary;
        }
        if let Some(stamp) = self.feedback_stamp(work, &self.channel, binding) {
            if work.observations.read_after(&self.channel, binding, &stamp) {
                return WriteFeedback::Observed;
            }
            if work
                .refresh
                .borrow()
                .inflight_after(context, work, &self.channel, binding, &stamp)
            {
                return WriteFeedback::InFlight;
            }
        }
        WriteFeedback::Ordinary
    }
    fn receipts(&self, receipts: Vec<StepReceipt>) {
        let status = {
            let mut status = self.status.borrow_mut();
            let start = status.steps.len() - receipts.len();
            for (step, receipt) in status.steps[start..].iter_mut().zip(receipts) {
                step.receipt = receipt;
            }
            status.clone()
        };
        self.publish(status);
    }
    fn has_accepted(&self) -> bool {
        self.status
            .borrow()
            .steps
            .iter()
            .any(|step| matches!(step.receipt, StepReceipt::Accepted))
    }
    fn schedule_feedback(
        self: &Rc<Self>,
        context: &Context,
        work: &Rc<DeviceWork>,
        sink: &LogicalSink,
    ) {
        self.schedule_feedback_if(context, work, sink, self.has_accepted(), false);
    }
    fn schedule_feedback_if(
        self: &Rc<Self>,
        context: &Context,
        work: &Rc<DeviceWork>,
        sink: &LogicalSink,
        accepted: bool,
        include_all: bool,
    ) {
        if !self.feedback_enabled.get() || !accepted {
            return;
        }
        let shared = self.shared_feedback.borrow();
        if shared.as_ref().is_some_and(|scope| {
            scope.group.sink.life.is_closed() || !work_current(context, &scope.group.work)
        }) || ((!include_all || shared.is_none())
            && (sink.life.is_closed() || !work_current(context, work)))
        {
            self.feedback(CommandFeedback::Failed("read source retired".into()));
            return;
        }
        let stamps = if let Some(shared) = shared.as_ref() {
            shared.stamps.borrow()
        } else {
            self.write_stamps.borrow()
        };
        if stamps.is_empty() {
            self.feedback(CommandFeedback::Failed(
                "no readable feedback source".into(),
            ));
            return;
        }
        let group_did = shared
            .as_ref()
            .map(|scope| scope.group.work.key.id.as_str())
            .or_else(|| {
                context
                    .is_virtual_group(&self.did)
                    .then_some(self.did.as_str())
            });
        let group_ready = group_did.is_some_and(|did| {
            context.active_notification(did).is_some()
                && stamps
                    .iter()
                    .all(|target| notifications_ready(context, &target.work, &target.id))
        });
        let mut needed = Vec::new();
        let mut without_notifications = false;
        for FeedbackTarget {
            work: target,
            id,
            stamps: bindings,
        } in stamps.iter()
        {
            if shared.is_some()
                && !include_all
                && (target.key.id != self.did || *id != self.channel)
            {
                continue;
            }
            if group_ready || (group_did.is_none() && notifications_ready(context, target, id)) {
                continue;
            }
            without_notifications = true;
            let current = work_current(context, target);
            for (binding, stamp) in bindings {
                if !current || !target.observations.read_after(id, *binding, stamp) {
                    needed.push((target, id, *binding, stamp));
                }
            }
        }
        if needed.is_empty() {
            if without_notifications {
                self.feedback(CommandFeedback::Complete);
            }
            return;
        }
        let tracker = SyncTracker::new(self.clone(), needed.len());
        for (target, id, binding, stamp) in needed {
            let current = work_current(context, target);
            if !current {
                tracker.resolve(Some("read source retired".into()));
            } else if context.read_path_for(&target.key.id, id).is_none() {
                tracker.resolve(Some("no usable read path".into()));
            } else {
                target.refresh.borrow_mut().request_once(
                    context,
                    target,
                    id,
                    binding,
                    stamp,
                    tracker.clone(),
                );
                target.refresh_changed.notify(usize::MAX);
            }
        }
    }
    fn publish(&self, status: CommandStatus) {
        if let Some(entry) = self
            .commands
            .records
            .borrow_mut()
            .get_mut(&(self.did.clone(), self.channel.clone()))
        {
            if entry
                .inflight
                .as_ref()
                .is_some_and(|current| current.id == status.id)
            {
                entry.inflight = Some(status.clone());
            }
            if entry
                .last
                .as_ref()
                .is_some_and(|current| current.id == status.id)
            {
                entry.last = Some(status);
            }
        }
        self.commands.changed.notify(usize::MAX);
    }
    pub(super) fn feedback(&self, feedback: CommandFeedback) {
        let status = {
            let mut status = self.status.borrow_mut();
            status.feedback = Some(feedback);
            status.clone()
        };
        self.publish(status);
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
            PathKind::Local => "hub",
            PathKind::Direct => "direct",
            PathKind::Cloud => "cloud",
        });
        let result = status.result_text();
        let level = if status.result.as_ref().unwrap().is_ok() {
            log::Level::Info
        } else {
            log::Level::Warn
        };
        let now = Instant::now();
        let (queue_ms, response_ms) = match self.sent_at.get() {
            Some(sent) => (
                sent.duration_since(self.queued_at).as_millis(),
                Some(now.duration_since(sent).as_millis()),
            ),
            None => (now.duration_since(self.queued_at).as_millis(), None),
        };
        log::log!(
            level,
            "Xiaomi command result op={} did={} channel={} property={} target={} path={} result={} queue_ms={} response_ms={:?} total_ms={}",
            status.id,
            self.did,
            self.channel,
            status.properties_text(),
            status.target,
            path,
            result,
            queue_ms,
            response_ms,
            now.duration_since(self.queued_at).as_millis(),
        );
        self.commands.changed.notify(usize::MAX);
    }
}

pub(super) struct SyncTracker {
    operation: Rc<CommandOperation>,
    remaining: Cell<usize>,
    problems: RefCell<Vec<String>>,
}
impl SyncTracker {
    fn new(operation: Rc<CommandOperation>, count: usize) -> Rc<Self> {
        operation.feedback(CommandFeedback::Pending);
        Rc::new(Self {
            operation,
            remaining: Cell::new(count),
            problems: RefCell::new(Vec::new()),
        })
    }
    pub(super) fn resolve(&self, problem: Option<String>) {
        if let Some(problem) = problem {
            let mut problems = self.problems.borrow_mut();
            if !problems.contains(&problem) {
                problems.push(problem);
            }
        }
        let remaining = self.remaining.get() - 1;
        self.remaining.set(remaining);
        if remaining == 0 {
            let problems = self.problems.borrow();
            self.operation.feedback(if problems.is_empty() {
                CommandFeedback::Complete
            } else {
                CommandFeedback::Failed(problems.join("; "))
            });
        }
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
    sink: Rc<LogicalSink>,
    id: &str,
    request: CommandRequest,
    operation: Rc<CommandOperation>,
) -> Option<CommandRequest> {
    let binding = work.binding(id);
    let did = work.key.id.clone();
    let target = request.target;
    let pending = Rc::new(RefCell::new(Some(request)));
    let completion = pending.clone();
    let observing = work.clone();
    let observing_sink = sink.clone();
    let id = id.to_owned();
    let stamp = Rc::new(RefCell::new(None));
    let capture_stamp = stamp.clone();
    let capture_work = work.clone();
    let capture_id = id.clone();
    let guard_work = work.clone();
    let guard_sink = sink.clone();
    let guard_context = context.clone();
    let sent_context = context.clone();
    let guard_handle = handle.clone();
    let guard_did = did.clone();
    let guard_id = id.clone();
    let result_context = context.clone();
    let result_handle = handle.clone();
    let submitted = operation.clone();
    let hooks = SendHooks::new(
        move || {
            if guard_work.life.is_closed()
                || guard_sink.life.is_closed()
                || !guard_context.current_local(&guard_handle)
            {
                SendDecision::Cancel
            } else if matches!(guard_context.control_path_for(&guard_did, &guard_id), Some(ControlPath::Local(selected)) if selected.same_connection(&guard_handle))
            {
                SendDecision::Send
            } else {
                SendDecision::Withdraw
            }
        },
        move || {
            submitted.mark_write(&sent_context, &capture_work);
            submitted.submitted(super::model::PathKind::Local);
            *capture_stamp.borrow_mut() =
                Some(capture_work.observations.write_stamp(&capture_id, binding));
        },
    );
    handle
        .set_property_guarded(&did, binding, target, hooks, move |outcome| {
            result_context.local_device_outcome(&result_handle, &observing, &outcome);
            if outcome.result == Err(CentralError::Withdrawn)
                && outcome.stage == RequestStage::NotSent
            {
                return;
            }
            let request = completion.borrow_mut().take().unwrap();
            match outcome.result {
                Ok(()) => {
                    operation.receipts(vec![StepReceipt::Accepted]);
                    if let Some(stamp) = stamp.borrow_mut().take() {
                        let result = observing.observations.accept_write(
                            &observing,
                            &observing_sink,
                            &id,
                            &stamp,
                            operation.write_feedback(&result_context, &observing, binding),
                            request,
                        );
                        operation.schedule_feedback(&result_context, &observing, &observing_sink);
                        operation.finish(result);
                    } else {
                        let error =
                            CommandError::SendFailed("gateway accepted an unsent write".into());
                        operation.reject(request, error);
                    }
                }
                Err(error) => {
                    if outcome.stage != RequestStage::NotSent {
                        operation.receipts(vec![properties::local_receipt(&error)]);
                    }
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
    work: Rc<DeviceWork>,
    sink: Rc<LogicalSink>,
    function_id: String,
) -> Result<(), AdapterError> {
    future::or(
        async {
            futures_lite::future::or(work.life.ended(), sink.life.ended()).await;
            Ok(())
        },
        async {
            while let Some(request) = receiver.next_operation().await {
                send_owned(
                    request,
                    context.clone(),
                    work.clone(),
                    sink.clone(),
                    &function_id,
                )
                .await;
            }
            Ok(())
        },
    )
    .await
}

/// Keep every original receipt alive when its consumer or device is retired.
async fn send_owned(
    request: DeviceRequest,
    context: Rc<Context>,
    work: Rc<DeviceWork>,
    sink: Rc<LogicalSink>,
    id: &str,
) {
    let light = work
        .native
        .device()
        .functions()
        .iter()
        .find(|function| function.id() == id)
        .unwrap()
        .lighting()
        .is_some();
    let gate = (light
        || matches!(
            &request,
            DeviceRequest::Curtain(_) | DeviceRequest::Fan(_) | DeviceRequest::AirConditioner(_)
        ))
    .then(|| {
        context
            .write_gates
            .for_device(&work.key.id, context.clock.clone())
    });
    let completion = Rc::new((Cell::new(false), Event::new()));
    let done = completion.clone();
    let owner = context.clone();
    let id = id.to_owned();
    context.tasks.spawn(async move {
        let result = if let Some(gate) = gate {
            let _permit = gate.acquire().await;
            match request {
                DeviceRequest::Curtain(request) => {
                    curtain::send(request, owner, work, sink, &id, gate).await
                }
                DeviceRequest::Fan(request) => {
                    fan::send(request, owner, work, sink, &id, gate).await
                }
                DeviceRequest::AirConditioner(request) => {
                    air_conditioner::send(request, owner, work, sink, &id, gate).await
                }
                request => lighting::send(request, owner, work, sink, &id, gate).await,
            }
        } else {
            let DeviceRequest::Power(request) = request else {
                unreachable!("only outlet commands have no device write gate")
            };
            send_power(request, owner, work, sink, &id).await
        };
        done.0.set(true);
        done.1.notify(usize::MAX);
        result
    });
    loop {
        let changed = completion.1.listen();
        if completion.0.get() {
            return;
        }
        changed.await;
    }
}

async fn send_power(
    request: CommandRequest,
    context: Rc<Context>,
    work: Rc<DeviceWork>,
    sink: Rc<LogicalSink>,
    function_id: &str,
) -> Result<(), AdapterError> {
    let did = work.key.id.clone();
    let binding = work.binding(function_id);
    let target = request.target;
    let command = context
        .commands
        .begin(&did, function_id, binding, target, request.queued_at());
    let mut request = request;
    loop {
        if work.life.is_closed() || sink.life.is_closed() {
            command.0.reject(request, CommandError::Cancelled);
            break;
        }
        if let Err(error) = request.current_result() {
            command.0.reject(request, error);
            break;
        }
        let Some(path) = context.control_path_for(&did, function_id) else {
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
                    sink.clone(),
                    function_id,
                    request,
                    command.0.clone(),
                )
                .await
                else {
                    break;
                };
                request = withdrawn;
            }
            ControlPath::Direct(client) => {
                use crate::xiaomi::lan::{Failure, RequestError, SendStage};
                let stamp = RefCell::new(None);
                let result = super::lan::set_properties(
                    &client,
                    &did,
                    &[(binding, serde_json::Value::Bool(target))],
                    || {
                        !work.life.is_closed()
                            && !sink.life.is_closed()
                            && request.current_result().is_ok()
                            && matches!(context.control_path_for(&did, function_id), Some(ControlPath::Direct(selected)) if selected.same(&client))
                    },
                    || {
                        let _ = request.mark_sent();
                        command.0.mark_write(&context, &work);
                        command.0.submitted(super::model::PathKind::Direct);
                        *stamp.borrow_mut() = Some(work.observations.write_stamp(function_id, binding));
                    },
                ).await;
                match result {
                    Err(error)
                        if error.stage == SendStage::BeforeSend
                            && error.failure == Failure::Withdrawn =>
                    {
                        continue;
                    }
                    Err(error) => {
                        context.lan_business_failure(&did, &client, &error)?;
                        if error.stage == SendStage::BeforeSend {
                            continue;
                        }
                        command.0.receipts(vec![properties::direct_receipt(&error)]);
                        command
                            .0
                            .reject(request, properties::direct_command_error(&error));
                    }
                    Ok(results) => {
                        command.0.receipts(
                            results
                                .items
                                .iter()
                                .map(|item| match item {
                                    Ok(()) => StepReceipt::Accepted,
                                    Err(error) => properties::direct_receipt(error),
                                })
                                .collect(),
                        );
                        let mut maintenance = None;
                        if results.protocol_error {
                            maintenance = context
                                .lan_business_failure(
                                    &did,
                                    &client,
                                    &RequestError {
                                        stage: SendStage::Sent,
                                        failure: Failure::InvalidResponse,
                                    },
                                )
                                .err();
                            command.0.reject(
                                request,
                                CommandError::SendFailed("invalid direct response".into()),
                            );
                        } else if let Some(error) =
                            results.items.iter().find_map(|item| item.as_ref().err())
                        {
                            command
                                .0
                                .reject(request, properties::direct_command_error(error));
                        } else {
                            let result = work.observations.accept_write(
                                &work,
                                &sink,
                                function_id,
                                &stamp.into_inner().unwrap(),
                                command.0.write_feedback(&context, &work, binding),
                                request,
                            );
                            command.0.finish(result);
                        }
                        command.0.schedule_feedback(&context, &work, &sink);
                        if let Some(error) = maintenance {
                            return Err(error);
                        }
                    }
                }
                break;
            }
            ControlPath::Cloud(credentials) => {
                let stamp = work.observations.write_stamp(function_id, binding);
                command.0.mark_write(&context, &work);
                command.0.submitted(super::model::PathKind::Cloud);
                match context
                    .cloud
                    .set_property(&credentials.access_token, &did, binding, target)
                    .await
                {
                    Ok(()) => {
                        command.0.receipts(vec![StepReceipt::Accepted]);
                        let result = work.observations.accept_write(
                            &work,
                            &sink,
                            function_id,
                            &stamp,
                            command.0.write_feedback(&context, &work, binding),
                            request,
                        );
                        command.0.schedule_feedback(&context, &work, &sink);
                        command.0.finish(result);
                    }
                    Err(error) => {
                        command.0.receipts(vec![properties::cloud_receipt(&error)]);
                        let command_error = match error.kind() {
                            crate::xiaomi::cloud::CloudErrorKind::Timeout => CommandError::Timeout,
                            crate::xiaomi::cloud::CloudErrorKind::Business(code)
                            | crate::xiaomi::cloud::CloudErrorKind::PropertyRejected(code) => {
                                CommandError::Rejected(format!("cloud code {code}"))
                            }
                            _ => CommandError::SendFailed(error.to_string()),
                        };
                        command.0.reject(request, command_error);
                        let current = !work.life.is_closed() && !sink.life.is_closed();
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
    Ok(())
}

#[cfg(test)]
mod tests;

mod lighting;
