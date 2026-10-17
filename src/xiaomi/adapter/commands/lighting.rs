use super::super::{
    model::{AdapterError, CommandTarget, PathKind},
    routing::ControlPath,
    state::{Context, DeviceRecord},
    work::DeviceWork,
};
use super::command_error;
use crate::{
    device::{CommandError, DeviceRequest, LightColorTarget, LightTarget},
    xiaomi::{
        capability::PropertyBinding,
        central::{CentralError, RequestStage, SendDecision, SendHooks},
        cloud::{CloudError, CloudErrorKind},
        property::PropertyResults,
    },
};
use event_listener::Event;
use serde_json::Value;

mod repair;
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

fn current(request: &DeviceRequest) -> bool {
    current_result(request).is_ok()
}
fn current_result(request: &DeviceRequest) -> Result<(), CommandError> {
    match request {
        DeviceRequest::Power(request) => request.current_result(),
        DeviceRequest::Light(request) => request.current_result(),
    }
}
fn mark_sent(request: &DeviceRequest) -> Result<(), CommandError> {
    match request {
        DeviceRequest::Power(request) => request.mark_sent(),
        DeviceRequest::Light(request) => request.mark_sent(),
    }
}
fn reject(request: DeviceRequest, error: CommandError) {
    match request {
        DeviceRequest::Power(request) => request.reject(error),
        DeviceRequest::Light(request) => request.reject(error),
    }
}
fn accept(request: DeviceRequest) -> Result<(), CommandError> {
    match request {
        DeviceRequest::Power(request) => request.accept_preserving_state().map(|_| ()),
        DeviceRequest::Light(request) => request.accept().map(|_| ()),
    }
}
fn skip(request: DeviceRequest) -> Result<(), CommandError> {
    match request {
        DeviceRequest::Power(request) => request.accept_preserving_state().map(|_| ()),
        DeviceRequest::Light(request) => request.skip_unchanged().map(|_| ()),
    }
}
fn cloud_command_error(error: &CloudError) -> CommandError {
    match error.kind() {
        CloudErrorKind::Timeout => CommandError::Timeout,
        CloudErrorKind::Business(code) | CloudErrorKind::PropertyRejected(code) => {
            CommandError::Rejected(format!("cloud code {code}"))
        }
        _ => CommandError::SendFailed(error.to_string()),
    }
}
fn alive(work: &DeviceWork, generation: u64) -> bool {
    !work.life.is_closed()
        && !work.record.life.is_closed()
        && work.record.binding_generation.get() == generation
}

/// One settled property batch: logical result plus a fallible cloud report.
struct BatchReceipt {
    result: Result<(), CommandError>,
    /// A fallible cloud maintenance report after the receipt already settled.
    maintenance: Option<AdapterError>,
}

enum BatchAttempt {
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
struct Batch<'a> {
    function_id: &'a str,
    properties: Vec<(PropertyBinding, Value)>,
    retain_feedback: bool,
    gate: &'a Rc<super::LightWriteGate>,
    operation: &'a Rc<super::CommandOperation>,
    mark: &'a Rc<dyn Fn() -> Result<(), CommandError>>,
    /// Ownership and the exact error once the request is superseded.
    current: &'a Rc<dyn Fn() -> Result<(), CommandError>>,
    /// Single-light frames withdraw when their re-encode no longer matches.
    frame_current: Option<&'a Rc<dyn Fn() -> bool>>,
}

fn settle_receipt<E>(
    work: &DeviceWork,
    id: &str,
    properties: &[(PropertyBinding, Value)],
    results: &PropertyResults<(), E>,
    gate: &Rc<super::LightWriteGate>,
    authority: u64,
    retain_feedback: bool,
) {
    gate.finish_feedback_for(authority, properties, results, retain_feedback);
    for (binding, _) in properties {
        work.request_dirty_read(id, *binding);
    }
}

/// Submit one exact property batch over the device's current path. The sender
/// owns route selection, the final transmission guard, send marking, feedback
/// registration and receipt settlement.
/// Callers own encoding, ownership closures and the logical request result.
async fn submit_properties(
    context: &Rc<Context>,
    work: &Rc<DeviceWork>,
    batch: Batch<'_>,
) -> BatchAttempt {
    let Batch {
        function_id,
        properties,
        retain_feedback,
        gate,
        operation,
        mark,
        current,
        frame_current,
    } = batch;
    let did = work.record.physical.key().id.clone();
    let generation = work.record.binding_generation.get();
    let authority = gate.authority();
    if let Err(error) = current() {
        return settled(Err(error));
    }
    if !alive(work, generation) {
        return settled(Err(CommandError::Cancelled));
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
                        || guard_work.record.life.is_closed()
                        || guard_work.record.binding_generation.get() != generation
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
                    result_context.local_device_outcome(
                        &result_handle,
                        &result_work,
                        generation,
                        &outcome,
                    );
                    let receipt = match outcome.result {
                        Ok(results) => {
                            settle_receipt(
                                &result_work,
                                &result_id,
                                &result_properties,
                                &results,
                                &result_gate,
                                authority,
                                retain_feedback,
                            );
                            for error in results.items.iter().filter_map(|item| item.as_ref().err())
                            {
                                result_context.local_device_outcome(
                                    &result_handle,
                                    &result_work,
                                    generation,
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
            let receipt = match &result {
                Ok(results) => {
                    settle_receipt(
                        work,
                        function_id,
                        &properties,
                        results,
                        gate,
                        authority,
                        retain_feedback,
                    );
                    let current = alive(work, generation);
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
                            alive(work, generation).then_some(did.as_str()),
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

/// Receipt ownership outlives the consumer and the immutable function instance.
pub(super) async fn send_owned(
    request: DeviceRequest,
    context: Rc<Context>,
    record: Rc<DeviceRecord>,
    id: &str,
) -> Result<(), AdapterError> {
    let completion = Rc::new((Cell::new(false), Event::new()));
    let complete = completion.clone();
    let owner = context.clone();
    let id = id.to_owned();
    let gate = context
        .light_writes
        .for_device(&record.physical.key().id, context.clock.clone());
    context.tasks.spawn(async move {
        let _permit = gate.acquire().await;
        let result = send(request, owner, record, &id, gate).await;
        complete.0.set(true);
        complete.1.notify(usize::MAX);
        result
    });
    loop {
        let changed = completion.1.listen();
        if completion.0.get() {
            return Ok(());
        }
        changed.await;
    }
}

pub(super) async fn send(
    request: DeviceRequest,
    context: Rc<Context>,
    record: Rc<DeviceRecord>,
    id: &str,
    gate: Rc<super::LightWriteGate>,
) -> Result<(), AdapterError> {
    let did = record.physical.key().id.clone();
    let native_group = context.is_group(&did);
    let target = match &request {
        DeviceRequest::Power(request) => LightTarget {
            power: Some(request.target),
            ..Default::default()
        },
        DeviceRequest::Light(request) => request.target,
    };
    if current(&request) {
        context.take_over_related_targets(&did, target);
    }
    if let Some(LightColorTarget::Rgb(rgb)) = target.color
        && let Some(plan) = context.group_repair(&did)
    {
        let DeviceRequest::Light(request) = request else {
            unreachable!("RGB requests are light requests");
        };
        return repair::send(request, rgb, context, record, plan).await;
    }
    let properties = match super::super::lighting::encode(&record, id, target, native_group) {
        Ok(properties) => properties,
        Err(error) => {
            reject(request, error);
            return Ok(());
        }
    };
    let diagnostic_target = match &request {
        DeviceRequest::Power(request) => CommandTarget::Power(request.target),
        DeviceRequest::Light(_) => CommandTarget::Light(target),
    };
    let operation = context.commands.begin_operation(
        &did,
        id,
        properties
            .first()
            .map_or(record.binding(id), |(binding, _)| *binding),
        diagnostic_target,
        properties.clone(),
        request.queued_at(),
    );

    let work = context
        .live
        .borrow()
        .devices
        .get(&did)
        .and_then(|slot| slot.work.clone());
    let Some(work) = work else {
        let error = CommandError::Unavailable("no usable control path".into());
        operation.0.finish(Err(error.clone()));
        reject(request, error);
        return Ok(());
    };
    let generation = record.binding_generation.get();
    let mut request = request;
    loop {
        if !alive(&work, generation) || !current(&request) {
            let error = current_result(&request)
                .err()
                .unwrap_or(CommandError::Cancelled);
            operation.0.finish(Err(error.clone()));
            reject(request, error);
            return Ok(());
        }
        let properties = match super::super::lighting::encode(&record, id, target, native_group) {
            Ok(properties) => properties,
            Err(error) => {
                operation.0.finish(Err(error.clone()));
                reject(request, error);
                return Ok(());
            }
        };
        operation.0.properties(properties.clone());
        if properties.is_empty() {
            operation.0.finish(skip(request));
            return Ok(());
        }
        let retain_feedback = context.is_independent_light(&did);
        let pending = Rc::new(RefCell::new(Some(request)));
        let mark: Rc<dyn Fn() -> Result<(), CommandError>> = {
            let pending = pending.clone();
            Rc::new(move || mark_sent(pending.borrow().as_ref().unwrap()))
        };
        let current: Rc<dyn Fn() -> Result<(), CommandError>> = {
            let pending = pending.clone();
            Rc::new(move || match pending.borrow().as_ref() {
                Some(request) => current_result(request),
                None => Err(CommandError::Cancelled),
            })
        };
        let frame_current: Rc<dyn Fn() -> bool> = {
            let record = record.clone();
            let id = id.to_owned();
            let properties = properties.clone();
            Rc::new(move || {
                super::super::lighting::encode(&record, &id, target, native_group)
                    .ok()
                    .as_ref()
                    == Some(&properties)
            })
        };
        match submit_properties(
            &context,
            &work,
            Batch {
                function_id: id,
                properties,
                retain_feedback,
                gate: &gate,
                operation: &operation.0,
                mark: &mark,
                current: &current,
                frame_current: Some(&frame_current),
            },
        )
        .await
        {
            BatchAttempt::Withdrawn => {
                request = pending.borrow_mut().take().unwrap();
            }
            BatchAttempt::Settled(receipt) => {
                if matches!(target.color, Some(LightColorTarget::Temperature(_))) {
                    repair::refresh_native_color_mode(&context, &did);
                }
                let request = pending.borrow_mut().take().unwrap();
                let result = match receipt.result {
                    Ok(()) => accept(request),
                    Err(error) => {
                        reject(request, error.clone());
                        Err(error)
                    }
                };
                operation.0.finish(result);
                if let Some(error) = receipt.maintenance {
                    return Err(error);
                }
                return Ok(());
            }
        }
    }
}
