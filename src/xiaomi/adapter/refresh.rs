use super::{
    model::AdapterError,
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
}
enum ReadResult {
    Observed,
    Retired,
    Failed(String),
}

#[derive(Default)]
pub(super) struct RefreshState {
    pending: VecDeque<Refresh>,
    inflight: BTreeMap<super::lighting::PropertyKey, Rc<ReadFlight>>,
    dirty_again: BTreeSet<super::lighting::PropertyKey>,
    problems: BTreeMap<super::lighting::PropertyKey, String>,
}
impl RefreshState {
    pub(super) fn request(&mut self, id: &str, binding: PropertyBinding) {
        if !self
            .pending
            .iter()
            .any(|read| read.id == id && read.binding == binding)
        {
            self.pending.push_back(Refresh {
                id: id.into(),
                binding,
                attempts: 0,
                ready_at: Duration::ZERO,
            });
        }
    }
    pub(super) fn contains(&self, id: &str, binding: PropertyBinding) -> bool {
        self.inflight
            .contains_key(&super::lighting::key(id, binding))
            || self
                .pending
                .iter()
                .any(|read| read.id == id && read.binding == binding)
    }
    pub(super) fn request_dirty(&mut self, id: &str, binding: PropertyBinding) -> bool {
        let key = super::lighting::key(id, binding);
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
        if self.dirty_again.remove(&super::lighting::key(id, binding)) {
            self.request(id, binding);
            true
        } else {
            false
        }
    }
    fn next_due(&self) -> Option<Duration> {
        self.pending
            .iter()
            .filter(|read| !self.inflight.contains_key(&read.key()))
            .map(|read| read.ready_at)
            .min()
    }
    fn reserve(&mut self, now: Duration, path: ControlPath) -> Option<(Refresh, Rc<ReadFlight>)> {
        let index = self
            .pending
            .iter()
            .position(|read| read.ready_at <= now && !self.inflight.contains_key(&read.key()))?;
        let read = self.pending.remove(index).unwrap();
        let flight = Rc::new(ReadFlight {
            life: Lifetime::default(),
            path,
        });
        self.inflight.insert(read.key(), flight.clone());
        Some((read, flight))
    }
    fn requeue(&mut self, read: Refresh) {
        if !self.pending.iter().any(|queued| queued.key() == read.key()) {
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
    pub(super) fn problems(&self) -> Vec<String> {
        self.problems.values().cloned().collect()
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
    pub(super) inflight: BTreeSet<super::lighting::PropertyKey>,
    pub(super) problems: BTreeMap<super::lighting::PropertyKey, String>,
}

pub(super) struct ReadFlight {
    life: Lifetime,
    path: ControlPath,
}
impl Refresh {
    fn key(&self) -> super::lighting::PropertyKey {
        super::lighting::key(&self.id, self.binding)
    }
}
impl ReadFlight {
    fn current(&self, context: &Context, did: &str) -> bool {
        match (&self.path, context.read_path(did)) {
            (ControlPath::Local(old), Some(ControlPath::Local(new))) => old.same_connection(&new),
            (ControlPath::Cloud(old), Some(ControlPath::Cloud(new))) => {
                old.uid == new.uid && old.access_token == new.access_token
            }
            _ => false,
        }
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
fn retire(context: &Context) {
    for work in works(context) {
        let did = &work.key.id;
        for flight in work.refresh.borrow().inflight.values() {
            if work.life.is_closed() || !flight.current(context, did) {
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
    let path = context.read_path(&work.key.id)?;
    if matches!(path, ControlPath::Local(_)) != local {
        return None;
    }
    work.refresh.borrow_mut().reserve(context.clock.now(), path)
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
            .filter_map(|work| work.refresh.borrow().next_due())
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
            .filter_map(|work| work.refresh.borrow().next_due())
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
        let lighting = work
            .native
            .device
            .functions()
            .iter()
            .any(|function| function.id() == read.id && function.lighting().is_some());
        let stamp = Rc::new(work.observations.read_stamp(&read.id, read.binding));
        let reading = async {
            if flight.life.is_closed() || !flight.current(&context, &did) {
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
                        .read_property_value_with_timeout_guarded(
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
                                    } else if !guard_context
                                        .local_reads_allowed(&guard_did, &guard_handle)
                                        || !guard_work.observations.read_is_current(
                                            &guard_work,
                                            &guard_id,
                                            read.binding,
                                            &guard_stamp,
                                        )
                                    {
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
                                    && result_flight.current(&result_context, &observing.key.id)
                                    && let Ok(value) = outcome.result
                                {
                                    *observed_validation.borrow_mut() = observing.observe_read(
                                        &observed_id,
                                        read.binding,
                                        &observed_stamp,
                                        &value,
                                    );
                                    if observed_validation.borrow().is_ok() {
                                        result_context.refresh_group_states(&observing.key.id);
                                    }
                                }
                            },
                        )
                        .await;
                    match outcome.result {
                        Ok(_) => Ok(if flight.current(&context, &did) {
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
                        .read_property_value(&credentials.access_token, &did, read.binding)
                        .await
                    {
                        Ok(value) => {
                            let mut validation = Ok(());
                            if !work.life.is_closed() && flight.current(&context, &did) {
                                validation =
                                    work.observe_read(&read.id, read.binding, &stamp, &value);
                                if validation.is_ok() {
                                    context.refresh_group_states(&did);
                                }
                            }
                            Ok(if flight.current(&context, &did) {
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
            if !matches!(result, ReadResult::Failed(_))
                || !lighting
                || work
                    .observations
                    .read_is_current(&work, &read.id, read.binding, &stamp)
            {
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
