use super::super::{
    model::{AdapterError, CommandTarget, PathKind},
    routing::ControlPath,
    state::{Context, DeviceRecord},
    work::DeviceWork,
};
use super::command_error;
use crate::{
    device::{CommandError, DeviceRequest, LightPatch, LightTarget},
    xiaomi::{
        capability::PropertyBinding,
        central::{CentralError, RequestStage, SendDecision, SendHooks},
        cloud::{CloudError, CloudErrorKind},
        property::PropertyResults,
    },
};
use event_listener::Event;
use serde_json::Value;
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
fn apply<E>(
    work: &DeviceWork,
    id: &str,
    properties: &[(PropertyBinding, Value)],
    stamps: &[super::super::observations::WriteStamp],
    results: &PropertyResults<(), E>,
) {
    if work.life.is_closed() || work.record.life.is_closed() {
        return;
    }
    let cached = work.record.cached.borrow();
    let description = cached
        .device
        .functions
        .iter()
        .find(|function| function.description.id == id)
        .unwrap();
    let mut patch = LightPatch::default();
    for (((binding, value), stamp), result) in properties.iter().zip(stamps).zip(&results.items) {
        if result.is_ok()
            && work
                .observations
                .write_is_current(&work.record, id, *binding, stamp)
        {
            super::super::lighting::merge(
                &mut patch,
                super::super::lighting::decode(description, *binding, value)
                    .expect("encoded property"),
            );
            work.observations
                .observed(&work.record, id, *binding, false);
        }
    }
    drop(cached);
    work.record
        .physical
        .function(id)
        .unwrap()
        .observe_light(patch)
        .expect("encoded light patch");
}
fn alive(work: &DeviceWork, generation: u64) -> bool {
    !work.life.is_closed()
        && !work.record.life.is_closed()
        && work.record.binding_generation.get() == generation
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
    context.tasks.spawn(async move {
        let result = send(request, owner, record, &id).await;
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
) -> Result<(), AdapterError> {
    let did = record.physical.key().id.clone();
    let target = match &request {
        DeviceRequest::Power(request) => LightTarget {
            power: Some(request.target),
            ..Default::default()
        },
        DeviceRequest::Light(request) => request.target,
    };
    let properties = match super::super::lighting::encode(&record, id, target) {
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
        let properties = match super::super::lighting::encode(&record, id, target) {
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
        let Some(path) = context.control_path(&did) else {
            let error = CommandError::Unavailable("no usable control path".into());
            operation.0.finish(Err(error.clone()));
            reject(request, error);
            return Ok(());
        };
        match path {
            ControlPath::Local(handle) => {
                let pending = Rc::new(RefCell::new(Some(request)));
                let guard_pending = pending.clone();
                let guard_work = work.clone();
                let guard_context = context.clone();
                let guard_handle = handle.clone();
                let guard_did = did.clone();
                let guard_id = id.to_owned();
                let guard_properties = properties.clone();
                let capture_pending = pending.clone();
                let capture_work = work.clone();
                let capture_id = id.to_owned();
                let capture_properties = properties.clone();
                let submitted = operation.0.clone();
                let stamps = Rc::new(RefCell::new(Vec::new()));
                let capture_stamps = stamps.clone();
                let result_pending = pending.clone();
                let result_work = work.clone();
                let result_context = context.clone();
                let result_handle = handle.clone();
                let result_properties = properties.clone();
                let result_id = id.to_owned();
                let result_operation = operation.0.clone();
                let hooks = SendHooks::new(
                    move || {
                        if !alive(&guard_work, generation)
                            || !guard_context.current_local(&guard_handle)
                            || !current(guard_pending.borrow().as_ref().unwrap())
                        {
                            SendDecision::Cancel
                        } else if super::super::lighting::encode(
                            &guard_work.record,
                            &guard_id,
                            target,
                        )
                        .ok()
                        .as_ref()
                            != Some(&guard_properties)
                        {
                            SendDecision::Withdraw
                        } else if matches!(guard_context.control_path(&guard_did), Some(ControlPath::Local(selected)) if selected.same_connection(&guard_handle))
                        {
                            SendDecision::Send
                        } else {
                            SendDecision::Withdraw
                        }
                    },
                    move || {
                        // Rc tasks cannot interleave guard, socket poll and this callback.
                        mark_sent(capture_pending.borrow().as_ref().unwrap())
                            .expect("synchronous send guard");
                        submitted.submitted(PathKind::Local);
                        *capture_stamps.borrow_mut() = capture_properties
                            .iter()
                            .map(|(binding, _)| {
                                capture_work.observations.write_stamp(
                                    &capture_work.record,
                                    &capture_id,
                                    *binding,
                                )
                            })
                            .collect();
                        capture_work.observations.register_feedback(
                            &capture_work.record,
                            &capture_id,
                            &capture_properties,
                        );
                    },
                );
                handle
                    .set_properties_guarded(&did, &properties, hooks, move |outcome| {
                        result_context.local_device_outcome(
                            &result_handle,
                            &result_work,
                            generation,
                            &outcome,
                        );
                        if outcome.result == Err(CentralError::Withdrawn)
                            && outcome.stage == RequestStage::NotSent
                        {
                            return;
                        }
                        let request = result_pending.borrow_mut().take().unwrap();
                        result_work.observations.finish_feedback(&result_id);
                        let result = match outcome.result {
                            Ok(results) => {
                                apply(
                                    &result_work,
                                    &result_id,
                                    &result_properties,
                                    &stamps.borrow(),
                                    &results,
                                );
                                for error in results
                                    .items
                                    .iter()
                                    .filter_map(|result| result.as_ref().err())
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
                                if results.protocol_error {
                                    Err(command_error(CentralError::Protocol))
                                } else if let Some(error) =
                                    results.items.into_iter().find_map(Result::err)
                                {
                                    Err(command_error(error))
                                } else {
                                    Ok(())
                                }
                            }
                            Err(error) => {
                                if outcome.stage == RequestStage::NotSent && !current(&request) {
                                    current_result(&request)
                                } else {
                                    Err(command_error(error))
                                }
                            }
                        };
                        match result {
                            Ok(()) => result_operation.finish(accept(request)),
                            Err(error) => {
                                result_operation.finish(Err(error.clone()));
                                reject(request, error);
                            }
                        }
                    })
                    .await;
                let withdrawn = pending.borrow_mut().take();
                if let Some(withdrawn) = withdrawn {
                    request = withdrawn;
                } else {
                    return Ok(());
                }
            }
            ControlPath::Cloud(credentials) => {
                // No await separates the final ownership/path check from HTTP submission.
                if !alive(&work, generation) || !current(&request) {
                    let error = current_result(&request)
                        .err()
                        .unwrap_or(CommandError::Cancelled);
                    operation.0.finish(Err(error.clone()));
                    reject(request, error);
                    return Ok(());
                }
                if let Err(error) = mark_sent(&request) {
                    operation.0.finish(Err(error.clone()));
                    reject(request, error);
                    return Ok(());
                }
                let stamps: Vec<_> = properties
                    .iter()
                    .map(|(binding, _)| work.observations.write_stamp(&record, id, *binding))
                    .collect();
                work.observations
                    .register_feedback(&record, id, &properties);
                operation.0.submitted(PathKind::Cloud);
                let result = context
                    .cloud
                    .set_properties(&credentials.access_token, &did, &properties)
                    .await;
                work.observations.finish_feedback(id);
                let outcome = match &result {
                    Ok(results) => {
                        apply(&work, id, &properties, &stamps, results);
                        if results.protocol_error {
                            Err(CommandError::SendFailed("invalid cloud response".into()))
                        } else if let Some(error) =
                            results.items.iter().find_map(|item| item.as_ref().err())
                        {
                            Err(cloud_command_error(error))
                        } else {
                            Ok(())
                        }
                    }
                    Err(error) => Err(cloud_command_error(error)),
                };
                match outcome {
                    Ok(()) => operation.0.finish(accept(request)),
                    Err(error) => {
                        operation.0.finish(Err(error.clone()));
                        reject(request, error);
                    }
                }
                // Settle the original receipt even if persisting refreshed access fails.
                match result {
                    Ok(results) => {
                        for error in results.items.iter().filter_map(|item| item.as_ref().err()) {
                            context.cloud_error(
                                &credentials.access_token,
                                alive(&work, generation).then_some(did.as_str()),
                                error,
                            )?;
                        }
                    }
                    Err(error) => context.cloud_error(
                        &credentials.access_token,
                        alive(&work, generation).then_some(did.as_str()),
                        &error,
                    )?,
                }
                return Ok(());
            }
        }
    }
}
