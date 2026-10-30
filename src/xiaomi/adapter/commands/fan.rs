use std::{collections::VecDeque, rc::Rc};

use serde_json::{Value, json};

use super::{
    DeviceWriteGate,
    steps::{self, Operation, StepRequest},
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
            if let Some(mode) = bindings.mode() {
                steps.push_back((
                    FanStep::Auxiliary,
                    (mode.property(), json!(mode.straight())),
                ));
            }
            steps.push_back((
                FanStep::Speed(value),
                (
                    bindings.speed().property(),
                    json!(bindings.speed().encode(value)),
                ),
            ));
        }
        FanCommand::Swing(value) => steps.push_back((
            FanStep::Swing(value),
            (bindings.swing().unwrap(), json!(value)),
        )),
    }
    steps
}

struct FanSequence {
    request: FanRequest,
    bindings: FanBindings,
    work: Rc<DeviceWork>,
    id: String,
}
impl StepRequest for FanSequence {
    type Step = FanStep;

    fn steps(&self) -> Result<VecDeque<(FanStep, (PropertyBinding, Value))>, CommandError> {
        Ok(encode(
            self.request.command,
            &self.bindings,
            self.request.needs_power_on(),
        ))
    }

    fn current_result(&self) -> Result<(), CommandError> {
        self.request.current_result()
    }

    fn first_step_current(&self, step: FanStep) -> bool {
        !matches!(self.request.command, FanCommand::Speed(1..=100))
            || self.request.needs_power_on() == (step == FanStep::Power(true))
    }

    fn mark_sent(&self, step: FanStep, binding: PropertyBinding) -> Result<(), CommandError> {
        self.request.mark_sent(step)?;
        if step == FanStep::Power(false) {
            self.work.observations.invalidate_write(&self.id, binding);
        }
        Ok(())
    }

    fn step_accepted(&self, _step: FanStep) -> Result<(), CommandError> {
        self.request.step_accepted()
    }

    fn accept(self) -> Result<(), CommandError> {
        self.request.accept().map(|_| ())
    }

    fn reject(self, error: CommandError) {
        self.request.reject(error);
    }
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
        .unwrap()
        .clone();
    let operation = Operation {
        binding: bindings.power(),
        target: CommandTarget::Fan(request.command),
        queued_at: request.queued_at(),
    };
    let sequence = FanSequence {
        request,
        bindings,
        work: work.clone(),
        id: id.into(),
    };
    steps::send(sequence, context, work, sink, id, gate, operation).await
}
