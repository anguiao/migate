use super::{
    model::{AdapterError, PathKind, StepReceipt},
    state::{Context, LogicalSink},
    work::DeviceWork,
};
use crate::{
    device::{CommandError, CommandReceiver, DeviceRequest},
    xiaomi::central::CentralError,
};
use event_listener::Event;
use futures_lite::future;
use std::{cell::Cell, rc::Rc};

mod air_conditioner;
mod curtain;
mod fan;
mod feedback;
mod gates;
mod heater;
mod lighting;
mod power;
mod properties;
mod records;
mod refresh;
mod steps;
pub(super) use fan::SharedFanPower;
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
            DeviceRequest::Curtain(_)
                | DeviceRequest::Fan(_)
                | DeviceRequest::AirConditioner(_)
                | DeviceRequest::Heater(_)
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
                DeviceRequest::Heater(request) => {
                    heater::send(request, owner, work, sink, &id, gate).await
                }
                request => lighting::send(request, owner, work, sink, &id, gate).await,
            }
        } else {
            let DeviceRequest::Power(request) = request else {
                unreachable!("only ordinary power commands have no device write gate")
            };
            power::send(request, owner, work, sink, &id).await
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
