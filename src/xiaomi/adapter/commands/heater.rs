use std::{collections::VecDeque, rc::Rc};

use serde_json::{Value, json};

use super::{
    DeviceWriteGate,
    steps::{self, Operation, StepRequest},
};
use crate::{
    device::{CommandError, HeaterRequest, HeaterStep},
    xiaomi::{
        adapter::{
            model::{AdapterError, CommandTarget},
            state::{Context, LogicalSink},
            work::DeviceWork,
        },
        capability::{HeaterBindings, PropertyBinding},
    },
};

struct HeaterSequence {
    request: HeaterRequest,
    bindings: HeaterBindings,
    work: Rc<DeviceWork>,
    id: String,
}
impl StepRequest for HeaterSequence {
    type Step = HeaterStep;

    fn steps(&self) -> Result<VecDeque<(Self::Step, (PropertyBinding, Value))>, CommandError> {
        Ok(self
            .request
            .plan()?
            .into_iter()
            .map(|step| {
                let property = match step {
                    HeaterStep::Power(value) => (self.bindings.power(), json!(value)),
                    HeaterStep::Temperature(value) => (
                        self.bindings.temperature().property(),
                        self.bindings.temperature().encode(value),
                    ),
                };
                (step, property)
            })
            .collect())
    }
    fn current_result(&self) -> Result<(), CommandError> {
        self.request.current_result()
    }
    fn send_result(&self, step: Self::Step) -> Result<(), CommandError> {
        self.request.send_result(step)
    }
    fn first_step_current(&self, step: Self::Step) -> bool {
        self.request
            .plan()
            .is_ok_and(|steps| steps.first() == Some(&step))
    }
    fn mark_sent(&self, step: Self::Step, binding: PropertyBinding) -> Result<(), CommandError> {
        self.request.mark_sent(step)?;
        if step == HeaterStep::Power(false) {
            self.work.observations.invalidate_write(&self.id, binding);
        }
        Ok(())
    }
    fn step_accepted(&self, step: Self::Step) -> Result<(), CommandError> {
        self.request.step_accepted(step)
    }
    fn accept(self) -> Result<(), CommandError> {
        self.request.accept().map(|_| ())
    }
    fn reject(self, error: CommandError) {
        self.request.reject(error)
    }
}

pub(super) async fn send(
    request: HeaterRequest,
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
        .heater()
        .unwrap()
        .clone();
    let operation = Operation {
        binding: bindings.power(),
        target: CommandTarget::Heater(request.command),
        queued_at: request.queued_at(),
    };
    let sequence = HeaterSequence {
        request,
        bindings,
        work: work.clone(),
        id: id.into(),
    };
    steps::send(sequence, context, work, sink, id, gate, operation).await
}
