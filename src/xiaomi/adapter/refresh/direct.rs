use super::super::observations::Observation;
use super::*;
use futures_lite::future;

const DIRECT_READ_LIMIT: usize = 8;

pub(in crate::xiaomi::adapter) async fn direct(context: Rc<Context>) -> Result<(), AdapterError> {
    let mut next_round = BTreeMap::<String, Duration>::new();
    let mut last_did: Option<String> = None;
    loop {
        let listener = context.refresh_changed.listen();
        let changed = context.changed.listen();
        retire(&context);
        let mut candidates = works(&context);
        if let Some(last) = &last_did {
            let next = candidates
                .iter()
                .position(|work| work.key.id > *last)
                .unwrap_or(0);
            candidates.rotate_left(next);
        }
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
        for kind in [ReadKind::Foreground, ReadKind::Periodic] {
            for work in candidates.iter().cloned() {
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
                let due = refresh.next_due(kind, |id| {
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
                if let Some((read, flight)) = reserve(&work, &context, ReadQueue::Direct, kind) {
                    *next = now + ROUND_DELAY;
                    in_flight += 1;
                    last_did = Some(work.key.id.clone());
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
                        context.tasks.spawn(read_legacy(
                            work,
                            reads,
                            flight,
                            mapping,
                            context.clone(),
                        ));
                    } else {
                        context
                            .tasks
                            .spawn(read_one(work, read, flight, context.clone()));
                    }
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

fn read_legacy(
    work: Rc<DeviceWork>,
    reads: Vec<Refresh>,
    flight: Rc<ReadFlight>,
    mapping: &'static crate::xiaomi::legacy_miio::Mapping,
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
        let ControlPath::Direct(client) = &flight.path else {
            unreachable!()
        };
        let eligible = |read: &Refresh, stamp: &super::super::observations::ReadStamp| {
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
                        super::super::legacy_miio::read(mapping, client, || {
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
        for cleanup in &mut cleanups {
            cleanup.retry = false;
        }
        if let Some(Err(error)) = &result
            && !(error.stage == SendStage::BeforeSend && error.failure == Failure::Withdrawn)
        {
            for (read, stamp) in reads.iter().zip(&stamps) {
                resolve_initial_failure(&context, &work, read, &flight, stamp);
            }
            context.lan_business_failure(did, client, error)?;
        }
        for (read, stamp) in reads.into_iter().zip(stamps) {
            if work.life.is_closed() || !work.contains(&read.id, read.binding) {
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
                            source: super::super::model::PathKind::Direct,
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
