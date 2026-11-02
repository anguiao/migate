use super::super::observations::Observation;
use super::*;
use crate::xiaomi::central::{CentralError, RequestStage, SendDecision, SendHooks};
use futures_lite::future;

pub(super) fn read_one(
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
        let ordered = ordered_read(&work, &read);
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
                    let result_read = read.clone();
                    let periodic = read.periodic;
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
                                if let Err(error) = &outcome.result
                                    && !(outcome.stage == RequestStage::NotSent
                                        && matches!(
                                            error,
                                            CentralError::Withdrawn | CentralError::Cancelled
                                        ))
                                {
                                    resolve_initial_failure(
                                        &result_context,
                                        &observing,
                                        &result_read,
                                        &result_flight,
                                        &observed_stamp,
                                    );
                                }
                                if !result_flight.life.is_closed() {
                                    if periodic {
                                        result_context.local_background_property_outcome(
                                            &result_handle,
                                            &observing,
                                            &outcome,
                                        );
                                    } else {
                                        result_context.local_device_outcome(
                                            &result_handle,
                                            &observing,
                                            &outcome,
                                        );
                                    }
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
                                            source: super::super::model::PathKind::Local,
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
                ControlPath::Direct(client) => {
                    let result =
                        super::super::lan::read_property(client, &did, read.binding, || {
                            !flight.life.is_closed()
                                && !work.life.is_closed()
                                && flight.current(&context, &did, &read.id)
                                && work.refresh.borrow().required(&read)
                                && work.observations.read_is_current(
                                    &work,
                                    &read.id,
                                    read.binding,
                                    &stamp,
                                )
                        })
                        .await;
                    match result {
                        Ok(reading) => {
                            if work.life.is_closed() || !flight.current(&context, &did, &read.id) {
                                Ok(ReadResult::Retired)
                            } else {
                                match work.observe_read_at(
                                    &read.id,
                                    read.binding,
                                    &stamp,
                                    &reading.value,
                                    Observation {
                                        source: super::super::model::PathKind::Direct,
                                        source_time: reading.update_time,
                                        received_at: context.clock.unix_now(),
                                    },
                                ) {
                                    Ok(()) => {
                                        context.refresh_group_states(&did);
                                        Ok(ReadResult::Observed)
                                    }
                                    Err(error) => Ok(ReadResult::Failed(work.problem(
                                        "read",
                                        read.binding,
                                        &error,
                                    ))),
                                }
                            }
                        }
                        Err(error)
                            if error.stage == crate::xiaomi::lan::SendStage::BeforeSend
                                && error.failure == crate::xiaomi::lan::Failure::Withdrawn =>
                        {
                            Ok(ReadResult::Retired)
                        }
                        Err(error) => {
                            resolve_initial_failure(&context, &work, &read, &flight, &stamp);
                            context.lan_business_failure(&did, client, &error)?;
                            Ok(ReadResult::Failed(work.problem(
                                "read",
                                read.binding,
                                &error,
                            )))
                        }
                    }
                }
                ControlPath::Cloud(_) => unreachable!("cloud reads use the batch scheduler"),
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
            if current {
                record_battery_read(&work, &read, &result);
                work.refresh
                    .borrow_mut()
                    .complete(read, result, context.clock.now());
            }
            work.report(&context, &did);
        }
        work.refresh_changed.notify(usize::MAX);
        Ok(())
    }
}
