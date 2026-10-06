use super::state::DeviceRecord;
use crate::device::{CommandError, CommandRequest};
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
    channels: RefCell<BTreeMap<String, Counts>>,
}
pub(super) struct ReadStamp {
    binding: u64,
    source: u64,
    observed: u64,
    refresh: u64,
}
pub(super) struct WriteStamp {
    binding: u64,
    notified: u64,
}
impl Observations {
    pub(super) fn source_changed(&self) {
        self.source_epoch.set(self.source_epoch.get() + 1);
    }
    pub(super) fn refresh(&self, id: &str) {
        self.channels
            .borrow_mut()
            .entry(id.into())
            .or_default()
            .refresh += 1;
    }
    pub(super) fn read_stamp(&self, record: &DeviceRecord, id: &str) -> ReadStamp {
        let mut channels = self.channels.borrow_mut();
        let counts = channels.entry(id.into()).or_default();
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
        if self.read_is_current(record, id, stamp) {
            let mut channels = self.channels.borrow_mut();
            let counts = channels.entry(id.into()).or_default();
            counts.observed += 1;
            record.physical.function(id).unwrap().observe(value);
        }
    }
    pub(super) fn read_is_current(
        &self,
        record: &DeviceRecord,
        id: &str,
        stamp: &ReadStamp,
    ) -> bool {
        let mut channels = self.channels.borrow_mut();
        let counts = channels.entry(id.into()).or_default();
        !record.life.is_closed()
            && record.binding_generation.get() == stamp.binding
            && self.source_epoch.get() == stamp.source
            && counts.observed == stamp.observed
            && counts.refresh == stamp.refresh
    }
    pub(super) fn notify(&self, record: &DeviceRecord, id: &str, value: bool) {
        if record.life.is_closed() {
            return;
        }
        let mut channels = self.channels.borrow_mut();
        let counts = channels.entry(id.into()).or_default();
        counts.observed += 1;
        counts.notified += 1;
        record.physical.function(id).unwrap().observe(value);
    }
    pub(super) fn write_stamp(&self, record: &DeviceRecord, id: &str) -> WriteStamp {
        WriteStamp {
            binding: record.binding_generation.get(),
            notified: self
                .channels
                .borrow_mut()
                .entry(id.into())
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
        if record.life.is_closed() || record.binding_generation.get() != stamp.binding {
            request.reject(CommandError::Cancelled);
            return Err(CommandError::Cancelled);
        }
        let mut channels = self.channels.borrow_mut();
        let counts = channels.entry(id.into()).or_default();
        counts.observed += 1;
        if counts.notified == stamp.notified {
            request.accept().map(|_| ())
        } else {
            request.accept_preserving_state().map(|_| ())
        }
    }
}
