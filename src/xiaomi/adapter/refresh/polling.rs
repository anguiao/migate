use super::*;
use futures_lite::future;
use std::hash::{DefaultHasher, Hash, Hasher};

const CURTAIN_POLL_INTERVAL: Duration = Duration::from_secs(10);
const FAN_POLL_INTERVAL: Duration = Duration::from_secs(16);
const POLL_INTERVAL: Duration = Duration::from_secs(60);

struct Poll {
    work: Rc<DeviceWork>,
    due: Duration,
}

type PollKey = (String, String, Duration);

fn schedule_polls(
    context: &Context,
    candidates: &[Rc<DeviceWork>],
    polls: &mut BTreeMap<PollKey, Poll>,
    now: Duration,
) -> Option<Duration> {
    let mut eligible = BTreeSet::new();
    for work in candidates {
        let did = &work.key.id;
        if work.life.is_closed() || context.is_virtual_group(did) {
            continue;
        }
        let mut groups = BTreeMap::<(String, Duration), Vec<PropertyBinding>>::new();
        for function in work.native.device().functions() {
            if context.read_path_for(did, function.id()).is_none() {
                continue;
            }
            for binding in function.observation_bindings() {
                let interval = if function.curtain().is_some() {
                    CURTAIN_POLL_INTERVAL
                } else if let Some(fan) = function.fan()
                    && (binding == fan.power()
                        || binding == fan.speed().property()
                        || fan.mode().is_some_and(|mode| binding == mode.property()))
                {
                    FAN_POLL_INTERVAL
                } else {
                    POLL_INTERVAL
                };
                groups
                    .entry((function.id().to_owned(), interval))
                    .or_default()
                    .push(binding);
            }
        }
        if let Some(battery) = work.native.device().battery()
            && context.read_path_for(did, "battery").is_some()
        {
            groups
                .entry(("battery".into(), POLL_INTERVAL))
                .or_default()
                .push(battery.property());
        }
        for ((id, interval), bindings) in groups {
            let key = (did.clone(), id.clone(), interval);
            eligible.insert(key.clone());
            let poll = polls.entry(key).or_insert_with(|| Poll {
                work: work.clone(),
                due: now + poll_phase(did, interval),
            });
            if !Rc::ptr_eq(&poll.work, work) {
                *poll = Poll {
                    work: work.clone(),
                    due: now + poll_phase(did, interval),
                };
            }
            if now >= poll.due {
                for binding in bindings {
                    if work.refresh.borrow_mut().request_poll(&id, binding) {
                        work.observations.refresh(&id, binding);
                        work.refresh_changed.notify(usize::MAX);
                    }
                }
                poll.due = now + interval;
            }
        }
    }
    polls.retain(|key, _| eligible.contains(key));
    for work in candidates {
        work.refresh.borrow_mut().pending.retain(|read| {
            !read.periodic
                || !work.life.is_closed()
                    && !context.is_virtual_group(&work.key.id)
                    && work.contains(&read.id, read.binding)
                    && context.read_path_for(&work.key.id, &read.id).is_some()
        });
    }
    polls.values().map(|poll| poll.due).min()
}

fn poll_phase(did: &str, interval: Duration) -> Duration {
    let mut hash = DefaultHasher::new();
    did.hash(&mut hash);
    Duration::from_millis(1 + hash.finish() % interval.as_millis() as u64)
}

pub(in crate::xiaomi::adapter) async fn polling(context: Rc<Context>) -> Result<(), AdapterError> {
    let mut polls = BTreeMap::new();
    loop {
        let changed = context.changed.listen();
        let due = schedule_polls(&context, &works(&context), &mut polls, context.clock.now());
        if let Some(due) = due {
            future::or(changed, context.clock.wait_until(due)).await;
        } else {
            changed.await;
        }
    }
}
