use super::{
    lighting::{PropertyKey, key},
    state::DeviceRecord,
};
use crate::device::{CommandError, CommandRequest};
use crate::xiaomi::capability::PropertyBinding;
use serde_json::Value;
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
};

#[derive(Default)]
struct Counts {
    observed: u64,
    notified: u64,
    refresh: u64,
}
#[derive(Default)]
pub(super) struct Observations {
    source_epoch: Cell<u64>,
    channels: RefCell<BTreeMap<PropertyKey, Counts>>,
    feedback: RefCell<BTreeMap<PropertyKey, Feedback>>,
}
struct Feedback {
    value: Value,
    binding: u64,
    source: u64,
}
#[derive(Clone)]
pub(super) struct ReadStamp {
    binding: u64,
    source: u64,
    observed: u64,
    refresh: u64,
}
#[derive(Clone)]
pub(super) struct WriteStamp {
    source: u64,
    binding: u64,
    notified: u64,
}
impl Observations {
    pub(super) fn source_changed(&self) {
        self.source_epoch.set(self.source_epoch.get() + 1);
        // Only outstanding receipts have feedback entries; the new source is validated at ingress.
        for entry in self.feedback.borrow_mut().values_mut() {
            entry.source = self.source_epoch.get();
        }
    }
    pub(super) fn refresh(&self, id: &str, binding: PropertyBinding) {
        self.channels
            .borrow_mut()
            .entry(key(id, binding))
            .or_default()
            .refresh += 1;
    }
    pub(super) fn read_stamp(
        &self,
        record: &DeviceRecord,
        id: &str,
        binding: PropertyBinding,
    ) -> ReadStamp {
        let mut channels = self.channels.borrow_mut();
        let counts = channels.entry(key(id, binding)).or_default();
        ReadStamp {
            binding: record.binding_generation.get(),
            source: self.source_epoch.get(),
            observed: counts.observed,
            refresh: counts.refresh,
        }
    }
    pub(super) fn observe_read(
        &self,
        record: &DeviceRecord,
        id: &str,
        stamp: &ReadStamp,
        value: bool,
    ) {
        let binding = record.binding(id);
        if self.read_is_current(record, id, binding, stamp) {
            let mut channels = self.channels.borrow_mut();
            let counts = channels.entry(key(id, binding)).or_default();
            counts.observed += 1;
            record.physical.function(id).unwrap().observe(value);
        }
    }
    pub(super) fn read_is_current(
        &self,
        record: &DeviceRecord,
        id: &str,
        binding: PropertyBinding,
        stamp: &ReadStamp,
    ) -> bool {
        let mut channels = self.channels.borrow_mut();
        let counts = channels.entry(key(id, binding)).or_default();
        !record.life.is_closed()
            && record.binding_generation.get() == stamp.binding
            && self.source_epoch.get() == stamp.source
            && counts.observed == stamp.observed
            && counts.refresh == stamp.refresh
    }
    pub(super) fn notify(&self, record: &DeviceRecord, id: &str, value: bool) {
        let binding = record.binding(id);
        if record.life.is_closed() {
            return;
        }
        let mut channels = self.channels.borrow_mut();
        let counts = channels.entry(key(id, binding)).or_default();
        counts.observed += 1;
        counts.notified += 1;
        record.physical.function(id).unwrap().observe(value);
    }
    pub(super) fn write_stamp(
        &self,
        record: &DeviceRecord,
        id: &str,
        binding: PropertyBinding,
    ) -> WriteStamp {
        WriteStamp {
            source: self.source_epoch.get(),
            binding: record.binding_generation.get(),
            notified: self
                .channels
                .borrow_mut()
                .entry(key(id, binding))
                .or_default()
                .notified,
        }
    }
    pub(super) fn accept_write(
        &self,
        record: &DeviceRecord,
        id: &str,
        stamp: &WriteStamp,
        request: CommandRequest,
    ) -> Result<(), CommandError> {
        let binding = record.binding(id);
        if record.life.is_closed() || record.binding_generation.get() != stamp.binding {
            request.reject(CommandError::Cancelled);
            return Err(CommandError::Cancelled);
        }
        let mut channels = self.channels.borrow_mut();
        let counts = channels.entry(key(id, binding)).or_default();
        counts.observed += 1;
        if counts.notified == stamp.notified {
            request.accept().map(|_| ())
        } else {
            request.accept_preserving_state().map(|_| ())
        }
    }

    pub(super) fn observed(
        &self,
        record: &DeviceRecord,
        id: &str,
        binding: PropertyBinding,
        notified: bool,
    ) {
        if record.life.is_closed() {
            return;
        }
        let mut channels = self.channels.borrow_mut();
        let counts = channels.entry(key(id, binding)).or_default();
        counts.observed += 1;
        counts.notified += u64::from(notified);
    }
    pub(super) fn write_is_current(
        &self,
        record: &DeviceRecord,
        id: &str,
        binding: PropertyBinding,
        stamp: &WriteStamp,
    ) -> bool {
        !record.life.is_closed()
            && record.binding_generation.get() == stamp.binding
            && self.source_epoch.get() == stamp.source
            && self
                .channels
                .borrow_mut()
                .entry(key(id, binding))
                .or_default()
                .notified
                == stamp.notified
    }
    pub(super) fn register_feedback(
        &self,
        record: &DeviceRecord,
        id: &str,
        properties: &[(PropertyBinding, Value)],
    ) {
        let mut feedback = self.feedback.borrow_mut();
        for (binding, value) in properties {
            feedback.insert(
                key(id, *binding),
                Feedback {
                    value: value.clone(),
                    binding: record.binding_generation.get(),
                    source: self.source_epoch.get(),
                },
            );
        }
    }
    pub(super) fn finish_feedback(&self, id: &str) {
        self.feedback.borrow_mut().retain(|key, _| key.0 != id);
    }
    pub(super) fn is_feedback(
        &self,
        record: &DeviceRecord,
        id: &str,
        binding: PropertyBinding,
        value: &Value,
    ) -> bool {
        self.feedback
            .borrow()
            .get(&key(id, binding))
            .is_some_and(|entry| {
                entry.value == *value
                    && entry.binding == record.binding_generation.get()
                    && entry.source == self.source_epoch.get()
            })
    }
}
