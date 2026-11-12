use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    rc::Rc,
    time::Instant,
};

use serde_json::json;

use super::{
    DeviceWriteGate,
    steps::{self, Operation, StepRequest, StepTarget},
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

/// On a shared-speed fan the native power-on write also selects high speed.
#[derive(Default)]
pub(in crate::xiaomi::adapter) struct SharedFanPower {
    running: bool,
    needs_read: bool,
    awaiting_low_power: bool,
    low_receipt: Option<(Instant, Result<(), CommandError>)>,
}

enum PowerOnPlan {
    Write,
    ReadPower,
    Settle(Result<(), CommandError>),
}

impl SharedFanPower {
    pub(in crate::xiaomi::adapter) fn observe(&mut self, running: bool) {
        self.running = running;
        self.needs_read = false;
        // A read may still return off while the low-speed request is in flight.
        if running || self.low_receipt.is_some() {
            self.awaiting_low_power = false;
        }
    }

    pub(in crate::xiaomi::adapter) fn require_read(&mut self) {
        self.needs_read = true;
        self.awaiting_low_power = false;
        self.low_receipt = None;
    }

    fn plan_on(&self, queued_at: Instant, unsent_low_failure: Option<CommandError>) -> PowerOnPlan {
        if let Some(error) = unsent_low_failure {
            // A cancelled start also cancels its follower. A fan already running
            // needs no write when a queued speed change is cancelled.
            return PowerOnPlan::Settle(if self.running && !self.needs_read {
                Ok(())
            } else {
                Err(error)
            });
        }
        // A queued On shares the actual outcome of a low operation it overlapped.
        // After settlement only an outstanding power confirmation can extend it.
        if let Some((settled_at, result)) = &self.low_receipt
            && (self.awaiting_low_power || queued_at <= *settled_at)
        {
            return PowerOnPlan::Settle(result.clone());
        }
        if self.needs_read {
            PowerOnPlan::ReadPower
        } else if self.running {
            PowerOnPlan::Settle(Ok(()))
        } else {
            PowerOnPlan::Write
        }
    }
}

fn encode(
    command: FanCommand,
    bindings: &FanBindings,
    needs_power_on: bool,
) -> VecDeque<(FanStep, StepTarget)> {
    let mut steps = VecDeque::new();
    match command {
        FanCommand::Power(value) => steps.push_back((
            FanStep::Power(value),
            StepTarget::property(bindings.power(), json!(value)),
        )),
        FanCommand::Speed(0) => steps.push_back((
            FanStep::Power(false),
            StepTarget::property(bindings.power(), json!(false)),
        )),
        FanCommand::Speed(value) => {
            if let Some(shared) = bindings.shared_speed() {
                if value == 1 {
                    // Low speed is a single native operation and never
                    // pre-powers the fan through the high-speed boolean.
                    steps.push_back((
                        FanStep::Speed(1),
                        StepTarget::LowSpeed {
                            anchor: bindings.power(),
                            speed: shared,
                        },
                    ));
                } else {
                    // Explicit high speed must write even when the fan is running.
                    steps.push_back((
                        FanStep::Power(true),
                        StepTarget::property(bindings.power(), json!(true)),
                    ));
                }
                return steps;
            }
            if needs_power_on {
                steps.push_back((
                    FanStep::Power(true),
                    StepTarget::property(bindings.power(), json!(true)),
                ));
            }
            if let Some(mode) = bindings.mode() {
                steps.push_back((
                    FanStep::Auxiliary,
                    StepTarget::property(mode.property(), json!(mode.straight())),
                ));
            }
            steps.push_back((
                FanStep::Speed(value),
                StepTarget::property(
                    bindings.speed().property(),
                    json!(bindings.speed().encode(value)),
                ),
            ));
        }
        FanCommand::Swing(value) => steps.push_back((
            FanStep::Swing(value),
            StepTarget::property(bindings.swing().unwrap(), json!(value)),
        )),
    }
    steps
}

struct FanSequence {
    request: FanRequest,
    bindings: FanBindings,
    work: Rc<DeviceWork>,
    id: String,
    shared_power: Option<Rc<RefCell<SharedFanPower>>>,
    unchanged: Cell<bool>,
    starting_low: Cell<bool>,
}
impl FanSequence {
    fn power_on_plan(&self) -> PowerOnPlan {
        if self.request.command == FanCommand::Power(true)
            && let Some(power) = &self.shared_power
        {
            power
                .borrow()
                .plan_on(self.request.queued_at(), self.request.unsent_low_failure())
        } else {
            PowerOnPlan::Write
        }
    }
}
impl StepRequest for FanSequence {
    type Step = FanStep;

    async fn steps(
        &self,
        context: &Context,
        sink: &LogicalSink,
    ) -> Result<VecDeque<(FanStep, StepTarget)>, CommandError> {
        let mut plan = self.power_on_plan();
        if matches!(plan, PowerOnPlan::ReadPower)
            && context.read_path_for(&self.work.key.id, &self.id).is_some()
        {
            self.work
                .request_dirty_read(&self.id, self.bindings.power());
        }
        while matches!(plan, PowerOnPlan::ReadPower) {
            let changed = self.work.refresh_changed.listen();
            self.request.current_result()?;
            if self.work.life.is_closed() || sink.life.is_closed() {
                return Err(CommandError::Cancelled);
            }
            if context.read_path_for(&self.work.key.id, &self.id).is_none()
                || !self
                    .work
                    .refresh
                    .borrow()
                    .contains(&self.id, self.bindings.power())
            {
                return Err(CommandError::UnknownState);
            }
            futures_lite::future::or(
                changed,
                futures_lite::future::or(self.work.life.ended(), sink.life.ended()),
            )
            .await;
            plan = self.power_on_plan();
        }
        if let PowerOnPlan::Settle(result) = plan {
            self.unchanged.set(true);
            return result.map(|()| VecDeque::new());
        }
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
        if self.shared_power.is_some() {
            return matches!(self.power_on_plan(), PowerOnPlan::Write);
        }
        !matches!(self.request.command, FanCommand::Speed(1..=100))
            || self.request.needs_power_on() == (step == FanStep::Power(true))
    }

    fn mark_sent(&self, step: FanStep, binding: PropertyBinding) -> Result<(), CommandError> {
        self.request.mark_sent(step)?;
        if let Some(power) = &self.shared_power {
            let mut power = power.borrow_mut();
            power.low_receipt = None;
            let start = step == FanStep::Speed(1) && (!power.running || power.needs_read);
            self.starting_low.set(start);
            power.awaiting_low_power = start;
        }
        if step == FanStep::Power(false) || self.shared_power.is_some() {
            self.work.observations.invalidate_write(&self.id, binding);
        }
        Ok(())
    }

    fn step_accepted(&self, step: FanStep) -> Result<(), CommandError> {
        if step == FanStep::Power(false)
            && let Some(power) = &self.shared_power
        {
            let mut power = power.borrow_mut();
            power.running = false;
            power.needs_read = false;
        }
        self.request.step_accepted()
    }

    fn accept(self) -> Result<(), CommandError> {
        let low = self.starting_low.get();
        let result = if self.unchanged.get() {
            self.request.accept_unchanged()
        } else {
            self.request.accept()
        }
        .map(|_| ());
        if low && let Some(power) = self.shared_power {
            power.borrow_mut().low_receipt = Some((Instant::now(), result.clone()));
        }
        result
    }

    fn reject(self, error: CommandError) {
        if self.starting_low.get()
            && let Some(power) = self.shared_power
        {
            power.borrow_mut().low_receipt = Some((Instant::now(), Err(error.clone())));
        }
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
    let shared_power = bindings.shared_speed().map(|_| work.shared_fan_power(id));
    let sequence = FanSequence {
        request,
        bindings,
        work: work.clone(),
        id: id.into(),
        shared_power,
        unchanged: Cell::new(false),
        starting_low: Cell::new(false),
    };
    steps::send(sequence, context, work, sink, id, gate, operation).await
}
