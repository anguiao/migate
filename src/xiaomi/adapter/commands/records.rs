use super::CommandLease;
use crate::{
    device::{CommandError, CommandRequest},
    xiaomi::{
        adapter::model::{
            ChannelCommands, CommandProperty, CommandStatus, CommandStep, CommandTarget, PathKind,
            StepReceipt,
        },
        capability::PropertyBinding,
    },
};
use event_listener::Event;
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
    time::Instant,
};

/// Diagnostics retain one current and one completed operation per channel, never a history.
#[derive(Default)]
pub(in crate::xiaomi::adapter) struct Commands {
    pub(super) next: Cell<u64>,
    pub(super) records: RefCell<BTreeMap<(String, String), ChannelCommands>>,
    pub(in crate::xiaomi::adapter) changed: Event,
}
impl Commands {
    pub(super) fn begin(
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
    pub(super) fn begin_operation(
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
        }))
    }
    pub(in crate::xiaomi::adapter) fn snapshot(
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
    pub(in crate::xiaomi::adapter) fn remove(&self, did: &str) {
        self.records
            .borrow_mut()
            .retain(|(device, _), _| device != did);
        self.changed.notify(usize::MAX);
    }
}
pub(in crate::xiaomi::adapter) struct CommandOperation {
    pub(super) commands: Rc<Commands>,
    pub(super) did: String,
    pub(super) channel: String,
    pub(super) status: RefCell<CommandStatus>,
    pub(super) queued_at: Instant,
    pub(super) sent_at: Cell<Option<Instant>>,
    pub(super) pending_step: RefCell<Option<CommandProperty>>,
}
impl CommandOperation {
    pub(super) fn pending_step(&self, property: (PropertyBinding, serde_json::Value)) {
        *self.pending_step.borrow_mut() = Some(property.into());
    }
    pub(super) fn properties(&self, properties: Vec<(PropertyBinding, serde_json::Value)>) {
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

    pub(super) fn submitted(&self, path: PathKind) {
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
    pub(super) fn receipts(&self, receipts: Vec<StepReceipt>) {
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
    pub(super) fn publish(&self, status: CommandStatus) {
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
    pub(super) fn reject(&self, request: CommandRequest, error: CommandError) {
        self.finish(Err(error.clone()));
        request.reject(error);
    }
    pub(super) fn finish(&self, result: Result<(), CommandError>) {
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
