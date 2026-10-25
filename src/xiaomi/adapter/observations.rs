use super::{
    lighting::{Origin, PropertyKey, key},
    work::DeviceWork,
};
use crate::device::{CommandError, CommandRequest};
use crate::xiaomi::capability::PropertyBinding;
use std::{cell::RefCell, collections::BTreeMap};

#[derive(Default)]
struct Counts {
    source: u64,
    observed: u64,
    notified: u64,
    refresh: u64,
    // A completed write can make the displayed feedback too old for deduplication.
    unconfirmed_write: bool,
}
#[derive(Default)]
pub(super) struct Observations {
    channels: RefCell<BTreeMap<PropertyKey, Counts>>,
}
#[derive(Clone)]
pub(super) struct ReadStamp {
    channel_source: u64,
    observed: u64,
    refresh: u64,
}
#[derive(Clone)]
pub(super) struct WriteStamp {
    notified: u64,
}
impl Observations {
    pub(super) fn invalidate_write(&self, id: &str, binding: PropertyBinding) {
        let mut channels = self.channels.borrow_mut();
        let counts = channels.entry(key(id, binding)).or_default();
        counts.observed += 1;
        counts.unconfirmed_write = true;
    }
    pub(super) fn has_unconfirmed_write(&self, id: &str, binding: PropertyBinding) -> bool {
        self.channels
            .borrow()
            .get(&key(id, binding))
            .is_some_and(|counts| counts.unconfirmed_write)
    }
    pub(super) fn function_source_changed(&self, id: &str) {
        for ((channel, _, _), counts) in self.channels.borrow_mut().iter_mut() {
            if channel == id {
                counts.source = counts.source.wrapping_add(1);
            }
        }
    }
    pub(super) fn refresh(&self, id: &str, binding: PropertyBinding) {
        self.channels
            .borrow_mut()
            .entry(key(id, binding))
            .or_default()
            .refresh += 1;
    }
    pub(super) fn read_stamp(&self, id: &str, binding: PropertyBinding) -> ReadStamp {
        let mut channels = self.channels.borrow_mut();
        let counts = channels.entry(key(id, binding)).or_default();
        ReadStamp {
            channel_source: counts.source,
            observed: counts.observed,
            refresh: counts.refresh,
        }
    }
    pub(super) fn observe_read(&self, work: &DeviceWork, id: &str, stamp: &ReadStamp, value: bool) {
        let binding = work.binding(id);
        if self.read_is_current(work, id, binding, stamp) {
            let mut channels = self.channels.borrow_mut();
            let counts = channels.entry(key(id, binding)).or_default();
            counts.observed += 1;
            work.physical().function(id).unwrap().observe(value);
        }
    }
    pub(super) fn read_is_current(
        &self,
        work: &DeviceWork,
        id: &str,
        binding: PropertyBinding,
        stamp: &ReadStamp,
    ) -> bool {
        let mut channels = self.channels.borrow_mut();
        let counts = channels.entry(key(id, binding)).or_default();
        !work.life.is_closed()
            && counts.source == stamp.channel_source
            && counts.observed == stamp.observed
            && counts.refresh == stamp.refresh
    }
    pub(super) fn notify(&self, work: &DeviceWork, id: &str, value: bool) {
        let binding = work.binding(id);
        if work.life.is_closed() {
            return;
        }
        let mut channels = self.channels.borrow_mut();
        let counts = channels.entry(key(id, binding)).or_default();
        counts.observed += 1;
        counts.notified += 1;
        work.physical().function(id).unwrap().observe(value);
    }
    pub(super) fn write_stamp(&self, id: &str, binding: PropertyBinding) -> WriteStamp {
        WriteStamp {
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
        work: &DeviceWork,
        sink: &super::state::LogicalSink,
        id: &str,
        stamp: &WriteStamp,
        request: CommandRequest,
    ) -> Result<(), CommandError> {
        let binding = work.binding(id);
        if work.life.is_closed() || sink.life.is_closed() {
            request.reject(CommandError::Cancelled);
            return Err(CommandError::Cancelled);
        }
        // A closed or already settled request must never observe: acceptance
        // itself only settles, the source observation owns the target state.
        if let Err(error) = request.current_result() {
            request.reject(error.clone());
            return Err(error);
        }
        let target = request.target;
        let mut channels = self.channels.borrow_mut();
        let counts = channels.entry(key(id, binding)).or_default();
        counts.observed += 1;
        if counts.notified == stamp.notified {
            sink.physical.function(id).unwrap().observe(target);
        }
        request.accept().map(|_| ())
    }

    pub(super) fn observed(
        &self,
        work: &DeviceWork,
        id: &str,
        binding: PropertyBinding,
        origin: Origin,
    ) {
        if work.life.is_closed() {
            return;
        }
        let mut channels = self.channels.borrow_mut();
        let counts = channels.entry(key(id, binding)).or_default();
        counts.observed += 1;
        counts.notified += u64::from(matches!(origin, Origin::Notification));
        if !matches!(origin, Origin::Cache) {
            counts.unconfirmed_write = false;
        }
    }
}
