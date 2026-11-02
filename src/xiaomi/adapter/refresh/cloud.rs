use super::super::{observations::Observation, state::Backoff};
use super::*;
use futures_lite::future;
use std::cell::RefCell;

pub(in crate::xiaomi::adapter) async fn cloud(context: Rc<Context>) -> Result<(), AdapterError> {
    let poll = Rc::new(RefCell::new(CloudPoll::default()));
    loop {
        let listener = context.refresh_changed.listen();
        let changed = context.changed.listen();
        let credentials = context.credentials_changed.listen();
        retire(&context);
        let identity = context
            .live
            .borrow()
            .credentials
            .as_ref()
            .map(|credentials| {
                (
                    credentials.uid.clone(),
                    credentials.tokens.access_token.clone(),
                )
            });
        {
            let mut poll = poll.borrow_mut();
            if poll.identity != identity {
                poll.identity = identity;
                poll.next = Duration::ZERO;
                poll.backoff = Backoff::new(30, 300);
            }
        }
        for work in works(&context) {
            while let Some((read, flight)) =
                reserve(&work, &context, ReadQueue::Cloud, ReadKind::Foreground)
            {
                context
                    .tasks
                    .spawn(read_one(work.clone(), read, flight, context.clone()));
            }
        }
        let ready = {
            let poll = poll.borrow();
            !poll.running && context.clock.now() >= poll.next
        };
        if ready {
            let mut candidates = works(&context);
            if let Some(last) = &poll.borrow().last_did {
                let next = candidates
                    .iter()
                    .position(|work| work.key.id > *last)
                    .unwrap_or(0);
                candidates.rotate_left(next);
            }
            for work in candidates {
                let Some((read, flight)) =
                    reserve(&work, &context, ReadQueue::Cloud, ReadKind::Periodic)
                else {
                    continue;
                };
                let mut reads = vec![read];
                {
                    let mut state = work.refresh.borrow_mut();
                    let mut index = 0;
                    while index < state.pending.len() {
                        let candidate = &state.pending[index];
                        if candidate.periodic
                            && candidate.id == reads[0].id
                            && candidate.ready_at <= context.clock.now()
                            && !state.inflight.contains_key(&candidate.key())
                        {
                            let candidate = state.pending.remove(index).unwrap();
                            state.inflight.insert(candidate.key(), flight.clone());
                            reads.push(candidate);
                        } else {
                            index += 1;
                        }
                    }
                }
                {
                    let mut poll = poll.borrow_mut();
                    poll.running = true;
                    poll.next = context.clock.now() + ROUND_DELAY;
                    poll.last_did = Some(work.key.id.clone());
                }
                context.tasks.spawn(read_cloud_poll(
                    work,
                    reads,
                    flight,
                    context.clone(),
                    poll.clone(),
                ));
                break;
            }
        }
        let due_foreground = works(&context)
            .iter()
            .filter_map(|work| {
                work.refresh.borrow().next_due(ReadKind::Foreground, |id| {
                    matches!(
                        context.read_path_for(&work.key.id, id),
                        Some(ControlPath::Cloud(_))
                    )
                })
            })
            .min();
        let due_poll = if poll.borrow().running {
            None
        } else {
            works(&context)
                .iter()
                .filter_map(|work| {
                    work.refresh.borrow().next_due(ReadKind::Periodic, |id| {
                        matches!(
                            context.read_path_for(&work.key.id, id),
                            Some(ControlPath::Cloud(_))
                        )
                    })
                })
                .min()
                .map(|due| due.max(poll.borrow().next))
        };
        let due = due_foreground.into_iter().chain(due_poll).min();
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

struct CloudPoll {
    running: bool,
    next: Duration,
    backoff: Backoff,
    identity: Option<(String, String)>,
    last_did: Option<String>,
}
impl Default for CloudPoll {
    fn default() -> Self {
        Self {
            running: false,
            next: Duration::ZERO,
            backoff: Backoff::new(30, 300),
            identity: None,
            last_did: None,
        }
    }
}
struct CloudPollLease {
    poll: Rc<RefCell<CloudPoll>>,
    context: Rc<Context>,
}
impl Drop for CloudPollLease {
    fn drop(&mut self) {
        self.poll.borrow_mut().running = false;
        self.context.refresh_changed.notify(usize::MAX);
    }
}
fn read_cloud_poll(
    work: Rc<DeviceWork>,
    reads: Vec<Refresh>,
    flight: Rc<ReadFlight>,
    context: Rc<Context>,
    poll: Rc<RefCell<CloudPoll>>,
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
    // Construct leases before the future can be dropped without being polled.
    let lease = CloudPollLease {
        poll: poll.clone(),
        context: context.clone(),
    };
    async move {
        let _lease = lease;
        let mut cleanups = cleanups;
        let did = &work.key.id;
        let ControlPath::Cloud(credentials) = &flight.path else {
            unreachable!()
        };
        let stamps = reads
            .iter()
            .map(|read| work.observations.read_stamp(&read.id, read.binding))
            .collect::<Vec<_>>();
        let bindings = reads.iter().map(|read| read.binding).collect::<Vec<_>>();
        let result = if flight.life.is_closed()
            || work.life.is_closed()
            || !flight.current(&context, did, &reads[0].id)
        {
            None
        } else {
            future::or(
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
                            context
                                .cloud
                                .read_properties(&credentials.access_token, did, &bindings)
                                .await,
                        )
                    },
                ),
            )
            .await
        };
        for cleanup in &mut cleanups {
            cleanup.retry = false;
        }
        let current = !flight.life.is_closed()
            && !work.life.is_closed()
            && flight.current(&context, did, &reads[0].id);
        match &result {
            Some(Err(error)) if current && error.is_temporary() => {
                let mut poll = poll.borrow_mut();
                let delay = poll.backoff.failed();
                poll.next = poll.next.max(context.clock.now() + delay);
            }
            Some(Ok(_)) if current => poll.borrow_mut().backoff = Backoff::new(30, 300),
            _ => {}
        }
        let (results, batch_error) = match result {
            None => (vec![None; reads.len()], false),
            Some(Ok(results)) => (results.items.into_iter().map(Some).collect(), false),
            Some(Err(error)) => {
                context.cloud_background_property_error(
                    &credentials.access_token,
                    &work,
                    &error,
                )?;
                (vec![Some(Err(error)); reads.len()], true)
            }
        };
        for ((read, stamp), result) in reads.into_iter().zip(stamps).zip(results) {
            if !batch_error && let Some(Err(error)) = &result {
                context.cloud_background_property_error(&credentials.access_token, &work, error)?;
            }
            if work.life.is_closed() || !work.contains(&read.id, read.binding) {
                continue;
            }
            let outcome = match result {
                None => ReadResult::Retired,
                Some(_) if flight.life.is_closed() || !flight.current(&context, did, &read.id) => {
                    ReadResult::Retired
                }
                Some(Ok(reading)) => match work.observe_read_at(
                    &read.id,
                    read.binding,
                    &stamp,
                    &reading.value,
                    Observation {
                        source: super::super::model::PathKind::Cloud,
                        source_time: reading.update_time,
                        received_at: context.clock.unix_now(),
                    },
                ) {
                    Ok(()) => {
                        context.refresh_group_states(did);
                        ReadResult::Observed
                    }
                    Err(error) => ReadResult::Failed(work.problem("read", read.binding, &error)),
                },
                Some(Err(error)) => ReadResult::Failed(work.problem("read", read.binding, &error)),
            };
            let current = !matches!(outcome, ReadResult::Failed(_))
                || work
                    .observations
                    .read_is_current(&work, &read.id, read.binding, &stamp);
            if current {
                work.refresh
                    .borrow_mut()
                    .complete(read, outcome, context.clock.now());
            }
        }
        if !work.life.is_closed() {
            work.report(&context, did);
        }
        Ok(())
    }
}
