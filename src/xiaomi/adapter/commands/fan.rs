use std::{cell::RefCell, collections::VecDeque, rc::Rc};

use serde_json::{Value, json};

use super::{
    DeviceWriteGate,
    properties::{Batch, BatchAttempt, alive, submit_properties},
};
use crate::{
    device::{CommandError, FanCommand, FanRequest, FanStep},
    xiaomi::adapter::{
        model::{AdapterError, CommandTarget},
        state::{Context, LogicalSink},
        work::DeviceWork,
    },
    xiaomi::capability::{FanBindings, PropertyBinding},
};

fn encode(
    command: FanCommand,
    bindings: &FanBindings,
    needs_power_on: bool,
) -> VecDeque<(FanStep, (PropertyBinding, Value))> {
    let mut steps = VecDeque::new();
    match command {
        FanCommand::Power(value) => {
            steps.push_back((FanStep::Power(value), (bindings.power(), json!(value))))
        }
        FanCommand::Speed(0) => {
            steps.push_back((FanStep::Power(false), (bindings.power(), json!(false))))
        }
        FanCommand::Speed(value) => {
            if needs_power_on {
                steps.push_back((FanStep::Power(true), (bindings.power(), json!(true))));
            }
            steps.push_back((
                FanStep::Mode,
                (
                    bindings.mode().property(),
                    json!(bindings.mode().straight()),
                ),
            ));
            steps.push_back((
                FanStep::Speed(value),
                (bindings.speed().property(), json!(value)),
            ));
        }
        FanCommand::Swing(value) => steps.push_back((
            FanStep::Swing(value),
            (bindings.swing().unwrap(), json!(value)),
        )),
    }
    steps
}

pub(super) async fn send(
    request: FanRequest,
    context: Rc<Context>,
    work: Rc<DeviceWork>,
    sink: Rc<LogicalSink>,
    id: &str,
    gate: Rc<DeviceWriteGate>,
) -> Result<(), AdapterError> {
    let bindings = work
        .native
        .device()
        .functions()
        .iter()
        .find(|function| function.id() == id)
        .unwrap()
        .fan()
        .unwrap();
    let command = request.command;
    let mut steps = encode(command, bindings, request.needs_power_on());
    let operation = context.commands.begin_operation(
        &work.key.id,
        id,
        steps[0].1.0,
        CommandTarget::Fan(request.command),
        Vec::new(),
        request.queued_at(),
    );
    let pending = Rc::new(RefCell::new(Some(request)));
    let current: Rc<dyn Fn() -> Result<(), CommandError>> = {
        let pending = pending.clone();
        Rc::new(move || pending.borrow().as_ref().unwrap().current_result())
    };
    let mut first_step = true;
    while let Some((step, property)) = steps.pop_front() {
        operation.0.fan_step(property.clone());
        let mark: Rc<dyn Fn() -> Result<(), CommandError>> = {
            let pending = pending.clone();
            let work = work.clone();
            let id = id.to_owned();
            let binding = property.0;
            Rc::new(move || {
                pending.borrow().as_ref().unwrap().mark_sent(step)?;
                if step == FanStep::Power(false) {
                    work.observations.invalidate_write(&id, binding);
                }
                Ok(())
            })
        };
        let frame_current: Option<Rc<dyn Fn() -> bool>> =
            if first_step && matches!(command, FanCommand::Speed(1..=100)) {
                let pending = pending.clone();
                let planned_on = step == FanStep::Power(true);
                Some(Rc::new(move || {
                    pending.borrow().as_ref().unwrap().needs_power_on() == planned_on
                }))
            } else {
                None
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
                    after_write: None,
                    gate: &gate,
                    operation: &operation.0,
                    mark: &mark,
                    current: &current,
                    frame_current: frame_current.as_ref(),
                },
            )
            .await
            {
                BatchAttempt::Withdrawn => {
                    if frame_current.as_ref().is_some_and(|current| !current()) {
                        let needs_power_on = pending.borrow().as_ref().unwrap().needs_power_on();
                        steps = encode(command, bindings, needs_power_on);
                        break;
                    }
                    continue;
                }
                BatchAttempt::Settled(receipt) => {
                    let result = receipt.result.and_then(|()| {
                        if !alive(&work, &sink) {
                            return Err(CommandError::Cancelled);
                        }
                        let pending = pending.borrow();
                        let request = pending.as_ref().unwrap();
                        request.current_result()?;
                        request.step_accepted()
                    });
                    if let Err(error) = result {
                        pending.borrow_mut().take().unwrap().reject(error.clone());
                        operation.0.finish(Err(error));
                        if let Some(error) = receipt.maintenance {
                            return Err(error);
                        }
                        return Ok(());
                    }
                    if let Some(error) = receipt.maintenance {
                        pending
                            .borrow_mut()
                            .take()
                            .unwrap()
                            .reject(CommandError::Cancelled);
                        operation.0.finish(Err(CommandError::Cancelled));
                        return Err(error);
                    }
                    first_step = false;
                    break;
                }
            }
        }
    }
    let result = pending.borrow_mut().take().unwrap().accept().map(|_| ());
    operation.0.finish(result);
    Ok(())
}
