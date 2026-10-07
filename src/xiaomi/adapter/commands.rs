use super::model::{ChannelCommands, CommandStatus, PathKind};
use crate::{device::CommandError, xiaomi::capability::PropertyBinding};
use event_listener::Event;
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
};

/// Diagnostics retain one current and one completed operation per channel, never a history.
#[derive(Default)]
pub(super) struct Commands {
    next: Cell<u64>,
    records: RefCell<BTreeMap<(String, String), ChannelCommands>>,
    pub(super) changed: Event,
}
impl Commands {
    pub(super) fn begin(
        self: &Rc<Self>,
        did: &str,
        channel: &str,
        binding: PropertyBinding,
        target: bool,
    ) -> CommandLease {
        let id = self.next.get() + 1;
        self.next.set(id);
        let status = CommandStatus {
            id,
            binding,
            target,
            actual_path: None,
            result: None,
        };
        let mut records = self.records.borrow_mut();
        let entry = records
            .entry((did.into(), channel.into()))
            .or_insert_with(|| ChannelCommands {
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
            finished: Cell::new(false),
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
pub(super) struct CommandOperation {
    commands: Rc<Commands>,
    did: String,
    channel: String,
    status: RefCell<CommandStatus>,
    finished: Cell<bool>,
}
impl CommandOperation {
    pub(super) fn submitted(&self, path: PathKind) {
        if self.finished.get() || self.status.borrow().actual_path.is_some() {
            return;
        }
        self.status.borrow_mut().actual_path = Some(path);
        let status = self.status.borrow().clone();
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
            "Xiaomi command start op={} did={} channel={} property={}.{} target={} path={}",
            status.id,
            self.did,
            self.channel,
            status.binding.siid,
            status.binding.piid,
            status.target,
            path
        );
        self.commands.changed.notify(usize::MAX);
    }
    pub(super) fn finish(&self, result: Result<(), CommandError>) {
        if self.finished.replace(true) {
            return;
        }
        self.status.borrow_mut().result = Some(result.clone());
        let status = self.status.borrow().clone();
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
        let result = match result {
            Ok(()) => "accepted".into(),
            Err(error) => error.to_string(),
        };
        let level = if status.result.as_ref().unwrap().is_ok() {
            log::Level::Info
        } else {
            log::Level::Warn
        };
        log::log!(
            level,
            "Xiaomi command result op={} did={} channel={} property={}.{} target={} path={} result={}",
            status.id,
            self.did,
            self.channel,
            status.binding.siid,
            status.binding.piid,
            status.target,
            path,
            result
        );
        self.commands.changed.notify(usize::MAX);
    }
}
/// Only the consumer owns this lease; a transport callback cannot extend its lifetime.
pub(super) struct CommandLease(pub(super) Rc<CommandOperation>);
impl Drop for CommandLease {
    fn drop(&mut self) {
        self.0.finish(Err(CommandError::Cancelled));
    }
}

#[cfg(test)]
mod tests;
