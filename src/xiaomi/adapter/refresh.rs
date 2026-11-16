use super::{
    model::AdapterError,
    observations::{PropertyKey, input_key, key},
    routing::ControlPath,
    state::{Context, Lifetime},
    work::DeviceWork,
};
use crate::xiaomi::capability::{PropertyBinding, ReadInput};
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
mod selection;

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
    pub(super) input: ReadInput,
    pub(super) attempts: u8,
    pub(super) ready_at: Duration,
    periodic: bool,
    // Reserved reads and cleanup leases retain the notification baseline generation.
    baseline: Option<u64>,
}
pub(super) enum ReadResult {
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
            .is_none_or(|battery| read.input.property() != Some(battery.property()))
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
    read.input == ReadInput::SharedFeedback && work.shared_feedback_plan()
        || work.native.device().battery().is_some_and(|battery| {
            read.id == "battery" && read.input.property() == Some(battery.property())
        })
        || work.native.device().functions().iter().any(|function| {
            function.id() == read.id
                && (function.lighting().is_some()
                    || function.kind().is_read_only()
                    || function.curtain().is_some()
                    || function.fan().is_some()
                    || function.air_conditioner().is_some()
                    || function.heater().is_some()
                    || function.relay_mode().is_some())
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
        || !flight.current(context, &work.key.id, &read.id, read.input)
        || !work.contains_input(&read.id, read.input)
        || !work.refresh.borrow().required(read)
        || !work
            .observations
            .read_is_current_input(work, &read.id, read.input, stamp)
    {
        return;
    }
    let current_work = context
        .directory
        .borrow()
        .devices
        .get(&work.key.id)
        .and_then(|slot| slot.admitted.as_deref())
        .is_some_and(|current| std::ptr::eq(current, work));
    if current_work {
        work.resolve_initial(&read.id, read.input);
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
    function_paths: BTreeMap<String, Option<ControlPath>>,
    shared_path: Option<Option<ControlPath>>,
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
    pub(super) fn initial_complete(&self, id: &str, input: impl Into<ReadInput>) -> bool {
        self.initial_results.contains(&input_key(id, input.into()))
    }
    pub(super) fn resolve_initial(&mut self, id: &str, input: ReadInput) {
        if self.initial_results.insert(input_key(id, input)) {
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
        self.request_input(id, ReadInput::Property(binding), false);
    }
    pub(super) fn request_baseline(&mut self, id: &str, binding: PropertyBinding) {
        self.request_input(id, ReadInput::Property(binding), true);
    }
    pub(super) fn request_input(&mut self, id: &str, input: ReadInput, baseline: bool) {
        let baseline = baseline.then(|| {
            *self
                .baseline_generations
                .entry(input_key(id, input))
                .or_default()
        });
        if let Some(pending) = self
            .pending
            .iter_mut()
            .find(|read| read.id == id && read.input == input)
        {
            pending.periodic = false;
            if pending.baseline.is_some() {
                pending.baseline = baseline;
            }
        } else {
            self.pending.push_back(Refresh {
                id: id.into(),
                input,
                attempts: 0,
                ready_at: Duration::ZERO,
                periodic: false,
                baseline,
            });
        }
    }
    fn request_poll(&mut self, id: &str, binding: PropertyBinding) -> bool {
        self.request_poll_input(id, ReadInput::Property(binding))
    }
    pub(super) fn request_poll_input(&mut self, id: &str, input: ReadInput) -> bool {
        if self.contains_input(id, input) {
            return false;
        }
        self.pending.push_back(Refresh {
            id: id.into(),
            input,
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
        self.contains_input(id, ReadInput::Property(binding))
    }
    pub(super) fn contains_input(&self, id: &str, input: ReadInput) -> bool {
        self.inflight.contains_key(&input_key(id, input))
            || self
                .pending
                .iter()
                .any(|read| read.id == id && read.input == input)
    }
    pub(super) fn request_dirty(&mut self, id: &str, binding: PropertyBinding) -> bool {
        self.request_dirty_input(id, ReadInput::Property(binding))
    }
    pub(super) fn request_dirty_input(&mut self, id: &str, input: ReadInput) -> bool {
        let key = input_key(id, input);
        if self.inflight.contains_key(&key) {
            self.dirty_again.insert(key);
            false
        } else if self.contains_input(id, input) {
            self.request_input(id, input, false);
            false
        } else {
            self.request_input(id, input, false);
            true
        }
    }
    fn finish_dirty_input(&mut self, id: &str, input: ReadInput) -> bool {
        if self.dirty_again.remove(&input_key(id, input)) {
            self.request_input(id, input, false);
            true
        } else {
            false
        }
    }
    fn next_due(
        &self,
        kind: ReadKind,
        eligible: impl Fn(&str, ReadInput) -> bool,
    ) -> Option<Duration> {
        self.pending
            .iter()
            .filter(|read| {
                kind.accepts(read.periodic)
                    && !self.inflight.contains_key(&read.key())
                    && eligible(&read.id, read.input)
            })
            .map(|read| read.ready_at)
            .min()
    }
    fn reserve(
        &mut self,
        now: Duration,
        path_for: impl Fn(&str, ReadInput) -> Option<ControlPath>,
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
                let path = path_for(&read.id, read.input)?;
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
                self.resolve_initial(&read.id, read.input);
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
            .filter(|((channel, _), _)| channel == id)
            .map(|(_, problem)| problem.clone())
            .collect::<Vec<_>>();
        (!values.is_empty()).then(|| values.join("; "))
    }
    pub(super) fn property_problem(&self, id: &str, binding: PropertyBinding) -> Option<String> {
        self.problems.get(&key(id, binding)).cloned()
    }
    pub(super) fn input_problem(&self, id: &str, input: ReadInput) -> Option<String> {
        self.problems.get(&input_key(id, input)).cloned()
    }
    pub(super) fn problems_except(&self, id: &str) -> Vec<String> {
        self.problems
            .iter()
            .filter(|((entry, _), _)| entry != id)
            .map(|(_, problem)| problem.clone())
            .collect()
    }
}
pub(super) struct ReadFlight {
    life: Lifetime,
    path: ControlPath,
}
impl Refresh {
    fn key(&self) -> PropertyKey {
        input_key(&self.id, self.input)
    }
}
impl ReadFlight {
    fn current(&self, context: &Context, did: &str, id: &str, input: ReadInput) -> bool {
        context
            .read_path_for_input(did, id, input)
            .is_some_and(|path| self.path.same(&path))
    }
}
fn works(context: &Context) -> Vec<Rc<DeviceWork>> {
    context
        .directory
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
    for ((channel, _), flight) in &work.refresh.borrow().inflight {
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
    reconcile_input(work, context, id, ReadInput::Property(binding), available);
}

pub(super) fn reconcile_input(
    work: &DeviceWork,
    context: &Context,
    id: &str,
    input: ReadInput,
    available: bool,
) {
    let current = {
        let refresh = work.refresh.borrow();
        let flight = refresh.inflight.get(&input_key(id, input));
        if let Some(flight) = flight
            && !flight.current(context, &work.key.id, id, input)
        {
            flight.life.close();
        }
        flight.is_some_and(|flight| flight.current(context, &work.key.id, id, input))
    };
    if available && !current {
        work.refresh_input(id, input);
    }
    work.refresh_changed.notify(usize::MAX);
}
fn retire(context: &Context) {
    for work in works(context) {
        let did = &work.key.id;
        for ((id, input), flight) in &work.refresh.borrow().inflight {
            if work.life.is_closed() || !flight.current(context, did, id, *input) {
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
        |id, input| {
            let path = context.read_path_for_input(&work.key.id, id, input)?;
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
        let dirty = state.finish_dirty_input(&self.read.id, self.read.input);
        if self.retry && !self.read.periodic && !self.work.life.is_closed() {
            state.requeue(self.read.clone());
        }
        drop(state);
        if dirty {
            self.work
                .observations
                .refresh_input(&self.read.id, self.read.input);
        }
        self.work.refresh_changed.notify(usize::MAX);
    }
}

#[cfg(test)]
mod tests;
