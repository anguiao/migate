use std::{cell::RefCell, collections::VecDeque, rc::Rc, time::Instant};

use serde_json::Value;

use super::{
    DeviceWriteGate,
    properties::{Batch, BatchAttempt, alive, submit_properties},
};
use crate::{
    device::CommandError,
    xiaomi::{
        adapter::{
            model::{AdapterError, CommandTarget},
            state::{Context, LogicalSink},
            work::DeviceWork,
        },
        capability::PropertyBinding,
    },
};

type Property = (PropertyBinding, Value);

/// A function owns its encoding and validity; the sender owns transport and receipts.
pub(super) trait StepRequest: Sized + 'static {
    type Step: Copy + 'static;

    fn steps(&self) -> Result<VecDeque<(Self::Step, Property)>, CommandError>;
    fn current_result(&self) -> Result<(), CommandError>;
    fn first_step_current(&self, _step: Self::Step) -> bool {
        true
    }
    fn mark_sent(&self, step: Self::Step, binding: PropertyBinding) -> Result<(), CommandError>;
    fn step_accepted(&self, step: Self::Step) -> Result<(), CommandError>;
    fn accept(self) -> Result<(), CommandError>;
    fn reject(self, error: CommandError);
}

pub(super) struct Operation {
    /// Used for diagnostics when planning fails before the first property exists.
    pub(super) binding: PropertyBinding,
    pub(super) target: CommandTarget,
    pub(super) queued_at: Instant,
}

/// Execute one operation in order, re-planning only an unsent first step.
pub(super) async fn send<R: StepRequest>(
    request: R,
    context: Rc<Context>,
    work: Rc<DeviceWork>,
    sink: Rc<LogicalSink>,
    id: &str,
    gate: Rc<DeviceWriteGate>,
    info: Operation,
) -> Result<(), AdapterError> {
    let planned = request.steps();
    let binding = planned
        .as_ref()
        .ok()
        .and_then(|steps| steps.front())
        .map_or(info.binding, |(_, property)| property.0);
    let operation = context.commands.begin_operation(
        &work.key.id,
        id,
        binding,
        info.target,
        Vec::new(),
        info.queued_at,
    );
    let mut steps = match planned {
        Ok(steps) => steps,
        Err(error) => {
            request.reject(error.clone());
            operation.0.finish(Err(error));
            return Ok(());
        }
    };
    let pending = Rc::new(RefCell::new(Some(request)));
    let current: Rc<dyn Fn() -> Result<(), CommandError>> = {
        let pending = pending.clone();
        Rc::new(move || pending.borrow().as_ref().unwrap().current_result())
    };
    let mut first_step = true;
    while let Some((step, property)) = steps.pop_front() {
        operation.0.pending_step(property.clone());
        let mark: Rc<dyn Fn() -> Result<(), CommandError>> = {
            let pending = pending.clone();
            let binding = property.0;
            Rc::new(move || pending.borrow().as_ref().unwrap().mark_sent(step, binding))
        };
        let frame_current: Option<Rc<dyn Fn() -> bool>> = if first_step {
            let pending = pending.clone();
            Some(Rc::new(move || {
                pending.borrow().as_ref().unwrap().first_step_current(step)
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
                        let planned = pending.borrow().as_ref().unwrap().steps();
                        match planned {
                            Ok(planned) => steps = planned,
                            Err(error) => {
                                pending.borrow_mut().take().unwrap().reject(error.clone());
                                operation.0.finish(Err(error));
                                return Ok(());
                            }
                        }
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
                        request.step_accepted(step)
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
    let result = pending.borrow_mut().take().unwrap().accept();
    operation.0.finish(result);
    Ok(())
}
