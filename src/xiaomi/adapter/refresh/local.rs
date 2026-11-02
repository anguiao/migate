use super::*;
use crate::xiaomi::central::SessionHandle;
use futures_lite::future;
use futures_util::future::join_all;

const LOCAL_READ_LIMIT: usize = 8;

pub(in crate::xiaomi::adapter) async fn local(
    context: Rc<Context>,
    handle: SessionHandle,
) -> Result<(), AdapterError> {
    let mut next_round = None;
    let mut last_did: Option<String> = None;
    loop {
        let listener = context.refresh_changed.listen();
        retire(&context);
        let candidates = works(&context);
        let ready = candidates
            .iter()
            .filter_map(|work| {
                work.refresh.borrow().next_due(ReadKind::Any, |id| {
                    context.local_function_reads_allowed(&work.key.id, id, &handle)
                })
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
        let mut candidates = works(&context);
        if let Some(last) = &last_did {
            let next = candidates
                .iter()
                .position(|work| work.key.id > *last)
                .unwrap_or(0);
            candidates.rotate_left(next);
        }
        let mut last = None;
        let mut selected = BTreeSet::new();
        let mut round = Vec::new();
        for kind in [ReadKind::Foreground, ReadKind::Periodic] {
            for work in &candidates {
                if round.len() == LOCAL_READ_LIMIT || !context.current_local(&handle) {
                    break;
                }
                if selected.contains(&work.key.id) {
                    continue;
                }
                if let Some((read, flight)) = reserve(work, &context, ReadQueue::Local, kind) {
                    last = Some(work.key.id.clone());
                    selected.insert(work.key.id.clone());
                    round.push(read_one(work.clone(), read, flight, context.clone()));
                }
            }
        }
        if last.is_some() {
            last_did = last;
        }
        // Keep one property per DID and a fair, bounded shared gateway round.
        join_all(round)
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()?;
        next_round = Some(context.clock.now() + ROUND_DELAY);
    }
}
