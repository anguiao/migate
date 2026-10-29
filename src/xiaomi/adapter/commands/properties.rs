//! Shared property transport and receipt settlement for device commands.
use super::super::{
    model::{AdapterError, PathKind},
    routing::ControlPath,
    state::{Context, LogicalSink},
    work::DeviceWork,
};
use super::{DeviceWriteGate, command_error};
use crate::{
    device::CommandError,
    xiaomi::{
        capability::PropertyBinding,
        central::{CentralError, RequestStage, SendDecision, SendHooks},
        cloud::{CloudError, CloudErrorKind},
    },
};
use serde_json::Value;
use std::{cell::RefCell, rc::Rc};

type AfterWrite =
    fn(&Context, &DeviceWork, &str, &[(PropertyBinding, Value)], &DeviceWriteGate, u64);

fn cloud_command_error(error: &CloudError) -> CommandError {
    match error.kind() {
        CloudErrorKind::Timeout => CommandError::Timeout,
        CloudErrorKind::Business(code) | CloudErrorKind::PropertyRejected(code) => {
            CommandError::Rejected(format!("cloud code {code}"))
        }
        _ => CommandError::SendFailed(error.to_string()),
    }
}
pub(super) fn alive(work: &DeviceWork, sink: &LogicalSink) -> bool {
    !work.life.is_closed() && !sink.life.is_closed()
}

/// One settled property batch: logical result plus a fallible cloud report.
pub(super) struct BatchReceipt {
    pub(super) result: Result<(), CommandError>,
    /// A fallible cloud maintenance report after the receipt already settled.
    pub(super) maintenance: Option<AdapterError>,
}

pub(super) enum BatchAttempt {
    /// No byte was written; the caller re-encodes and retries.
    Withdrawn,
    Settled(BatchReceipt),
}

fn settled(result: Result<(), CommandError>) -> BatchAttempt {
    BatchAttempt::Settled(BatchReceipt {
        result,
        maintenance: None,
    })
}

/// Concrete inputs for one attempt of one property batch.
pub(super) struct Batch<'a> {
    pub(super) function_id: &'a str,
    pub(super) properties: Vec<(PropertyBinding, Value)>,
    pub(super) retain_feedback: bool,
    pub(super) after_write: Option<AfterWrite>,
    pub(super) gate: &'a Rc<super::DeviceWriteGate>,
    pub(super) operation: &'a Rc<super::CommandOperation>,
    pub(super) mark: &'a Rc<dyn Fn() -> Result<(), CommandError>>,
    /// Ownership and the exact error once the request is superseded.
    pub(super) current: &'a Rc<dyn Fn() -> Result<(), CommandError>>,
    /// An unsent property plan withdraws when its re-encode no longer matches.
    pub(super) frame_current: Option<&'a Rc<dyn Fn() -> bool>>,
}

/// Submit one exact property batch over the device's current path. The sender
/// owns route selection, the final transmission guard, send marking, feedback
/// registration and receipt settlement.
/// Callers own encoding, ownership closures and the logical request result.
pub(super) async fn submit_properties(
    context: &Rc<Context>,
    work: &Rc<DeviceWork>,
    sink: &Rc<LogicalSink>,
    batch: Batch<'_>,
) -> BatchAttempt {
    let Batch {
        function_id,
        properties,
        retain_feedback,
        after_write,
        gate,
        operation,
        mark,
        current,
        frame_current,
    } = batch;
    let did = work.key.id.clone();
    let authority = gate.authority();
    if let Err(error) = current() {
        return settled(Err(error));
    }
    if !alive(work, sink) {
        return settled(Err(CommandError::Cancelled));
    }
    if frame_current.is_some_and(|frame| !frame()) {
        return BatchAttempt::Withdrawn;
    }
    let Some(path) = context.control_path(&did) else {
        return settled(Err(CommandError::Unavailable(
            "no usable control path".into(),
        )));
    };
    match path {
        ControlPath::Local(handle) => {
            let slot = Rc::new(RefCell::new(None::<BatchReceipt>));
            let guard_work = work.clone();
            let guard_sink = sink.clone();
            let guard_context = context.clone();
            let guard_handle = handle.clone();
            let guard_did = did.clone();
            let guard_properties = properties.clone();
            let guard_gate = gate.clone();
            let guard_operation = operation.clone();
            let guard_mark = mark.clone();
            let guard_current = current.clone();
            let guard_frame = frame_current.cloned();
            let result_slot = slot.clone();
            let result_context = context.clone();
            let result_handle = handle.clone();
            let result_work = work.clone();
            let result_gate = gate.clone();
            let result_properties = properties.clone();
            let result_id = function_id.to_owned();
            let result_current = current.clone();
            let hooks = SendHooks::new(
                move || {
                    if guard_work.life.is_closed()
                        || guard_sink.life.is_closed()
                        || !guard_context.current_local(&guard_handle)
                        || guard_current().is_err()
                    {
                        SendDecision::Cancel
                    } else if guard_frame.as_ref().is_some_and(|frame| !frame()) {
                        SendDecision::Withdraw
                    } else if matches!(
                        guard_context.control_path(&guard_did),
                        Some(ControlPath::Local(selected)) if selected.same_connection(&guard_handle)
                    ) {
                        SendDecision::Send
                    } else {
                        SendDecision::Withdraw
                    }
                },
                move || {
                    // The synchronous guard proved the request current; a
                    // failed mark can only mean the frame was retired.
                    let _ = guard_mark();
                    guard_operation.submitted(PathKind::Local);
                    if retain_feedback {
                        guard_gate.register_feedback_for(authority, &guard_properties);
                    }
                },
            );
            handle
                .set_properties_guarded(&did, &properties, hooks, move |outcome| {
                    if outcome.result == Err(CentralError::Withdrawn)
                        && outcome.stage == RequestStage::NotSent
                    {
                        return;
                    }
                    if let Some(after_write) = after_write
                        && outcome.stage != RequestStage::NotSent
                    {
                        after_write(
                            &result_context,
                            &result_work,
                            &result_id,
                            &result_properties,
                            &result_gate,
                            authority,
                        );
                    }
                    result_context.local_device_outcome(&result_handle, &result_work, &outcome);
                    let receipt = match outcome.result {
                        Ok(results) => {
                            result_gate.finish_feedback_for(
                                authority,
                                &result_properties,
                                &results,
                                retain_feedback,
                            );
                            for error in results.items.iter().filter_map(|item| item.as_ref().err())
                            {
                                result_context.local_device_outcome(
                                    &result_handle,
                                    &result_work,
                                    &crate::xiaomi::central::RequestOutcome::<()> {
                                        result: Err(error.clone()),
                                        stage: outcome.stage,
                                        timeout: outcome.timeout,
                                    },
                                );
                            }
                            BatchReceipt {
                                result: if results.protocol_error {
                                    Err(command_error(CentralError::Protocol))
                                } else if let Some(error) =
                                    results.items.iter().find_map(|item| item.as_ref().err())
                                {
                                    Err(command_error(error.clone()))
                                } else {
                                    Ok(())
                                },
                                maintenance: None,
                            }
                        }
                        Err(error) => {
                            result_gate.cancel_feedback();
                            let result = if outcome.stage == RequestStage::NotSent {
                                // A cancelled not-sent request may already be
                                // superseded; keep its actual error.
                                result_current()
                                    .err()
                                    .unwrap_or_else(|| command_error(error))
                            } else {
                                command_error(error)
                            };
                            BatchReceipt {
                                result: Err(result),
                                maintenance: None,
                            }
                        }
                    };
                    *result_slot.borrow_mut() = Some(receipt);
                })
                .await;
            match slot.borrow_mut().take() {
                Some(receipt) => BatchAttempt::Settled(receipt),
                None => BatchAttempt::Withdrawn,
            }
        }
        ControlPath::Cloud(credentials) => {
            if frame_current.is_some_and(|frame| !frame()) {
                return BatchAttempt::Withdrawn;
            }
            if let Err(error) = mark() {
                return settled(Err(error));
            }
            if retain_feedback {
                gate.register_feedback_for(authority, &properties);
            }
            operation.submitted(PathKind::Cloud);
            let result = context
                .cloud
                .set_properties(&credentials.access_token, &did, &properties)
                .await;
            if let Some(after_write) = after_write {
                after_write(context, work, function_id, &properties, gate, authority);
            }
            let receipt = match &result {
                Ok(results) => {
                    gate.finish_feedback_for(authority, &properties, results, retain_feedback);
                    let current = alive(work, sink);
                    let mut maintenance = None;
                    for error in results.items.iter().filter_map(|item| item.as_ref().err()) {
                        if maintenance.is_none() {
                            maintenance = context
                                .cloud_error(
                                    &credentials.access_token,
                                    current.then_some(did.as_str()),
                                    error,
                                )
                                .err();
                        }
                    }
                    let result = if results.protocol_error {
                        Err(CommandError::SendFailed("invalid cloud response".into()))
                    } else if let Some(error) =
                        results.items.iter().find_map(|item| item.as_ref().err())
                    {
                        Err(cloud_command_error(error))
                    } else {
                        Ok(())
                    };
                    BatchReceipt {
                        result,
                        maintenance,
                    }
                }
                Err(error) => {
                    gate.cancel_feedback();
                    let maintenance = context
                        .cloud_error(
                            &credentials.access_token,
                            alive(work, sink).then_some(did.as_str()),
                            error,
                        )
                        .err();
                    BatchReceipt {
                        result: Err(cloud_command_error(error)),
                        maintenance,
                    }
                }
            };
            BatchAttempt::Settled(receipt)
        }
    }
}
