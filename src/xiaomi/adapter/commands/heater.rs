use std::{collections::VecDeque, rc::Rc};

use serde_json::json;

use super::{
    DeviceWriteGate,
    steps::{self, Operation, StepRequest, StepTarget},
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

    async fn steps(
        &self,
        _: &Context,
        _: &LogicalSink,
    ) -> Result<VecDeque<(Self::Step, StepTarget)>, CommandError> {
        let step = self.request.step();
        let (binding, value) = match step {
            HeaterStep::Power(value) => (self.bindings.power(), json!(value)),
            HeaterStep::Temperature(value) => (
                self.bindings.temperature().property(),
                self.bindings.temperature().encode(value),
            ),
        };
        Ok(VecDeque::from([(
            step,
            StepTarget::property(binding, value),
        )]))
    }
    fn current_result(&self) -> Result<(), CommandError> {
        self.request.current_result()
    }
    fn mark_sent(&self, step: Self::Step, binding: PropertyBinding) -> Result<(), CommandError> {
        self.request.mark_sent()?;
        if step == HeaterStep::Power(false) {
            self.work.observations.invalidate_write(&self.id, binding);
        }
        Ok(())
    }
    fn step_accepted(&self, _step: Self::Step) -> Result<(), CommandError> {
        self.request.step_accepted()
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
