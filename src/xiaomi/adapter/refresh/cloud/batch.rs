use super::super::super::{model::PathKind, observations::Observation, state::Backoff};
use super::super::{
    AdapterError, Context, ControlPath, DeviceWork, ReadCleanup, ReadFlight, ReadResult, Refresh,
    ordered_read, record_battery_read,
};
use super::CloudPoll;
use futures_lite::future;
use std::{cell::RefCell, rc::Rc};

pub(super) fn read_cloud_batch(
    work: Rc<DeviceWork>,
    reads: Vec<Refresh>,
    flight: Rc<ReadFlight>,
    context: Rc<Context>,
    poll: Option<(Rc<RefCell<CloudPoll>>, u64)>,
) -> impl std::future::Future<Output = Result<(), AdapterError>> {
    // Every property owns its demand, but the entire batch owns one flight.
    // The property leases exist before first poll.
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
        let mut cleanups = cleanups;
        let did = &work.key.id;
        let ControlPath::Cloud(credentials) = &flight.path else {
            unreachable!()
        };
        let stamps = reads
            .iter()
            .map(|read| work.observations.read_stamp(&read.id, read.binding))
            .collect::<Vec<_>>();
        let selected = reads
            .iter()
            .zip(&stamps)
            .enumerate()
            .filter_map(|(index, (read, stamp))| {
                (work.contains(&read.id, read.binding)
                    && work.refresh.borrow().required(read)
                    && work
                        .observations
                        .read_is_current(&work, &read.id, read.binding, stamp))
                .then_some(index)
            })
            .collect::<Vec<_>>();
        let bindings = selected
            .iter()
            .map(|index| reads[*index].binding)
            .collect::<Vec<_>>();
        let result = if bindings.is_empty()
            || flight.life.is_closed()
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
        let current = !flight.life.is_closed()
            && !work.life.is_closed()
            && flight.current(&context, did, &reads[0].id);
        if let Some((poll, generation)) = &poll {
            match &result {
                Some(Err(error)) if current && error.is_temporary() => {
                    let mut poll = poll.borrow_mut();
                    if poll.generation == *generation {
                        let delay = poll.backoff.failed();
                        poll.next = poll.next.max(context.clock.now() + delay);
                        poll.generation = poll.generation.wrapping_add(1);
                    }
                }
                Some(Ok(_)) if current => {
                    let mut poll = poll.borrow_mut();
                    // A success already in flight before a later failure
                    // cannot reset the failure's shared cooldown history.
                    if poll.generation == *generation {
                        poll.backoff = Backoff::new(30, 300);
                    }
                }
                _ => {}
            }
        }

        let mut results = vec![None; reads.len()];
        let batch_error = match result {
            None => false,
            Some(Ok(response)) => {
                if response.protocol_error {
                    log::warn!("Cloud property batch contained invalid or unrequested results");
                }
                if response.protocol_error && selected.len() == 1 {
                    // Reject a value from a malformed single-item response,
                    // but retain a correlated access failure for routing.
                    let result = response.items.into_iter().next().unwrap();
                    results[selected[0]] = Some(match result {
                        Ok(_) => Err(crate::xiaomi::cloud::CloudError::protocol(
                            "read Xiaomi property",
                        )),
                        Err(error) => Err(error),
                    });
                } else {
                    for (index, result) in selected.into_iter().zip(response.items) {
                        results[index] = Some(result);
                    }
                }
                false
            }
            Some(Err(error)) => {
                context.cloud_property_read_error(&credentials.access_token, &work, &error)?;
                for index in selected {
                    results[index] = Some(Err(error.clone()));
                }
                true
            }
        };
        for (index, ((read, stamp), result)) in
            reads.into_iter().zip(stamps).zip(results).enumerate()
        {
            if !batch_error && let Some(Err(error)) = &result {
                context.cloud_property_read_error(&credentials.access_token, &work, error)?;
            }
            if work.life.is_closed() || !work.contains(&read.id, read.binding) {
                cleanups[index].retry = false;
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
                        source: PathKind::Cloud,
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
                || !ordered_read(&work, &read)
                || work
                    .observations
                    .read_is_current(&work, &read.id, read.binding, &stamp);
            if current {
                record_battery_read(&work, &read, &outcome);
                work.refresh
                    .borrow_mut()
                    .complete(read, outcome, context.clock.now());
            }
            cleanups[index].retry = false;
        }
        if !work.life.is_closed() {
            work.report(&context, did);
        }
        Ok(())
    }
}
