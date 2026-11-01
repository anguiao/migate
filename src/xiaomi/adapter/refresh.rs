use super::{
    model::AdapterError,
    observations::{Observation, PropertyKey, key},
    routing::ControlPath,
    state::{Context, Lifetime},
    work::DeviceWork,
};
use crate::xiaomi::{
    capability::PropertyBinding,
    central::{CentralError, RequestStage, SendDecision, SendHooks, SessionHandle},
};
use futures_lite::future;
use futures_util::future::join_all;
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet, VecDeque},
    rc::Rc,
    time::Duration,
};

const ROUND_DELAY: Duration = Duration::from_millis(200);
const RETRY_DELAY: Duration = Duration::from_secs(3);
const DIRECT_READ_LIMIT: usize = 8;

#[derive(Clone)]
pub(super) struct Refresh {
    pub(super) id: String,
    pub(super) binding: PropertyBinding,
    pub(super) attempts: u8,
    pub(super) ready_at: Duration,
    // Reserved reads and cleanup leases retain the notification baseline generation.
    baseline: Option<u64>,
    retryable: bool,
    waiters: Vec<Rc<super::commands::SyncTracker>>,
}
enum ReadResult {
    Observed,
    Retired,
    Failed(String),
}
#[derive(Clone, Copy)]
enum ReadQueue {
    Local,
    Direct,
    Cloud,
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
}
impl RefreshState {
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
            pending.retryable = true;
            if pending.baseline.is_some() {
                pending.baseline = baseline;
            }
        } else {
            self.pending.push_back(Refresh {
                id: id.into(),
                binding,
                attempts: 0,
                ready_at: Duration::ZERO,
                baseline,
                retryable: true,
                waiters: Vec::new(),
            });
        }
    }
    pub(super) fn request_once(
        &mut self,
        context: &Context,
        work: &DeviceWork,
        id: &str,
        binding: PropertyBinding,
        write: &super::observations::ReadStamp,
        waiter: Rc<super::commands::SyncTracker>,
    ) {
        let key = key(id, binding);
        if self.inflight_after(context, work, id, binding, write) {
            self.inflight[&key]
                .waiters
                .borrow_mut()
                .entry(key)
                .or_default()
                .push(waiter);
            return;
        }
        if let Some(pending) = self.pending.iter_mut().find(|read| read.key() == key) {
            pending.baseline = None;
            pending.waiters.push(waiter);
        } else {
            self.pending.push_back(Refresh {
                id: id.into(),
                binding,
                attempts: 0,
                ready_at: Duration::ZERO,
                baseline: None,
                retryable: false,
                waiters: vec![waiter],
            });
        }
    }
    fn fail_unroutable(&mut self, available: impl Fn(&str) -> bool) {
        self.pending.retain_mut(|read| {
            if !available(&read.id) {
                for waiter in read.waiters.drain(..) {
                    waiter.resolve(Some("no usable read path".into()));
                }
                return read.retryable;
            }
            true
        });
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
            || !read.waiters.is_empty()
            || self.inflight.get(&read.key()).is_some_and(|flight| {
                flight.retry_required.borrow().contains(&read.key())
                    || flight
                        .waiters
                        .borrow()
                        .get(&read.key())
                        .is_some_and(|waiters| !waiters.is_empty())
            })
    }
    pub(super) fn contains(&self, id: &str, binding: PropertyBinding) -> bool {
        self.inflight.contains_key(&key(id, binding))
            || self
                .pending
                .iter()
                .any(|read| read.id == id && read.binding == binding)
    }
    pub(super) fn inflight_after(
        &self,
        context: &Context,
        work: &DeviceWork,
        id: &str,
        binding: PropertyBinding,
        write: &super::observations::ReadStamp,
    ) -> bool {
        let key = key(id, binding);
        self.inflight.get(&key).is_some_and(|flight| {
            !flight.life.is_closed()
                && flight.current(context, &work.key.id, id)
                && flight.stamps.borrow().get(&key).is_none_or(|stamp| {
                    stamp.at_or_after(write)
                        && work.observations.read_is_current(work, id, binding, stamp)
                })
        })
    }
    pub(super) fn retain_inflight_retry(&mut self, id: &str, binding: PropertyBinding) {
        let key = key(id, binding);
        self.inflight[&key].retry_required.borrow_mut().insert(key);
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
    fn next_due(&self, eligible: impl Fn(&str) -> bool) -> Option<Duration> {
        self.pending
            .iter()
            .filter(|read| !self.inflight.contains_key(&read.key()) && eligible(&read.id))
            .map(|read| read.ready_at)
            .min()
    }
    fn reserve(
        &mut self,
        now: Duration,
        path_for: impl Fn(&str) -> Option<ControlPath>,
        queue: ReadQueue,
    ) -> Option<(Refresh, Rc<ReadFlight>)> {
        let (index, path) = self.pending.iter().enumerate().find_map(|(index, read)| {
            if read.ready_at > now || self.inflight.contains_key(&read.key()) {
                return None;
            }
            let path = path_for(&read.id)?;
            queue.accepts(&path).then_some((index, path))
        })?;
        let read = self.pending.remove(index).unwrap();
        let flight = Rc::new(ReadFlight {
            life: Lifetime::default(),
            path,
            stamps: RefCell::new(BTreeMap::new()),
            waiters: RefCell::new(BTreeMap::new()),
            retry_required: RefCell::new(BTreeSet::new()),
        });
        self.inflight.insert(read.key(), flight.clone());
        Some((read, flight))
    }
    fn requeue(&mut self, mut read: Refresh) {
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
            pending.retryable |= read.retryable;
            pending.waiters.append(&mut read.waiters);
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
            ReadResult::Retired => {
                if read.retryable {
                    self.requeue(read);
                }
            }
            ReadResult::Failed(problem) => {
                self.problems.insert(read.key(), problem);
                if read.retryable && read.attempts == 3 {
                    self.pending.retain_mut(|queued| {
                        if queued.key() != read.key() {
                            true
                        } else if queued.waiters.is_empty() {
                            false
                        } else {
                            queued.retryable = false;
                            true
                        }
                    });
                } else if read.retryable {
                    read.attempts += 1;
                    read.ready_at = now + RETRY_DELAY;
                    if let Some(queued) = self
                        .pending
                        .iter_mut()
                        .find(|queued| queued.key() == read.key())
                    {
                        if queued.waiters.is_empty() {
                            *queued = read;
                        } else {
                            queued.retryable = true;
                            queued.attempts = read.attempts;
                            if read.baseline.is_none() {
                                queued.baseline = None;
                            }
                        }
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
    stamps: RefCell<BTreeMap<PropertyKey, super::observations::ReadStamp>>,
    waiters: RefCell<BTreeMap<PropertyKey, Vec<Rc<super::commands::SyncTracker>>>>,
    retry_required: RefCell<BTreeSet<PropertyKey>>,
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
    {
        let mut refresh = work.refresh.borrow_mut();
        refresh.fail_unroutable(|_| false);
        for flight in refresh.inflight.values() {
            flight.life.close();
        }
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
        let mut refresh = work.refresh.borrow_mut();
        refresh.fail_unroutable(|id| {
            !work.life.is_closed() && context.read_path_for(did, id).is_some()
        });
        for ((id, _, _), flight) in &refresh.inflight {
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
) -> Option<(Refresh, Rc<ReadFlight>)> {
    if work.life.is_closed() {
        return None;
    }
    work.refresh.borrow_mut().reserve(
        context.clock.now(),
        |id| context.read_path_for(&work.key.id, id),
        queue,
    )
}

pub(super) async fn local(context: Rc<Context>, handle: SessionHandle) -> Result<(), AdapterError> {
    let mut next_round = None;
    loop {
        let listener = context.refresh_changed.listen();
        retire(&context);
        let candidates = works(&context);
        let ready = candidates
            .iter()
            .filter(|work| context.local_reads_allowed(&work.key.id, &handle))
            .filter_map(|work| {
                work.refresh
                    .borrow()
                    .next_due(|id| context.local_function_reads_allowed(&work.key.id, id, &handle))
            })
            .min();
        let Some(ready) = ready else {
            next_round = None;
            listener.await;
            continue;
        };
        let deadline = next_round
            .map_or(context.clock.now() + ROUND_DELAY, |scheduled: Duration| {
                scheduled.min(context.clock.now() + ROUND_DELAY)
            })
            .max(ready);
        next_round = Some(deadline);
        future::or(context.clock.wait_until(deadline), async {
            listener.await;
        })
        .await;
        if context.clock.now() < deadline {
            continue;
        }
        let round = works(&context).into_iter().filter_map(|work| {
            if !context.current_local(&handle) {
                return None;
            }
            reserve(&work, &context, ReadQueue::Local)
                .map(|(read, flight)| read_one(work, read, flight, context.clone()))
        });
        // Keep the local round barrier: one property per DID, concurrent DIDs.
        join_all(round)
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()?;
        next_round = Some(context.clock.now() + ROUND_DELAY);
    }
}
pub(super) async fn direct(context: Rc<Context>) -> Result<(), AdapterError> {
    let mut next_round = BTreeMap::<String, Duration>::new();
    loop {
        let listener = context.refresh_changed.listen();
        let changed = context.changed.listen();
        retire(&context);
        let candidates = works(&context);
        next_round.retain(|did, _| candidates.iter().any(|work| &work.key.id == did));
        let mut in_flight = candidates
            .iter()
            .map(|work| {
                usize::from(
                    work.refresh
                        .borrow()
                        .inflight
                        .values()
                        .any(|flight| matches!(flight.path, ControlPath::Direct(_))),
                )
            })
            .sum::<usize>();
        let now = context.clock.now();
        let mut due_at = None;
        for work in candidates {
            if in_flight == DIRECT_READ_LIMIT {
                break;
            }
            let refresh = work.refresh.borrow();
            if refresh
                .inflight
                .values()
                .any(|flight| matches!(flight.path, ControlPath::Direct(_)))
            {
                continue;
            }
            let due = refresh.next_due(|id| {
                matches!(
                    context.read_path_for(&work.key.id, id),
                    Some(ControlPath::Direct(_))
                )
            });
            drop(refresh);
            let Some(due) = due else {
                continue;
            };
            let next = next_round
                .entry(work.key.id.clone())
                .or_insert(now + ROUND_DELAY);
            let due = due.max(*next);
            if due > now {
                due_at = Some(due_at.map_or(due, |previous: Duration| previous.min(due)));
                continue;
            }
            if let Some((read, flight)) = reserve(&work, &context, ReadQueue::Direct) {
                *next = now + ROUND_DELAY;
                in_flight += 1;
                if let Some(mapping) = context.legacy_mapping(&work.key.id)
                    && mapping.contains(read.binding)
                {
                    let mut reads = vec![read];
                    {
                        let mut refresh = work.refresh.borrow_mut();
                        let mut index = 0;
                        while index < refresh.pending.len() {
                            let candidate = &refresh.pending[index];
                            if candidate.id == reads[0].id
                                && candidate.ready_at <= now
                                && mapping.contains(candidate.binding)
                                && !refresh.inflight.contains_key(&candidate.key())
                            {
                                let candidate = refresh.pending.remove(index).unwrap();
                                refresh.inflight.insert(candidate.key(), flight.clone());
                                reads.push(candidate);
                            } else {
                                index += 1;
                            }
                        }
                    }
                    context
                        .tasks
                        .spawn(read_legacy(work, reads, flight, mapping, context.clone()));
                } else {
                    context
                        .tasks
                        .spawn(read_one(work, read, flight, context.clone()));
                }
            }
        }
        let wake = future::or(listener, changed);
        if in_flight < DIRECT_READ_LIMIT {
            if let Some(due) = due_at {
                future::or(wake, context.clock.wait_until(due)).await;
            } else {
                wake.await;
            }
        } else {
            wake.await;
        }
    }
}

pub(super) async fn cloud(context: Rc<Context>) -> Result<(), AdapterError> {
    loop {
        let listener = context.refresh_changed.listen();
        let changed = context.changed.listen();
        let credentials = context.credentials_changed.listen();
        retire(&context);
        for work in works(&context) {
            while let Some((read, flight)) = reserve(&work, &context, ReadQueue::Cloud) {
                context
                    .tasks
                    .spawn(read_one(work.clone(), read, flight, context.clone()));
            }
        }
        let due = works(&context)
            .iter()
            .filter(|work| matches!(context.read_path(&work.key.id), Some(ControlPath::Cloud(_))))
            .filter_map(|work| {
                work.refresh.borrow().next_due(|id| {
                    matches!(
                        context.read_path_for(&work.key.id, id),
                        Some(ControlPath::Cloud(_))
                    )
                })
            })
            .min();
        let wake = future::or(
            async {
                listener.await;
            },
            future::or(
                async {
                    changed.await;
                },
                async {
                    credentials.await;
                },
            ),
        );
        if let Some(due) = due {
            future::or(wake, context.clock.wait_until(due)).await;
        } else {
            wake.await;
        }
    }
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
        retain_retry(&mut self.read, &self.flight);
        let mut state = self.work.refresh.borrow_mut();
        if state
            .inflight
            .get(&self.read.key())
            .is_some_and(|flight| Rc::ptr_eq(flight, &self.flight))
        {
            state.inflight.remove(&self.read.key());
        }
        let dirty = state.finish_dirty(&self.read.id, self.read.binding);
        if self.retry {
            finish_waiters(&mut self.read, &self.flight, Some("read retired".into()));
        }
        if self.retry && self.read.retryable && !self.work.life.is_closed() {
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
fn retain_retry(read: &mut Refresh, flight: &ReadFlight) {
    if flight.retry_required.borrow_mut().remove(&read.key()) {
        read.retryable = true;
        read.baseline = None;
    }
}
fn finish_waiters(read: &mut Refresh, flight: &ReadFlight, problem: Option<String>) {
    let mut waiters = std::mem::take(&mut read.waiters);
    if let Some(mut attached) = flight.waiters.borrow_mut().remove(&read.key()) {
        waiters.append(&mut attached);
    }
    for waiter in waiters {
        waiter.resolve(problem.clone());
    }
}
fn read_legacy(
    work: Rc<DeviceWork>,
    reads: Vec<Refresh>,
    flight: Rc<ReadFlight>,
    mapping: &'static super::legacy_miio::Mapping,
    context: Rc<Context>,
) -> impl std::future::Future<Output = Result<(), AdapterError>> {
    let cleanups = reads
        .iter()
        .map(|read| ReadCleanup {
            work: work.clone(),
            flight: flight.clone(),
            read: read.clone(),
            retry: true,
        })
        .collect::<Vec<_>>();
    async move {
        use crate::xiaomi::lan::{Failure, SendStage};
        let mut cleanups = cleanups;
        let did = &work.key.id;
        let stamps = reads
            .iter()
            .map(|read| work.observations.read_stamp(&read.id, read.binding))
            .collect::<Vec<_>>();
        for (read, stamp) in reads.iter().zip(&stamps) {
            flight.stamps.borrow_mut().insert(read.key(), stamp.clone());
        }
        let ControlPath::Direct(client) = &flight.path else {
            unreachable!()
        };
        let eligible = |read: &Refresh, stamp: &super::observations::ReadStamp| {
            !flight.life.is_closed()
                && !work.life.is_closed()
                && flight.current(&context, did, &read.id)
                && work.refresh.borrow().required(read)
                && work
                    .observations
                    .read_is_current(&work, &read.id, read.binding, stamp)
        };
        let result = future::or(
            async {
                work.life.ended().await;
                None
            },
            future::or(
                async {
                    flight.life.ended().await;
                    None
                },
                async {
                    Some(
                        super::legacy_miio::read(mapping, client, || {
                            reads
                                .iter()
                                .zip(&stamps)
                                .any(|(read, stamp)| eligible(read, stamp))
                        })
                        .await,
                    )
                },
            ),
        )
        .await;
        if let Some(Err(error)) = &result
            && !(error.stage == SendStage::BeforeSend && error.failure == Failure::Withdrawn)
        {
            context.lan_business_failure(did, client, error)?;
        }
        for ((mut read, stamp), cleanup) in reads.into_iter().zip(stamps).zip(&mut cleanups) {
            retain_retry(&mut read, &flight);
            cleanup.retry = false;
            cleanup.read.waiters.clear();
            if work.life.is_closed() || !work.contains(&read.id, read.binding) {
                finish_waiters(&mut read, &flight, Some("read source retired".into()));
                continue;
            }
            let outcome = match &result {
                None => ReadResult::Retired,
                Some(Err(error))
                    if error.stage == SendStage::BeforeSend
                        && error.failure == Failure::Withdrawn =>
                {
                    ReadResult::Retired
                }
                Some(Err(error)) => ReadResult::Failed(work.problem("read", read.binding, error)),
                Some(Ok(_)) if !flight.current(&context, did, &read.id) => ReadResult::Retired,
                Some(Ok(values)) => match &values[mapping.index(read.binding).unwrap()] {
                    Ok(value) => match work.observe_read_at(
                        &read.id,
                        read.binding,
                        &stamp,
                        value,
                        Observation {
                            source: super::model::PathKind::Direct,
                            source_time: None,
                            received_at: context.clock.unix_now(),
                        },
                    ) {
                        Ok(()) => {
                            context.refresh_group_states(did);
                            ReadResult::Observed
                        }
                        Err(error) => {
                            ReadResult::Failed(work.problem("read", read.binding, &error))
                        }
                    },
                    Err(()) => ReadResult::Failed(work.problem(
                        "read",
                        read.binding,
                        &"invalid legacy miIO feedback",
                    )),
                },
            };
            let current = !matches!(outcome, ReadResult::Failed(_))
                || work
                    .observations
                    .read_is_current(&work, &read.id, read.binding, &stamp);
            let problem = match &outcome {
                ReadResult::Observed
                    if work.observations.read_after(&read.id, read.binding, &stamp) =>
                {
                    None
                }
                ReadResult::Observed | ReadResult::Retired => {
                    Some("read retired or superseded".into())
                }
                ReadResult::Failed(problem) => Some(problem.clone()),
            };
            finish_waiters(&mut read, &flight, problem);
            if current {
                work.refresh
                    .borrow_mut()
                    .complete(read, outcome, context.clock.now());
            }
        }
        if !work.life.is_closed() {
            work.report(&context, did);
        }
        work.refresh_changed.notify(usize::MAX);
        Ok(())
    }
}

fn read_one(
    work: Rc<DeviceWork>,
    read: Refresh,
    flight: Rc<ReadFlight>,
    context: Rc<Context>,
) -> impl std::future::Future<Output = Result<(), AdapterError>> {
    // The lease exists before first poll: dropping a session/task returns its reserved demand.
    let cleanup = ReadCleanup {
        work: work.clone(),
        flight: flight.clone(),
        read: read.clone(),
        retry: true,
    };
    async move {
        let mut cleanup = cleanup;
        let did = work.key.id.clone();
        let ordered = work
            .native
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
            });
        let stamp = Rc::new(work.observations.read_stamp(&read.id, read.binding));
        flight
            .stamps
            .borrow_mut()
            .insert(read.key(), (*stamp).clone());
        let reading = async {
            if flight.life.is_closed()
                || !flight.current(&context, &did, &read.id)
                || !work.refresh.borrow().required(&read)
            {
                return Ok(ReadResult::Retired);
            }
            match &flight.path {
                ControlPath::Local(handle) => {
                    let validation = Rc::new(std::cell::RefCell::new(Ok(())));
                    let observed_validation = validation.clone();
                    let observing = work.clone();
                    let observed_id = read.id.clone();
                    let observed_stamp = stamp.clone();
                    let guard_work = work.clone();
                    let guard_context = context.clone();
                    let guard_handle = handle.clone();
                    let guard_id = read.id.clone();
                    let guard_did = did.clone();
                    let guard_stamp = stamp.clone();
                    let guard_flight = flight.clone();
                    let result_context = context.clone();
                    let result_handle = handle.clone();
                    let result_flight = flight.clone();
                    let outcome = handle
                        .read_property_observation_with_timeout_guarded(
                            &did,
                            read.binding,
                            Duration::from_secs(6),
                            SendHooks::new(
                                move || {
                                    if guard_flight.life.is_closed()
                                        || guard_work.life.is_closed()
                                        || !guard_context.current_local(&guard_handle)
                                    {
                                        SendDecision::Cancel
                                    } else if !guard_context.local_function_reads_allowed(
                                        &guard_did,
                                        &guard_id,
                                        &guard_handle,
                                    ) || !guard_work.observations.read_is_current(
                                        &guard_work,
                                        &guard_id,
                                        read.binding,
                                        &guard_stamp,
                                    ) {
                                        SendDecision::Withdraw
                                    } else {
                                        SendDecision::Send
                                    }
                                },
                                || {},
                            ),
                            move |outcome| {
                                // Observation counters only gate values; access failures still affect routing.
                                if !result_flight.life.is_closed() {
                                    result_context.local_device_outcome(
                                        &result_handle,
                                        &observing,
                                        &outcome,
                                    );
                                }
                                if !result_flight.life.is_closed()
                                    && !observing.life.is_closed()
                                    && result_flight.current(
                                        &result_context,
                                        &observing.key.id,
                                        &observed_id,
                                    )
                                    && let Ok(value) = outcome.result
                                {
                                    *observed_validation.borrow_mut() = observing.observe_read_at(
                                        &observed_id,
                                        read.binding,
                                        &observed_stamp,
                                        &value.value,
                                        Observation {
                                            source: super::model::PathKind::Local,
                                            source_time: value.update_time,
                                            received_at: result_context.clock.unix_now(),
                                        },
                                    );
                                    if observed_validation.borrow().is_ok() {
                                        result_context.refresh_group_states(&observing.key.id);
                                    }
                                }
                            },
                        )
                        .await;
                    match outcome.result {
                        Ok(_) => Ok(if flight.current(&context, &did, &read.id) {
                            match &*validation.borrow() {
                                Ok(()) => ReadResult::Observed,
                                Err(error) => {
                                    ReadResult::Failed(work.problem("read", read.binding, error))
                                }
                            }
                        } else {
                            ReadResult::Retired
                        }),
                        Err(CentralError::Withdrawn | CentralError::Cancelled)
                            if outcome.stage == RequestStage::NotSent =>
                        {
                            Ok(ReadResult::Retired)
                        }
                        Err(error) => Ok(ReadResult::Failed(work.problem(
                            "read",
                            read.binding,
                            &error,
                        ))),
                    }
                }
                ControlPath::Direct(client) => {
                    let result = super::lan::read_property(client, &did, read.binding, || {
                        !flight.life.is_closed()
                            && !work.life.is_closed()
                            && flight.current(&context, &did, &read.id)
                            && work.refresh.borrow().required(&read)
                            && work.observations.read_is_current(
                                &work,
                                &read.id,
                                read.binding,
                                &stamp,
                            )
                    })
                    .await;
                    match result {
                        Ok(reading) => {
                            if work.life.is_closed() || !flight.current(&context, &did, &read.id) {
                                Ok(ReadResult::Retired)
                            } else {
                                match work.observe_read_at(
                                    &read.id,
                                    read.binding,
                                    &stamp,
                                    &reading.value,
                                    Observation {
                                        source: super::model::PathKind::Direct,
                                        source_time: reading.update_time,
                                        received_at: context.clock.unix_now(),
                                    },
                                ) {
                                    Ok(()) => {
                                        context.refresh_group_states(&did);
                                        Ok(ReadResult::Observed)
                                    }
                                    Err(error) => Ok(ReadResult::Failed(work.problem(
                                        "read",
                                        read.binding,
                                        &error,
                                    ))),
                                }
                            }
                        }
                        Err(error)
                            if error.stage == crate::xiaomi::lan::SendStage::BeforeSend
                                && error.failure == crate::xiaomi::lan::Failure::Withdrawn =>
                        {
                            Ok(ReadResult::Retired)
                        }
                        Err(error) => {
                            context.lan_business_failure(&did, client, &error)?;
                            Ok(ReadResult::Failed(work.problem(
                                "read",
                                read.binding,
                                &error,
                            )))
                        }
                    }
                }
                ControlPath::Cloud(credentials) => {
                    match context
                        .cloud
                        .read_property_observation(&credentials.access_token, &did, read.binding)
                        .await
                    {
                        Ok(reading) => {
                            let mut validation = Ok(());
                            if !work.life.is_closed() && flight.current(&context, &did, &read.id) {
                                validation = work.observe_read_at(
                                    &read.id,
                                    read.binding,
                                    &stamp,
                                    &reading.value,
                                    Observation {
                                        source: super::model::PathKind::Cloud,
                                        source_time: reading.update_time,
                                        received_at: context.clock.unix_now(),
                                    },
                                );
                                if validation.is_ok() {
                                    context.refresh_group_states(&did);
                                }
                            }
                            Ok(if flight.current(&context, &did, &read.id) {
                                match validation {
                                    Ok(()) => ReadResult::Observed,
                                    Err(error) => ReadResult::Failed(work.problem(
                                        "read",
                                        read.binding,
                                        &error,
                                    )),
                                }
                            } else {
                                ReadResult::Retired
                            })
                        }
                        Err(error) => {
                            context.cloud_error(&credentials.access_token, Some(&did), &error)?;
                            Ok(ReadResult::Failed(format!(
                                "{did} read {}.{}: {error}",
                                read.binding.siid, read.binding.piid
                            )))
                        }
                    }
                }
            }
        };
        let result: Result<ReadResult, AdapterError> = future::or(
            async {
                work.life.ended().await;
                Ok(ReadResult::Retired)
            },
            future::or(
                async {
                    flight.life.ended().await;
                    Ok(ReadResult::Retired)
                },
                reading,
            ),
        )
        .await;
        cleanup.retry = false;
        let mut read = read;
        retain_retry(&mut read, &flight);
        cleanup.read.waiters.clear();
        if !work.life.is_closed() && work.contains(&read.id, read.binding) {
            let result = match result {
                Ok(result) => result,
                Err(error) => {
                    finish_waiters(&mut read, &flight, Some(error.to_string()));
                    return Err(error);
                }
            };
            // A stale failure cannot consume demand queued after that read began.
            let current = !matches!(result, ReadResult::Failed(_))
                || !ordered
                || work
                    .observations
                    .read_is_current(&work, &read.id, read.binding, &stamp);
            let problem = match &result {
                ReadResult::Observed
                    if work.observations.read_after(&read.id, read.binding, &stamp) =>
                {
                    None
                }
                ReadResult::Observed | ReadResult::Retired => {
                    Some("read retired or superseded".into())
                }
                ReadResult::Failed(problem) => Some(problem.clone()),
            };
            finish_waiters(&mut read, &flight, problem);
            if current
                && read.id == "battery"
                && work
                    .native
                    .device()
                    .battery()
                    .is_some_and(|battery| read.binding == battery.property())
            {
                match &result {
                    ReadResult::Observed => work.set_battery_read_problem(None),
                    ReadResult::Failed(problem) => {
                        work.set_battery_read_problem(Some(problem.clone()))
                    }
                    ReadResult::Retired => {}
                }
            }
            if current {
                work.refresh
                    .borrow_mut()
                    .complete(read, result, context.clock.now());
            }
            work.report(&context, &did);
        } else {
            finish_waiters(&mut read, &flight, Some("read source retired".into()));
        }
        work.refresh_changed.notify(usize::MAX);
        Ok(())
    }
}

#[cfg(test)]
mod tests;
