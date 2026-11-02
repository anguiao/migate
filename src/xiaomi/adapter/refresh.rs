use super::{
    model::AdapterError,
    observations::{PropertyKey, key},
    routing::ControlPath,
    state::{Context, Lifetime},
    work::DeviceWork,
};
use crate::xiaomi::capability::PropertyBinding;
use event_listener::Event;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    rc::Rc,
    time::Duration,
};

mod cloud;
mod direct;
mod local;
mod polling;
mod read;

pub(super) use cloud::cloud;
pub(super) use direct::direct;
pub(super) use local::local;
pub(super) use polling::polling;
use read::read_one;

const ROUND_DELAY: Duration = Duration::from_millis(200);
const RETRY_DELAY: Duration = Duration::from_secs(3);

#[derive(Clone)]
pub(super) struct Refresh {
    pub(super) id: String,
    pub(super) binding: PropertyBinding,
    pub(super) attempts: u8,
    pub(super) ready_at: Duration,
    periodic: bool,
    // Reserved reads and cleanup leases retain the notification baseline generation.
    baseline: Option<u64>,
}
enum ReadResult {
    Observed,
    Retired,
    Failed(String),
}

fn record_battery_read(work: &DeviceWork, read: &Refresh, result: &ReadResult) {
    if read.id != "battery"
        || work
            .native
            .device()
            .battery()
            .is_none_or(|battery| read.binding != battery.property())
    {
        return;
    }
    match result {
        ReadResult::Observed => work.set_battery_read_problem(None),
        ReadResult::Failed(problem) => work.set_battery_read_problem(Some(problem.clone())),
        ReadResult::Retired => {}
    }
}

fn ordered_read(work: &DeviceWork, read: &Refresh) -> bool {
    work.native
        .device()
        .battery()
        .is_some_and(|battery| read.id == "battery" && read.binding == battery.property())
        || work.native.device().functions().iter().any(|function| {
            function.id() == read.id
                && (function.lighting().is_some()
                    || function.kind().is_read_only()
                    || function.curtain().is_some()
                    || function.fan().is_some()
                    || function.air_conditioner().is_some())
        })
}

// A real error may itself invalidate the selected route. Record it while the
// responding flight is still current, before the existing routing transition.
fn resolve_initial_failure(
    context: &Context,
    work: &DeviceWork,
    read: &Refresh,
    flight: &ReadFlight,
    stamp: &super::observations::ReadStamp,
) {
    if work.life.is_closed()
        || flight.life.is_closed()
        || !flight.current(context, &work.key.id, &read.id)
        || !work.contains(&read.id, read.binding)
        || !work.refresh.borrow().required(read)
        || !work
            .observations
            .read_is_current(work, &read.id, read.binding, stamp)
    {
        return;
    }
    let current_work = context
        .live
        .borrow()
        .devices
        .get(&work.key.id)
        .and_then(|slot| slot.admitted.as_deref())
        .is_some_and(|current| std::ptr::eq(current, work));
    if current_work {
        work.resolve_initial(&read.id, read.binding);
    }
}
#[derive(Clone, Copy)]
enum ReadQueue {
    Local,
    Direct,
    Cloud,
}
#[derive(Clone, Copy)]
enum ReadKind {
    Any,
    Foreground,
    Periodic,
}
impl ReadKind {
    fn accepts(self, periodic: bool) -> bool {
        matches!(self, Self::Any)
            || matches!(
                (self, periodic),
                (Self::Foreground, false) | (Self::Periodic, true)
            )
    }
}
impl ReadQueue {
    fn accepts(self, path: &ControlPath) -> bool {
        matches!(
            (self, path),
            (Self::Local, ControlPath::Local(_))
                | (Self::Direct, ControlPath::Direct(_))
                | (Self::Cloud, ControlPath::Cloud(_))
        )
    }
}

#[derive(Default)]
pub(super) struct RefreshState {
    pending: VecDeque<Refresh>,
    inflight: BTreeMap<PropertyKey, Rc<ReadFlight>>,
    dirty_again: BTreeSet<PropertyKey>,
    problems: BTreeMap<PropertyKey, String>,
    baseline_generations: BTreeMap<PropertyKey, u64>,
    initial_results: BTreeSet<PropertyKey>,
    initial_changed: Rc<Event>,
}
impl RefreshState {
    pub(super) fn new(initial_changed: Rc<Event>) -> Self {
        Self {
            initial_changed,
            ..Self::default()
        }
    }
    pub(super) fn initial_complete(&self, id: &str, binding: PropertyBinding) -> bool {
        self.initial_results.contains(&key(id, binding))
    }
    pub(super) fn resolve_initial(&mut self, id: &str, binding: PropertyBinding) {
        if self.initial_results.insert(key(id, binding)) {
            self.initial_changed.notify(usize::MAX);
        }
    }
    pub(super) fn reset_initial(&mut self) {
        self.initial_results.clear();
        self.notify_initial_changed();
    }
    pub(super) fn notify_initial_changed(&self) {
        self.initial_changed.notify(usize::MAX);
    }
    pub(super) fn request(&mut self, id: &str, binding: PropertyBinding) {
        self.queue(id, binding, false);
    }
    pub(super) fn request_baseline(&mut self, id: &str, binding: PropertyBinding) {
        self.queue(id, binding, true);
    }
    fn queue(&mut self, id: &str, binding: PropertyBinding, baseline: bool) {
        let baseline = baseline.then(|| {
            *self
                .baseline_generations
                .entry(key(id, binding))
                .or_default()
        });
        if let Some(pending) = self
            .pending
            .iter_mut()
            .find(|read| read.id == id && read.binding == binding)
        {
            pending.periodic = false;
            if pending.baseline.is_some() {
                pending.baseline = baseline;
            }
        } else {
            self.pending.push_back(Refresh {
                id: id.into(),
                binding,
                attempts: 0,
                ready_at: Duration::ZERO,
                periodic: false,
                baseline,
            });
        }
    }
    fn request_poll(&mut self, id: &str, binding: PropertyBinding) -> bool {
        if self.contains(id, binding) {
            return false;
        }
        self.pending.push_back(Refresh {
            id: id.into(),
            binding,
            attempts: 0,
            ready_at: Duration::ZERO,
            periodic: true,
            baseline: None,
        });
        true
    }
    pub(super) fn satisfy_baseline(&mut self, id: &str, binding: PropertyBinding) {
        let generation = self
            .baseline_generations
            .entry(key(id, binding))
            .or_default();
        *generation = generation.wrapping_add(1);
        self.pending
            .retain(|read| read.key() != key(id, binding) || read.baseline.is_none());
    }
    fn required(&self, read: &Refresh) -> bool {
        read.baseline
            .is_none_or(|generation| self.baseline_generations[&read.key()] == generation)
    }
    pub(super) fn contains(&self, id: &str, binding: PropertyBinding) -> bool {
        self.inflight.contains_key(&key(id, binding))
            || self
                .pending
                .iter()
                .any(|read| read.id == id && read.binding == binding)
    }
    pub(super) fn request_dirty(&mut self, id: &str, binding: PropertyBinding) -> bool {
        let key = key(id, binding);
        if self.inflight.contains_key(&key) {
            self.dirty_again.insert(key);
            false
        } else if self.contains(id, binding) {
            self.request(id, binding);
            false
        } else {
            self.request(id, binding);
            true
        }
    }
    fn finish_dirty(&mut self, id: &str, binding: PropertyBinding) -> bool {
        if self.dirty_again.remove(&key(id, binding)) {
            self.request(id, binding);
            true
        } else {
            false
        }
    }
    fn next_due(&self, kind: ReadKind, eligible: impl Fn(&str) -> bool) -> Option<Duration> {
        self.pending
            .iter()
            .filter(|read| {
                kind.accepts(read.periodic)
                    && !self.inflight.contains_key(&read.key())
                    && eligible(&read.id)
            })
            .map(|read| read.ready_at)
            .min()
    }
    fn reserve(
        &mut self,
        now: Duration,
        path_for: impl Fn(&str) -> Option<ControlPath>,
        queue: ReadQueue,
        kind: ReadKind,
    ) -> Option<(Refresh, Rc<ReadFlight>)> {
        let find = |periodic| {
            self.pending.iter().enumerate().find_map(|(index, read)| {
                if read.periodic != periodic
                    || !kind.accepts(read.periodic)
                    || read.ready_at > now
                    || self.inflight.contains_key(&read.key())
                {
                    return None;
                }
                let path = path_for(&read.id)?;
                queue.accepts(&path).then_some((index, path))
            })
        };
        // Baselines and command feedback take precedence over background polls.
        let (index, path) = find(false).or_else(|| find(true))?;
        let read = self.pending.remove(index).unwrap();
        let flight = Rc::new(ReadFlight {
            life: Lifetime::default(),
            path,
        });
        self.inflight.insert(read.key(), flight.clone());
        Some((read, flight))
    }
    fn requeue(&mut self, read: Refresh) {
        if !self.required(&read) {
            return;
        }
        if let Some(pending) = self
            .pending
            .iter_mut()
            .find(|queued| queued.key() == read.key())
        {
            if read.baseline.is_none() {
                pending.baseline = None;
            }
        } else {
            self.pending.push_back(read);
        }
    }
    fn complete(&mut self, mut read: Refresh, result: ReadResult, now: Duration) {
        if !self.required(&read) {
            return;
        }
        match result {
            ReadResult::Observed => {
                self.problems.remove(&read.key());
            }
            ReadResult::Retired if read.periodic => {}
            ReadResult::Retired => self.requeue(read),
            ReadResult::Failed(problem) => {
                self.resolve_initial(&read.id, read.binding);
                self.problems.insert(read.key(), problem);
                if read.periodic {
                    return;
                }
                if read.attempts == 3 {
                    self.pending.retain(|queued| queued.key() != read.key());
                } else {
                    read.attempts += 1;
                    read.ready_at = now + RETRY_DELAY;
                    if let Some(queued) = self
                        .pending
                        .iter_mut()
                        .find(|queued| queued.key() == read.key())
                    {
                        *queued = read;
                    } else {
                        self.pending.push_back(read);
                    }
                }
            }
        }
    }
    pub(super) fn problem_for(&self, id: &str) -> Option<String> {
        let values = self
            .problems
            .iter()
            .filter(|((channel, _, _), _)| channel == id)
            .map(|(_, problem)| problem.clone())
            .collect::<Vec<_>>();
        (!values.is_empty()).then(|| values.join("; "))
    }
    pub(super) fn property_problem(&self, id: &str, binding: PropertyBinding) -> Option<String> {
        self.problems.get(&key(id, binding)).cloned()
    }
    pub(super) fn problems_except(&self, id: &str) -> Vec<String> {
        self.problems
            .iter()
            .filter(|((entry, _, _), _)| entry != id)
            .map(|(_, problem)| problem.clone())
            .collect()
    }
    #[cfg(test)]
    pub(super) fn snapshot(&self) -> RefreshSnapshot {
        RefreshSnapshot {
            pending: self.pending.iter().cloned().collect(),
            inflight: self.inflight.keys().cloned().collect(),
            problems: self.problems.clone(),
        }
    }
    #[cfg(test)]
    pub(super) fn exhaust_pending(&mut self, id: &str, binding: PropertyBinding) {
        self.pending
            .iter_mut()
            .find(|read| read.id == id && read.binding == binding)
            .unwrap()
            .attempts = 3;
    }
}
#[cfg(test)]
pub(super) struct RefreshSnapshot {
    pub(super) pending: Vec<Refresh>,
    pub(super) inflight: BTreeSet<PropertyKey>,
    pub(super) problems: BTreeMap<PropertyKey, String>,
}

pub(super) struct ReadFlight {
    life: Lifetime,
    path: ControlPath,
}
impl Refresh {
    fn key(&self) -> PropertyKey {
        key(&self.id, self.binding)
    }
}
impl ReadFlight {
    fn current(&self, context: &Context, did: &str, id: &str) -> bool {
        context
            .read_path_for(did, id)
            .is_some_and(|path| self.path.same(&path))
    }
}
fn works(context: &Context) -> Vec<Rc<DeviceWork>> {
    context
        .live
        .borrow()
        .devices
        .values()
        .filter_map(|slot| slot.admitted.clone())
        .collect()
}
pub(super) fn cancel(work: &DeviceWork) {
    for flight in work.refresh.borrow().inflight.values() {
        flight.life.close();
    }
    work.refresh_changed.notify(usize::MAX);
}
pub(super) fn cancel_function(work: &DeviceWork, id: &str) {
    for ((channel, _, _), flight) in &work.refresh.borrow().inflight {
        if channel == id {
            flight.life.close();
        }
    }
    work.refresh_changed.notify(usize::MAX);
}
/// A route change retires only reads whose captured route actually ended.
pub(super) fn reconcile_state_read(
    work: &DeviceWork,
    context: &Context,
    id: &str,
    binding: PropertyBinding,
    available: bool,
) {
    let current = {
        let refresh = work.refresh.borrow();
        let flight = refresh.inflight.get(&key(id, binding));
        if let Some(flight) = flight
            && !flight.current(context, &work.key.id, id)
        {
            flight.life.close();
        }
        flight.is_some_and(|flight| flight.current(context, &work.key.id, id))
    };
    if available && !current {
        work.refresh_property(id, binding);
    }
    work.refresh_changed.notify(usize::MAX);
}
fn retire(context: &Context) {
    for work in works(context) {
        let did = &work.key.id;
        for ((id, _, _), flight) in &work.refresh.borrow().inflight {
            if work.life.is_closed() || !flight.current(context, did, id) {
                flight.life.close();
            }
        }
    }
}
fn reserve(
    work: &Rc<DeviceWork>,
    context: &Context,
    queue: ReadQueue,
    kind: ReadKind,
) -> Option<(Refresh, Rc<ReadFlight>)> {
    if work.life.is_closed() {
        return None;
    }
    work.refresh.borrow_mut().reserve(
        context.clock.now(),
        |id| {
            let path = context.read_path_for(&work.key.id, id)?;
            if let ControlPath::Local(handle) = &path
                && !context.local_function_reads_allowed(&work.key.id, id, handle)
            {
                None
            } else {
                Some(path)
            }
        },
        queue,
        kind,
    )
}

struct ReadCleanup {
    work: Rc<DeviceWork>,
    flight: Rc<ReadFlight>,
    read: Refresh,
    retry: bool,
}
impl Drop for ReadCleanup {
    fn drop(&mut self) {
        self.flight.life.close();
        let mut state = self.work.refresh.borrow_mut();
        if state
            .inflight
            .get(&self.read.key())
            .is_some_and(|flight| Rc::ptr_eq(flight, &self.flight))
        {
            state.inflight.remove(&self.read.key());
        }
        let dirty = state.finish_dirty(&self.read.id, self.read.binding);
        if self.retry && !self.read.periodic && !self.work.life.is_closed() {
            state.requeue(self.read.clone());
        }
        drop(state);
        if dirty {
            self.work
                .observations
                .refresh(&self.read.id, self.read.binding);
        }
        self.work.refresh_changed.notify(usize::MAX);
    }
}

#[cfg(test)]
mod tests;
