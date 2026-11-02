use super::super::state::Backoff;
use super::*;
use futures_lite::future;
use std::cell::RefCell;

mod batch;
use batch::read_cloud_batch;

const CLOUD_READ_LIMIT: usize = 8;
const CLOUD_DEVICE_READ_LIMIT: usize = 2;

pub(in crate::xiaomi::adapter) async fn cloud(context: Rc<Context>) -> Result<(), AdapterError> {
    let poll = Rc::new(RefCell::new(CloudPoll::default()));
    let mut next_round = BTreeMap::<String, Duration>::new();
    let mut last_did: Option<String> = None;
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
                poll.generation = poll.generation.wrapping_add(1);
            }
        }

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
            .map(|work| cloud_flights(work).0)
            .sum::<usize>();
        let now = context.clock.now();

        // Ready foreground work wins over periodic work. One slow function
        // does not occupy the device's other slot or block other devices.
        for kind in [ReadKind::Foreground, ReadKind::Periodic] {
            for work in &candidates {
                if in_flight >= CLOUD_READ_LIMIT {
                    break;
                }
                let (device_flights, blocked) = cloud_flights(work);
                if device_flights >= CLOUD_DEVICE_READ_LIMIT {
                    continue;
                }
                let Some(due) = cloud_due(work, &context, kind, &blocked) else {
                    continue;
                };
                let mut due = due.max(*next_round.get(&work.key.id).unwrap_or(&Duration::ZERO));
                if matches!(kind, ReadKind::Periodic) {
                    due = due.max(poll.borrow().next);
                }
                if due > now {
                    continue;
                }
                let Some((reads, flight)) = reserve_cloud_batch(work, &context, kind, &blocked)
                else {
                    continue;
                };
                next_round.insert(work.key.id.clone(), now + ROUND_DELAY);
                last_did = Some(work.key.id.clone());
                in_flight += 1;
                let periodic = if matches!(kind, ReadKind::Periodic) {
                    let mut poll = poll.borrow_mut();
                    poll.next = now + ROUND_DELAY;
                    Some(poll.generation)
                } else {
                    None
                };
                context.tasks.spawn(read_cloud_batch(
                    work.clone(),
                    reads,
                    flight,
                    context.clone(),
                    periodic.map(|generation| (poll.clone(), generation)),
                ));
            }
        }

        // A newly started batch may leave another function ready for the
        // device's second slot. Wake at its own spacing deadline even if the
        // first request is still waiting for a response.
        let due = if in_flight >= CLOUD_READ_LIMIT {
            None
        } else {
            candidates
                .iter()
                .filter_map(|work| {
                    let (device_flights, blocked) = cloud_flights(work);
                    if device_flights >= CLOUD_DEVICE_READ_LIMIT {
                        return None;
                    }
                    let gate = *next_round.get(&work.key.id).unwrap_or(&Duration::ZERO);
                    let foreground = cloud_due(work, &context, ReadKind::Foreground, &blocked)
                        .map(|due| due.max(gate));
                    let poll = poll.borrow();
                    let periodic = cloud_due(work, &context, ReadKind::Periodic, &blocked)
                        .map(|due| due.max(gate).max(poll.next));
                    foreground.into_iter().chain(periodic).min()
                })
                .min()
        };
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

/// Count shared flights once, while keeping each function independent.
fn cloud_flights(work: &DeviceWork) -> (usize, BTreeSet<String>) {
    let state = work.refresh.borrow();
    let mut flights = Vec::<Rc<ReadFlight>>::new();
    let mut blocked = BTreeSet::new();
    for ((id, _, _), flight) in &state.inflight {
        if matches!(flight.path, ControlPath::Cloud(_)) {
            blocked.insert(id.clone());
            if !flights.iter().any(|other| Rc::ptr_eq(other, flight)) {
                flights.push(flight.clone());
            }
        }
    }
    (flights.len(), blocked)
}

fn cloud_due(
    work: &DeviceWork,
    context: &Context,
    kind: ReadKind,
    blocked: &BTreeSet<String>,
) -> Option<Duration> {
    work.refresh.borrow().next_due(kind, |id| {
        !blocked.contains(id)
            && matches!(
                context.read_path_for(&work.key.id, id),
                Some(ControlPath::Cloud(_))
            )
    })
}

fn reserve_cloud_batch(
    work: &Rc<DeviceWork>,
    context: &Context,
    kind: ReadKind,
    blocked: &BTreeSet<String>,
) -> Option<(Vec<Refresh>, Rc<ReadFlight>)> {
    if work.life.is_closed() {
        return None;
    }
    let now = context.clock.now();
    let mut state = work.refresh.borrow_mut();
    let (read, flight) = state.reserve(
        now,
        |id| {
            (!blocked.contains(id))
                .then(|| context.read_path_for(&work.key.id, id))
                .flatten()
        },
        ReadQueue::Cloud,
        kind,
    )?;
    let mut reads = vec![read];
    let mut index = 0;
    while index < state.pending.len() {
        let candidate = &state.pending[index];
        if candidate.periodic == reads[0].periodic
            && candidate.id == reads[0].id
            && candidate.ready_at <= now
            && !state.inflight.contains_key(&candidate.key())
        {
            let candidate = state.pending.remove(index).unwrap();
            state.inflight.insert(candidate.key(), flight.clone());
            reads.push(candidate);
        } else {
            index += 1;
        }
    }
    Some((reads, flight))
}

struct CloudPoll {
    next: Duration,
    backoff: Backoff,
    identity: Option<(String, String)>,
    generation: u64,
}
impl Default for CloudPoll {
    fn default() -> Self {
        Self {
            next: Duration::ZERO,
            backoff: Backoff::new(30, 300),
            identity: None,
            generation: 0,
        }
    }
}
