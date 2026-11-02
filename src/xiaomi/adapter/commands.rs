use super::{
    model::{AdapterError, PathKind, StepReceipt},
    routing::ControlPath,
    state::{Context, LogicalSink},
    work::DeviceWork,
};
use crate::{
    device::{CommandError, CommandReceiver, CommandRequest, DeviceRequest},
    xiaomi::central::{CentralError, RequestStage, SendDecision, SendHooks, SessionHandle},
};
use event_listener::Event;
use futures_lite::future;
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

mod air_conditioner;
mod curtain;
mod fan;
mod feedback;
mod gates;
mod properties;
mod records;
mod refresh;
mod steps;
pub(super) use gates::{DeviceWriteGate, DeviceWriteGates};
pub(super) use records::{CommandOperation, Commands};

/// Only the consumer owns this lease; a transport callback cannot extend its lifetime.
struct CommandLease(Rc<CommandOperation>);
impl Drop for CommandLease {
    fn drop(&mut self) {
        self.0.finish(Err(CommandError::Cancelled));
    }
}

fn command_error(error: CentralError) -> CommandError {
    match error {
        CentralError::Timeout => CommandError::Timeout,
        CentralError::Rejected(code) => CommandError::Rejected(format!("gateway code {code}")),
        CentralError::Cancelled => CommandError::Cancelled,
        error => CommandError::SendFailed(error.to_string()),
    }
}
/// Return ownership only when the request was withdrawn before transmission.
async fn send_local_write(
    context: &Rc<Context>,
    handle: SessionHandle,
    work: Rc<DeviceWork>,
    sink: Rc<LogicalSink>,
    id: &str,
    request: CommandRequest,
    operation: Rc<CommandOperation>,
) -> Option<CommandRequest> {
    let binding = work.binding(id);
    let did = work.key.id.clone();
    let target = request.target;
    let pending = Rc::new(RefCell::new(Some(request)));
    let completion = pending.clone();
    let observing = work.clone();
    let observing_sink = sink.clone();
    let id = id.to_owned();
    let stamp = Rc::new(RefCell::new(None));
    let capture_stamp = stamp.clone();
    let capture_work = work.clone();
    let capture_id = id.clone();
    let guard_work = work.clone();
    let guard_sink = sink.clone();
    let guard_context = context.clone();
    let guard_handle = handle.clone();
    let guard_did = did.clone();
    let guard_id = id.clone();
    let result_context = context.clone();
    let result_handle = handle.clone();
    let submitted = operation.clone();
    let hooks = SendHooks::new(
        move || {
            if guard_work.life.is_closed()
                || guard_sink.life.is_closed()
                || !guard_context.current_local(&guard_handle)
            {
                SendDecision::Cancel
            } else if matches!(guard_context.control_path_for(&guard_did, &guard_id), Some(ControlPath::Local(selected)) if selected.same_connection(&guard_handle))
            {
                SendDecision::Send
            } else {
                SendDecision::Withdraw
            }
        },
        move || {
            submitted.submitted(super::model::PathKind::Local);
            let read_pending = capture_work.refresh.borrow().contains(&capture_id, binding);
            *capture_stamp.borrow_mut() = Some(capture_work.observations.write_stamp(
                &capture_id,
                binding,
                read_pending,
            ));
        },
    );
    handle
        .set_property_guarded(&did, binding, target, hooks, move |outcome| {
            result_context.local_device_outcome(&result_handle, &observing, &outcome);
            if outcome.result == Err(CentralError::Withdrawn)
                && outcome.stage == RequestStage::NotSent
            {
                return;
            }
            let request = completion.borrow_mut().take().unwrap();
            match outcome.result {
                Ok(()) => {
                    operation.receipts(vec![StepReceipt::Accepted]);
                    if let Some(stamp) = stamp.borrow_mut().take() {
                        let result = observing.observations.accept_write(
                            &observing,
                            &observing_sink,
                            &id,
                            &stamp,
                            !refresh::notifications_ready(&result_context, &observing, &id),
                            request,
                        );
                        operation.schedule_feedback(&result_context, &observing, &observing_sink);
                        operation.finish(result);
                    } else {
                        let error =
                            CommandError::SendFailed("gateway accepted an unsent write".into());
                        operation.reject(request, error);
                    }
                }
                Err(error) => {
                    if outcome.stage != RequestStage::NotSent {
                        operation.receipts(vec![properties::local_receipt(&error)]);
                    }
                    operation.reject(request, command_error(error));
                }
            }
        })
        .await;
    pending.borrow_mut().take()
}

pub(super) async fn consume(
    mut receiver: CommandReceiver,
    context: Rc<Context>,
    work: Rc<DeviceWork>,
    sink: Rc<LogicalSink>,
    function_id: String,
) -> Result<(), AdapterError> {
    future::or(
        async {
            futures_lite::future::or(work.life.ended(), sink.life.ended()).await;
            Ok(())
        },
        async {
            while let Some(request) = receiver.next_operation().await {
                send_owned(
                    request,
                    context.clone(),
                    work.clone(),
                    sink.clone(),
                    &function_id,
                )
                .await;
            }
            Ok(())
        },
    )
    .await
}

/// Keep every original receipt alive when its consumer or device is retired.
async fn send_owned(
    request: DeviceRequest,
    context: Rc<Context>,
    work: Rc<DeviceWork>,
    sink: Rc<LogicalSink>,
    id: &str,
) {
    let light = work
        .native
        .device()
        .functions()
        .iter()
        .find(|function| function.id() == id)
        .unwrap()
        .lighting()
        .is_some();
    let gate = (light
        || matches!(
            &request,
            DeviceRequest::Curtain(_) | DeviceRequest::Fan(_) | DeviceRequest::AirConditioner(_)
        ))
    .then(|| {
        context
            .write_gates
            .for_device(&work.key.id, context.clock.clone())
    });
    let completion = Rc::new((Cell::new(false), Event::new()));
    let done = completion.clone();
    let owner = context.clone();
    let id = id.to_owned();
    context.tasks.spawn(async move {
        let result = if let Some(gate) = gate {
            let _permit = gate.acquire().await;
            match request {
                DeviceRequest::Curtain(request) => {
                    curtain::send(request, owner, work, sink, &id, gate).await
                }
                DeviceRequest::Fan(request) => {
                    fan::send(request, owner, work, sink, &id, gate).await
                }
                DeviceRequest::AirConditioner(request) => {
                    air_conditioner::send(request, owner, work, sink, &id, gate).await
                }
                request => lighting::send(request, owner, work, sink, &id, gate).await,
            }
        } else {
            let DeviceRequest::Power(request) = request else {
                unreachable!("only outlet commands have no device write gate")
            };
            send_power(request, owner, work, sink, &id).await
        };
        done.0.set(true);
        done.1.notify(usize::MAX);
        result
    });
    loop {
        let changed = completion.1.listen();
        if completion.0.get() {
            return;
        }
        changed.await;
    }
}

async fn send_power(
    request: CommandRequest,
    context: Rc<Context>,
    work: Rc<DeviceWork>,
    sink: Rc<LogicalSink>,
    function_id: &str,
) -> Result<(), AdapterError> {
    let did = work.key.id.clone();
    let binding = work.binding(function_id);
    let target = request.target;
    let command = context
        .commands
        .begin(&did, function_id, binding, target, request.queued_at());
    let mut request = request;
    loop {
        if work.life.is_closed() || sink.life.is_closed() {
            command.0.reject(request, CommandError::Cancelled);
            break;
        }
        if let Err(error) = request.current_result() {
            command.0.reject(request, error);
            break;
        }
        let Some(path) = context.control_path_for(&did, function_id) else {
            command.0.reject(
                request,
                CommandError::Unavailable("no usable control path".into()),
            );
            break;
        };
        match path {
            ControlPath::Local(handle) => {
                let Some(withdrawn) = send_local_write(
                    &context,
                    handle,
                    work.clone(),
                    sink.clone(),
                    function_id,
                    request,
                    command.0.clone(),
                )
                .await
                else {
                    break;
                };
                request = withdrawn;
            }
            ControlPath::Direct(client) => {
                use crate::xiaomi::lan::{Failure, RequestError, SendStage};
                let stamp = RefCell::new(None);
                let result = super::lan::set_properties(
                    &client,
                    &did,
                    &[(binding, serde_json::Value::Bool(target))],
                    || {
                        !work.life.is_closed()
                            && !sink.life.is_closed()
                            && request.current_result().is_ok()
                            && matches!(context.control_path_for(&did, function_id), Some(ControlPath::Direct(selected)) if selected.same(&client))
                    },
                    || {
                        let _ = request.mark_sent();
                        command.0.submitted(super::model::PathKind::Direct);
                        let read_pending = work.refresh.borrow().contains(function_id, binding);
                        *stamp.borrow_mut() = Some(work.observations.write_stamp(function_id, binding, read_pending));
                    },
                ).await;
                match result {
                    Err(error)
                        if error.stage == SendStage::BeforeSend
                            && error.failure == Failure::Withdrawn =>
                    {
                        continue;
                    }
                    Err(error) => {
                        context.lan_business_failure(&did, &client, &error)?;
                        if error.stage == SendStage::BeforeSend {
                            continue;
                        }
                        command.0.receipts(vec![properties::direct_receipt(&error)]);
                        command
                            .0
                            .reject(request, properties::direct_command_error(&error));
                    }
                    Ok(results) => {
                        command.0.receipts(
                            results
                                .items
                                .iter()
                                .map(|item| match item {
                                    Ok(()) => StepReceipt::Accepted,
                                    Err(error) => properties::direct_receipt(error),
                                })
                                .collect(),
                        );
                        let mut maintenance = None;
                        if results.protocol_error {
                            maintenance = context
                                .lan_business_failure(
                                    &did,
                                    &client,
                                    &RequestError {
                                        stage: SendStage::Sent,
                                        failure: Failure::InvalidResponse,
                                    },
                                )
                                .err();
                            command.0.reject(
                                request,
                                CommandError::SendFailed("invalid direct response".into()),
                            );
                        } else if let Some(error) =
                            results.items.iter().find_map(|item| item.as_ref().err())
                        {
                            command
                                .0
                                .reject(request, properties::direct_command_error(error));
                        } else {
                            let result = work.observations.accept_write(
                                &work,
                                &sink,
                                function_id,
                                &stamp.into_inner().unwrap(),
                                true,
                                request,
                            );
                            command.0.finish(result);
                        }
                        command.0.schedule_feedback(&context, &work, &sink);
                        if let Some(error) = maintenance {
                            return Err(error);
                        }
                    }
                }
                break;
            }
            ControlPath::Cloud(credentials) => {
                let read_pending = work.refresh.borrow().contains(function_id, binding);
                let stamp = work
                    .observations
                    .write_stamp(function_id, binding, read_pending);
                command.0.submitted(super::model::PathKind::Cloud);
                match context
                    .cloud
                    .set_property(&credentials.access_token, &did, binding, target)
                    .await
                {
                    Ok(()) => {
                        command.0.receipts(vec![StepReceipt::Accepted]);
                        let result = work.observations.accept_write(
                            &work,
                            &sink,
                            function_id,
                            &stamp,
                            !refresh::notifications_ready(&context, &work, function_id),
                            request,
                        );
                        command.0.schedule_feedback(&context, &work, &sink);
                        command.0.finish(result);
                    }
                    Err(error) => {
                        command.0.receipts(vec![properties::cloud_receipt(&error)]);
                        let command_error = properties::cloud_command_error(&error);
                        command.0.reject(request, command_error);
                        let current = !work.life.is_closed() && !sink.life.is_closed();
                        // Account failures remain relevant after a binding is replaced.
                        context.cloud_error(
                            &credentials.access_token,
                            current.then_some(did.as_str()),
                            &error,
                        )?;
                    }
                }
                break;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;

mod lighting;
