use super::super::{
    model::{AdapterError, CommandTarget},
    state::{Context, LogicalSink},
    work::DeviceWork,
};
use super::properties::{Batch, BatchAttempt, alive, submit_properties};
use crate::{
    device::{CommandError, DeviceRequest, LightColorTarget, LightTarget},
    xiaomi::capability::PropertyBinding,
};
use serde_json::Value;

mod repair;
use std::{cell::RefCell, rc::Rc};

fn current(request: &DeviceRequest) -> bool {
    current_result(request).is_ok()
}
fn current_result(request: &DeviceRequest) -> Result<(), CommandError> {
    match request {
        DeviceRequest::Power(request) => request.current_result(),
        DeviceRequest::Light(request) => request.current_result(),
        DeviceRequest::Curtain(_) | DeviceRequest::Fan(_) => {
            unreachable!("non-light requests use their own sender")
        }
    }
}
fn mark_sent(request: &DeviceRequest) -> Result<(), CommandError> {
    match request {
        DeviceRequest::Power(request) => request.mark_sent(),
        DeviceRequest::Light(request) => request.mark_sent(),
        DeviceRequest::Curtain(_) | DeviceRequest::Fan(_) => {
            unreachable!("non-light requests use their own sender")
        }
    }
}
fn reject(request: DeviceRequest, error: CommandError) {
    match request {
        DeviceRequest::Power(request) => request.reject(error),
        DeviceRequest::Light(request) => request.reject(error),
        DeviceRequest::Curtain(_) | DeviceRequest::Fan(_) => {
            unreachable!("non-light requests use their own sender")
        }
    }
}
fn accept(request: DeviceRequest) -> Result<(), CommandError> {
    match request {
        DeviceRequest::Power(request) => request.accept().map(|_| ()),
        DeviceRequest::Light(request) => request.accept().map(|_| ()),
        DeviceRequest::Curtain(_) | DeviceRequest::Fan(_) => {
            unreachable!("non-light requests use their own sender")
        }
    }
}
fn skip(request: DeviceRequest) -> Result<(), CommandError> {
    match request {
        DeviceRequest::Power(request) => request.accept().map(|_| ()),
        DeviceRequest::Light(request) => request.accept_unchanged().map(|_| ()),
        DeviceRequest::Curtain(_) | DeviceRequest::Fan(_) => {
            unreachable!("non-light requests use their own sender")
        }
    }
}
/// The DID gate still owns this receipt. Keep displayed feedback, but do not
/// use it to omit later writes until fresh feedback arrives.
fn refresh_after_write(
    context: &Context,
    original: &DeviceWork,
    id: &str,
    properties: &[(PropertyBinding, Value)],
    gate: &super::DeviceWriteGate,
    authority: u64,
) {
    if gate.authority() != authority {
        return;
    }
    let Some(work) = context
        .live
        .borrow()
        .devices
        .get(&original.key.id)
        .and_then(|slot| slot.admitted.clone())
    else {
        return;
    };
    if work.life.is_closed() {
        return;
    }
    let mode = {
        let original = original
            .native
            .device()
            .functions()
            .iter()
            .find(|function| function.id() == id)
            .unwrap();
        let Some(function) = work
            .native
            .device()
            .functions()
            .iter()
            .find(|function| function.id() == id)
        else {
            return;
        };
        if original.power() != function.power() || original.lighting() != function.lighting() {
            return;
        }
        let light = function.lighting().unwrap();
        if context.is_virtual_group(&work.key.id) {
            let written = |property| properties.iter().any(|(binding, _)| *binding == property);
            let power = written(function.power().expect("light functions have power"));
            let brightness = light
                .brightness()
                .is_some_and(|binding| written(binding.property()));
            let temperature = light
                .temperature()
                .is_some_and(|binding| written(binding.property()));
            let color = light
                .color()
                .is_some_and(|binding| written(binding.property()));
            for member in context.group_member_work(&work.key.id) {
                let function = member.native.device().unique_light().unwrap();
                let light = function.lighting().unwrap();
                // Power can restore the member's remembered settings. Other
                // writes refresh the affected value and its actual mode.
                let bindings = if power {
                    function.bindings()
                } else {
                    [
                        light
                            .brightness()
                            .filter(|_| brightness)
                            .map(|binding| binding.property()),
                        light
                            .temperature()
                            .filter(|_| temperature)
                            .map(|binding| binding.property()),
                        light
                            .color()
                            .filter(|_| color)
                            .map(|binding| binding.property()),
                        light
                            .mode()
                            .filter(|_| temperature || color)
                            .map(|binding| binding.property()),
                    ]
                    .into_iter()
                    .flatten()
                    .collect()
                };
                for binding in bindings {
                    member.observations.invalidate_write(function.id(), binding);
                    member.request_dirty_read(function.id(), binding);
                }
            }
            return;
        }
        let color_written = light.color().is_some_and(|binding| {
            properties
                .iter()
                .any(|(property, _)| *property == binding.property())
        }) || light.temperature().is_some_and(|binding| {
            properties
                .iter()
                .any(|(property, _)| *property == binding.property())
        });
        light
            .mode()
            .filter(|mode| {
                color_written
                    && !properties
                        .iter()
                        .any(|(binding, _)| *binding == mode.property())
            })
            .map(|mode| mode.property())
    };
    for binding in properties.iter().map(|(binding, _)| *binding).chain(mode) {
        work.observations.invalidate_write(id, binding);
        work.request_dirty_read(id, binding);
    }
}

pub(super) async fn send(
    request: DeviceRequest,
    context: Rc<Context>,
    work: Rc<DeviceWork>,
    sink: Rc<LogicalSink>,
    id: &str,
    gate: Rc<super::DeviceWriteGate>,
) -> Result<(), AdapterError> {
    let did = work.key.id.clone();
    let native_group = context.is_group(&did);
    let light_target = match &request {
        DeviceRequest::Power(_) => None,
        DeviceRequest::Light(request) => Some(request.target),
        DeviceRequest::Curtain(_) | DeviceRequest::Fan(_) => {
            unreachable!("non-light requests use their own sender")
        }
    };
    let power = match &request {
        DeviceRequest::Power(request) => Some(request.target),
        DeviceRequest::Light(request) => request.target.power(),
        DeviceRequest::Curtain(_) | DeviceRequest::Fan(_) => {
            unreachable!("non-light requests use their own sender")
        }
    };
    if current(&request) {
        context.take_over_related_targets(&did, power, light_target.map(LightTarget::dimension));
    }
    if let Some(target) = light_target
        && let Some(LightColorTarget::Rgb(rgb)) = target.color_target()
        && let Some(plan) = context.group_repair(&did)
    {
        let DeviceRequest::Light(request) = request else {
            unreachable!("RGB requests are light requests");
        };
        return repair::send(request, rgb, context, work, sink, plan).await;
    }
    let properties =
        match super::super::lighting::encode(&work, id, power, light_target, native_group) {
            Ok(properties) => properties,
            Err(error) => {
                reject(request, error);
                return Ok(());
            }
        };
    let diagnostic_target = match &request {
        DeviceRequest::Power(request) => CommandTarget::Power(request.target),
        DeviceRequest::Light(_) => {
            CommandTarget::Light(light_target.expect("light requests carry a target"))
        }
        DeviceRequest::Curtain(_) | DeviceRequest::Fan(_) => {
            unreachable!("non-light requests use their own sender")
        }
    };
    let operation = context.commands.begin_operation(
        &did,
        id,
        properties
            .first()
            .map_or(work.binding(id), |(binding, _)| *binding),
        diagnostic_target,
        properties.clone(),
        request.queued_at(),
    );

    let mut request = request;
    loop {
        if !alive(&work, &sink) || !current(&request) {
            let error = current_result(&request)
                .err()
                .unwrap_or(CommandError::Cancelled);
            operation.0.finish(Err(error.clone()));
            reject(request, error);
            return Ok(());
        }
        let properties =
            match super::super::lighting::encode(&work, id, power, light_target, native_group) {
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
            let work = work.clone();
            let id = id.to_owned();
            let properties = properties.clone();
            Rc::new(move || {
                super::super::lighting::encode(&work, &id, power, light_target, native_group)
                    .ok()
                    .as_ref()
                    == Some(&properties)
            })
        };
        match submit_properties(
            &context,
            &work,
            &sink,
            Batch {
                function_id: id,
                properties,
                retain_feedback,
                after_write: Some(refresh_after_write),
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
                if !context.is_virtual_group(&did)
                    && light_target
                        .and_then(LightTarget::color_target)
                        .is_some_and(|color| matches!(color, LightColorTarget::Temperature(_)))
                {
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
