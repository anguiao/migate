use super::super::super::{model::PathKind, observations::Observation, state::Backoff};
use super::super::{
    AdapterError, Context, ControlPath, DeviceWork, ReadCleanup, ReadFlight, ReadResult, Refresh,
    ordered_read, record_battery_read, resolve_initial_failure,
};
use super::CloudPoll;
use crate::xiaomi::{
    capability::ReadInput,
    cloud::CloudError,
    property::{PropertyReading, PropertyResults},
};
use futures_lite::future;
use std::{cell::RefCell, rc::Rc};

enum CloudRead {
    Properties(PropertyResults<PropertyReading, CloudError>),
    Shared(serde_json::Value),
}

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
            .map(|read| work.observations.read_stamp_input(&read.id, read.input))
            .collect::<Vec<_>>();
        let selected = reads
            .iter()
            .zip(&stamps)
            .enumerate()
            .filter_map(|(index, (read, stamp))| {
                (work.contains_input(&read.id, read.input)
                    && work.refresh.borrow().required(read)
                    && work
                        .observations
                        .read_is_current_input(&work, &read.id, read.input, stamp))
                .then_some(index)
            })
            .collect::<Vec<_>>();
        let shared = selected
            .first()
            .is_some_and(|index| reads[*index].input == ReadInput::SharedFeedback);
        let bindings = selected
            .iter()
            .filter_map(|index| reads[*index].input.property())
            .collect::<Vec<_>>();
        let result = if selected.is_empty()
            || flight.life.is_closed()
            || work.life.is_closed()
            || !flight.current(&context, did, &reads[0].id, reads[0].input)
            || !context.current_mijia(credentials)
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
                        Some(if shared {
                            context
                                .mijia
                                .read_legacy_group(&credentials.session, did)
                                .await
                                .map(CloudRead::Shared)
                        } else {
                            context
                                .mijia
                                .read_properties(&credentials.session, did, &bindings)
                                .await
                                .map(CloudRead::Properties)
                        })
                    },
                ),
            )
            .await
        };
        let current = !flight.life.is_closed()
            && !work.life.is_closed()
            && flight.current(&context, did, &reads[0].id, reads[0].input)
            && context.current_mijia(credentials);
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

        if let Some(Ok(CloudRead::Shared(raw))) = &result {
            let read = &reads[0];
            let stamp = &stamps[0];
            if work.life.is_closed()
                || flight.life.is_closed()
                || !current
                || !work.contains_input(&read.id, read.input)
                || !flight.current(&context, did, &read.id, read.input)
            {
                return Ok(());
            }
            let observed = work.observe_shared_feedback(
                raw,
                stamp,
                Observation {
                    source: PathKind::Cloud,
                    source_time: None,
                    received_at: context.clock.unix_now(),
                },
            );
            work.refresh
                .borrow_mut()
                .complete(read.clone(), observed, context.clock.now());
            for cleanup in &mut cleanups {
                cleanup.retry = false;
            }
            context.refresh_group_states(did);
            work.report(&context, did);
            work.refresh_changed.notify(usize::MAX);
            return Ok(());
        }
        let result = match result {
            Some(Ok(CloudRead::Properties(response))) => Some(Ok(response)),
            Some(Ok(CloudRead::Shared(_))) => None,
            Some(Err(error)) => Some(Err(error)),
            None => None,
        };
        let mut results = vec![None; reads.len()];
        let batch_error = match result {
            None => false,
            Some(Ok(response)) => {
                if response.protocol_error {
                    log::warn!("Cloud property batch contained invalid or unrequested results");
                }
                for (index, result) in selected.iter().copied().zip(&response.items) {
                    if result.is_err() || response.protocol_error && selected.len() == 1 {
                        resolve_initial_failure(
                            &context,
                            &work,
                            &reads[index],
                            &flight,
                            &stamps[index],
                        );
                    }
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
                for index in selected.iter().copied() {
                    resolve_initial_failure(
                        &context,
                        &work,
                        &reads[index],
                        &flight,
                        &stamps[index],
                    );
                }
                context.cloud_property_read_error(credentials, &work, &error);
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
                context.cloud_property_read_error(credentials, &work, error);
            }
            if work.life.is_closed() || !work.contains_input(&read.id, read.input) {
                cleanups[index].retry = false;
                continue;
            }
            let outcome = match result {
                None => ReadResult::Retired,
                Some(_)
                    if !current
                        || flight.life.is_closed()
                        || !flight.current(&context, did, &read.id, read.input) =>
                {
                    ReadResult::Retired
                }
                Some(Ok(reading)) => match work.observe_read_at(
                    &read.id,
                    read.input.property().unwrap(),
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
                    Err(error) => {
                        ReadResult::Failed(work.problem_input("read", read.input, &error))
                    }
                },
                Some(Err(error)) => {
                    ReadResult::Failed(work.problem_input("read", read.input, &error))
                }
            };
            let current = !matches!(outcome, ReadResult::Failed(_))
                || !ordered_read(&work, &read)
                || work
                    .observations
                    .read_is_current_input(&work, &read.id, read.input, &stamp);
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
