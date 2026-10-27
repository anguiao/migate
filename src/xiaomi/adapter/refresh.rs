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
    collections::{BTreeMap, BTreeSet, VecDeque},
    rc::Rc,
    time::Duration,
};

const ROUND_DELAY: Duration = Duration::from_millis(200);
const RETRY_DELAY: Duration = Duration::from_secs(3);

#[derive(Clone)]
pub(super) struct Refresh {
    pub(super) id: String,
    pub(super) binding: PropertyBinding,
    pub(super) attempts: u8,
    pub(super) ready_at: Duration,
    // Reserved reads and cleanup leases retain the notification baseline generation.
    baseline: Option<u64>,
}
enum ReadResult {
    Observed,
    Retired,
    Failed(String),
}

#[derive(Default)]
pub(super) struct RefreshState {
    pending: VecDeque<Refresh>,
    inflight: BTreeMap<PropertyKey, Rc<ReadFlight>>,
    dirty_again: BTreeSet<PropertyKey>,
    problems: BTreeMap<PropertyKey, String>,
    baseline_generations: BTreeMap<String, u64>,
}
impl RefreshState {
    pub(super) fn request(&mut self, id: &str, binding: PropertyBinding) {
        self.queue(id, binding, false);
    }
    pub(super) fn request_baseline(&mut self, id: &str, binding: PropertyBinding) {
        self.queue(id, binding, true);
    }
    fn queue(&mut self, id: &str, binding: PropertyBinding, baseline: bool) {
        let baseline = baseline.then(|| *self.baseline_generations.entry(id.into()).or_default());
        if let Some(pending) = self
            .pending
            .iter_mut()
            .find(|read| read.id == id && read.binding == binding)
        {
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
            });
        }
    }
    pub(super) fn satisfy_baseline(&mut self, id: &str) {
        let generation = self.baseline_generations.entry(id.into()).or_default();
        *generation = generation.wrapping_add(1);
        self.pending
            .retain(|read| read.id != id || read.baseline.is_none());
    }
    fn required(&self, read: &Refresh) -> bool {
        read.baseline
            .is_none_or(|generation| self.baseline_generations[&read.id] == generation)
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
        local: bool,
    ) -> Option<(Refresh, Rc<ReadFlight>)> {
        let (index, path) = self.pending.iter().enumerate().find_map(|(index, read)| {
            if read.ready_at > now || self.inflight.contains_key(&read.key()) {
                return None;
            }
            let path = path_for(&read.id)?;
            (matches!(path, ControlPath::Local(_)) == local).then_some((index, path))
        })?;
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
    fn complete(
        &mut self,
        mut read: Refresh,
        result: ReadResult,
        now: Duration,
        cloud_first: bool,
    ) {
        if !self.required(&read) {
            return;
        }
        match result {
            ReadResult::Observed => {
                self.problems.remove(&read.key());
            }
            ReadResult::Retired => self.requeue(read),
            ReadResult::Failed(problem) => {
                if cloud_first {
                    self.problems.insert(read.key(), problem.clone());
                }
                if read.attempts == 3 {
                    self.pending.retain(|queued| queued.key() != read.key());
                    self.problems.insert(read.key(), problem);
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
    local: bool,
) -> Option<(Refresh, Rc<ReadFlight>)> {
    if work.life.is_closed() {
        return None;
    }
    work.refresh.borrow_mut().reserve(
        context.clock.now(),
        |id| context.read_path_for(&work.key.id, id),
        local,
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
            reserve(&work, &context, true)
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
pub(super) async fn cloud(context: Rc<Context>) -> Result<(), AdapterError> {
    loop {
        let listener = context.refresh_changed.listen();
        let changed = context.changed.listen();
        let credentials = context.credentials_changed.listen();
        retire(&context);
        for work in works(&context) {
            while let Some((read, flight)) = reserve(&work, &context, false) {
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
        let mut state = self.work.refresh.borrow_mut();
        if state
            .inflight
            .get(&self.read.key())
            .is_some_and(|flight| Rc::ptr_eq(flight, &self.flight))
        {
            state.inflight.remove(&self.read.key());
        }
        let dirty = state.finish_dirty(&self.read.id, self.read.binding);
        if self.retry && !self.work.life.is_closed() {
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
                    && (function.lighting().is_some() || function.kind().is_read_only())
            });
        let stamp = Rc::new(work.observations.read_stamp(&read.id, read.binding));
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
        if !work.life.is_closed() && work.contains(&read.id, read.binding) {
            let result = result?;
            // A stale failure cannot consume demand queued after that read began.
            let current = !matches!(result, ReadResult::Failed(_))
                || !ordered
                || work
                    .observations
                    .read_is_current(&work, &read.id, read.binding, &stamp);
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
                work.refresh.borrow_mut().complete(
                    read,
                    result,
                    context.clock.now(),
                    context.priority() == super::routing::PathPriority::CloudFirst,
                );
            }
            work.report(&context, &did);
        }
        work.refresh_changed.notify(usize::MAX);
        Ok(())
    }
}

#[cfg(test)]
mod tests;
