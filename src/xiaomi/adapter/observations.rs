use super::{model::PathKind, work::DeviceWork};
use crate::device::{CommandError, CommandRequest};
use crate::xiaomi::capability::{PropertyBinding, ReadInput};
use std::{cell::RefCell, collections::BTreeMap};

pub(super) type PropertyKey = (String, ReadInput);
pub(super) fn key(id: &str, binding: PropertyBinding) -> PropertyKey {
    input_key(id, ReadInput::Property(binding))
}
pub(super) fn input_key(id: &str, input: ReadInput) -> PropertyKey {
    (id.into(), input)
}

#[derive(Clone, Copy)]
pub(super) enum Origin {
    Read,
    Notification,
    Cache,
}

#[derive(Clone, Copy)]
pub(super) struct Observation {
    pub(super) source: PathKind,
    pub(super) source_time: Option<i64>,
    pub(super) received_at: i64,
}

#[derive(Default)]
struct Counts {
    source: u64,
    observed: u64,
    notified: u64,
    refresh: u64,
    fresh: bool,
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
    source: u64,
    observed: u64,
    notified: u64,
    read_pending: bool,
}

impl Observations {
    /// Retained snapshots may seed an unknown property, but never replace a
    /// real read or notification already accepted for this runtime.
    pub(super) fn permits(&self, id: &str, binding: PropertyBinding, origin: Origin) -> bool {
        !matches!(origin, Origin::Cache)
            || !self
                .channels
                .borrow()
                .get(&key(id, binding))
                .is_some_and(|counts| counts.fresh)
    }

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
    pub(super) fn invalidate_input(&self, id: &str, input: ReadInput) {
        let mut channels = self.channels.borrow_mut();
        let counts = channels.entry(input_key(id, input)).or_default();
        counts.observed += 1;
        counts.unconfirmed_write = true;
    }
    pub(super) fn function_source_changed(&self, id: &str) {
        for ((channel, _), counts) in self.channels.borrow_mut().iter_mut() {
            if channel == id {
                counts.source = counts.source.wrapping_add(1);
            }
        }
    }
    pub(super) fn refresh(&self, id: &str, binding: PropertyBinding) {
        self.refresh_input(id, ReadInput::Property(binding));
    }
    pub(super) fn refresh_input(&self, id: &str, input: ReadInput) {
        self.channels
            .borrow_mut()
            .entry(input_key(id, input))
            .or_default()
            .refresh += 1;
    }
    pub(super) fn read_stamp_input(&self, id: &str, input: ReadInput) -> ReadStamp {
        let mut channels = self.channels.borrow_mut();
        let counts = channels.entry(input_key(id, input)).or_default();
        ReadStamp {
            channel_source: counts.source,
            observed: counts.observed,
            refresh: counts.refresh,
        }
    }
    pub(super) fn observe_read(
        &self,
        work: &DeviceWork,
        id: &str,
        stamp: &ReadStamp,
        value: bool,
    ) -> bool {
        let binding = work.binding(id);
        if self.read_is_current(work, id, binding, stamp) {
            self.observed(work, id, binding, Origin::Read);
            work.physical().function(id).unwrap().observe(value);
            true
        } else {
            false
        }
    }
    pub(super) fn read_is_current(
        &self,
        work: &DeviceWork,
        id: &str,
        binding: PropertyBinding,
        stamp: &ReadStamp,
    ) -> bool {
        self.read_is_current_input(work, id, ReadInput::Property(binding), stamp)
    }
    pub(super) fn read_is_current_input(
        &self,
        work: &DeviceWork,
        id: &str,
        input: ReadInput,
        stamp: &ReadStamp,
    ) -> bool {
        let mut channels = self.channels.borrow_mut();
        let counts = channels.entry(input_key(id, input)).or_default();
        !work.life.is_closed()
            && counts.source == stamp.channel_source
            && counts.observed == stamp.observed
            && counts.refresh == stamp.refresh
    }
    pub(super) fn notify(&self, work: &DeviceWork, id: &str, value: bool, origin: Origin) -> bool {
        let binding = work.binding(id);
        if work.life.is_closed() || !self.permits(id, binding, origin) {
            return false;
        }
        self.observed(work, id, binding, origin);
        work.physical().function(id).unwrap().observe(value);
        true
    }
    pub(super) fn write_stamp(
        &self,
        id: &str,
        binding: PropertyBinding,
        read_pending: bool,
    ) -> WriteStamp {
        let mut channels = self.channels.borrow_mut();
        let counts = channels.entry(key(id, binding)).or_default();
        WriteStamp {
            source: counts.source,
            observed: counts.observed,
            notified: counts.notified,
            read_pending,
        }
    }
    pub(super) fn accept_write(
        &self,
        work: &DeviceWork,
        sink: &super::state::LogicalSink,
        id: &str,
        stamp: &WriteStamp,
        preserve_read: bool,
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
        let unchanged = counts.notified == stamp.notified
            && (!preserve_read
                || stamp.read_pending
                || counts.source != stamp.source
                || counts.observed == stamp.observed);
        counts.observed += 1;
        if unchanged
            && work
                .native
                .device()
                .functions()
                .iter()
                .find(|function| function.id() == id)
                .unwrap()
                .relay_mode()
                .is_none()
        {
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
        self.observed_input(work, id, ReadInput::Property(binding), origin);
    }
    pub(super) fn observed_input(
        &self,
        work: &DeviceWork,
        id: &str,
        input: ReadInput,
        origin: Origin,
    ) {
        if work.life.is_closed() {
            return;
        }
        work.resolve_initial(id, input);
        if matches!(origin, Origin::Cache) {
            return;
        }
        let mut channels = self.channels.borrow_mut();
        let counts = channels.entry(input_key(id, input)).or_default();
        counts.observed += 1;
        counts.notified += u64::from(matches!(origin, Origin::Notification));
        counts.fresh = true;
        counts.unconfirmed_write = false;
    }
}
