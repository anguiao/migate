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
        let ready = {
            let candidates = works(&context);
            let routes = read_routes(&context, &candidates);
            retire(&candidates, &routes);
            candidates
                .iter()
                .filter_map(|work| {
                    let routes = routes.get(&work.key.id)?;
                    work.refresh.borrow().next_due(ReadKind::Any, |id, input| {
                        matches!(routes.read_path_for_input(id, input), Some(ControlPath::Local(selected)) if selected.same_connection(&handle))
                    })
                })
                .min()
        };
        let Some(ready) = ready else {
            next_round = None;
            listener.await;
            continue;
        };
        let next = context.clock.now() + ROUND_DELAY;
        let deadline = next_round
            .map_or(next, |scheduled: Duration| scheduled.min(next))
            .max(ready);
        next_round = Some(deadline);
        future::or(context.clock.wait_until(deadline), async {
            listener.await;
        })
        .await;
        let now = context.clock.now();
        if now < deadline {
            continue;
        }
        let mut candidates = works(&context);
        let routes = read_routes(&context, &candidates);
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
        let current_local = context.current_local(&handle);
        for kind in [ReadKind::Foreground, ReadKind::Periodic] {
            for work in &candidates {
                if round.len() == LOCAL_READ_LIMIT || !current_local {
                    break;
                }
                if selected.contains(&work.key.id) {
                    continue;
                }
                let Some(routes) = routes.get(&work.key.id) else {
                    continue;
                };
                if let Some((read, flight)) = reserve(work, routes, now, ReadQueue::Local, kind) {
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
