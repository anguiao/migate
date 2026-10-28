use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

use event_listener::Event;
use serde_json::json;

use super::{
    LightWriteGate,
    lighting::{Batch, BatchAttempt, submit_properties},
};
use crate::{
    device::{CommandError, CurtainCommand, CurtainRequest},
    xiaomi::adapter::{
        model::{AdapterError, CommandTarget},
        state::{Context, LogicalSink},
        work::DeviceWork,
    },
};

/// The receipt task and DID gate survive retirement of the consumer.
pub(super) async fn send_owned(
    request: CurtainRequest,
    context: Rc<Context>,
    work: Rc<DeviceWork>,
    sink: Rc<LogicalSink>,
    id: &str,
) -> Result<(), AdapterError> {
    let completion = Rc::new((Cell::new(false), Event::new()));
    let complete = completion.clone();
    let owner = context.clone();
    let id = id.to_owned();
    let gate = context
        .light_writes
        .for_device(&work.key.id, context.clock.clone());
    context.tasks.spawn(async move {
        let _permit = gate.acquire().await;
        let result = send(request, owner, work, sink, &id, gate).await;
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

async fn send(
    request: CurtainRequest,
    context: Rc<Context>,
    work: Rc<DeviceWork>,
    sink: Rc<LogicalSink>,
    id: &str,
    gate: Rc<LightWriteGate>,
) -> Result<(), AdapterError> {
    let did = work.key.id.clone();
    let bindings = work
        .native
        .device()
        .functions()
        .iter()
        .find(|function| function.id() == id)
        .unwrap()
        .curtain()
        .unwrap();
    let property = match request.command {
        CurtainCommand::Target(value) => (
            bindings.target().property(),
            json!(bindings.target().quantize(value)),
        ),
        CurtainCommand::Stop => (bindings.motor().property(), json!(bindings.motor().pause())),
    };
    let operation = context.commands.begin_operation(
        &did,
        id,
        property.0,
        CommandTarget::Curtain(request.command),
        vec![property.clone()],
        request.queued_at(),
    );
    let pending = Rc::new(RefCell::new(Some(request)));
    let mark: Rc<dyn Fn() -> Result<(), CommandError>> = {
        let pending = pending.clone();
        Rc::new(move || pending.borrow().as_ref().unwrap().mark_sent())
    };
    let current: Rc<dyn Fn() -> Result<(), CommandError>> = {
        let pending = pending.clone();
        Rc::new(move || pending.borrow().as_ref().unwrap().current_result())
    };
    loop {
        match submit_properties(
            &context,
            &work,
            &sink,
            Batch {
                function_id: id,
                properties: vec![property.clone()],
                retain_feedback: false,
                refresh_feedback: false,
                gate: &gate,
                operation: &operation.0,
                mark: &mark,
                current: &current,
                frame_current: None,
            },
        )
        .await
        {
            BatchAttempt::Withdrawn => continue,
            BatchAttempt::Settled(receipt) => {
                let request = pending.borrow_mut().take().unwrap();
                let result = match receipt.result {
                    Ok(()) => request.accept().map(|_| ()),
                    Err(error) => {
                        request.reject(error.clone());
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
