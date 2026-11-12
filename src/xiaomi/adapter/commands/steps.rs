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
        capability::{PropertyBinding, SharedFanSpeedBinding},
        cloud::CloudError,
    },
};

type Property = (PropertyBinding, Value);

/// One real native operation. Shared low speed carries both path encodings and
/// resolves exactly one after the path is selected; actions and legacy commands
/// never become synthetic PropertyBindings.
pub(super) enum StepTarget {
    Property(Property),
    LowSpeed {
        anchor: PropertyBinding,
        speed: SharedFanSpeedBinding,
    },
}
impl StepTarget {
    pub(super) fn property(binding: PropertyBinding, value: Value) -> Self {
        Self::Property((binding, value))
    }
    fn anchor(&self) -> PropertyBinding {
        match self {
            Self::Property((binding, _)) => *binding,
            Self::LowSpeed { anchor, .. } => *anchor,
        }
    }
    fn pending(&self) -> (PropertyBinding, Value, Option<String>) {
        match self {
            Self::Property((binding, value)) => (*binding, value.clone(), None),
            Self::LowSpeed { anchor, speed } => {
                let (method, params) = speed.channel.direct_low_speed();
                let native = match speed.cloud_action {
                    Some(action) => format!(
                        "cloud action {}.{} or direct {method} {params}",
                        action.siid, action.aiid
                    ),
                    None => format!("direct {method} {params}"),
                };
                (*anchor, Value::Null, Some(format!("low-speed {native}")))
            }
        }
    }
}

/// A function owns its encoding and validity; the sender owns transport and receipts.
pub(super) trait StepRequest: Sized + 'static {
    type Step: Copy + 'static;

    async fn steps(
        &self,
        context: &Context,
        sink: &LogicalSink,
    ) -> Result<VecDeque<(Self::Step, StepTarget)>, CommandError>;
    /// Check operation ownership, including cancellation after a step was sent.
    fn current_result(&self) -> Result<(), CommandError>;
    /// Check whether this step may be sent now, without changing receipt settlement.
    fn send_result(&self, _step: Self::Step) -> Result<(), CommandError> {
        self.current_result()
    }
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
    let planned = request.steps(&context, &sink).await;
    let binding = planned
        .as_ref()
        .ok()
        .and_then(|steps| steps.front())
        .map_or(info.binding, |(_, target)| target.anchor());
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
    let mut first_step = true;
    while let Some((step, property)) = steps.pop_front() {
        let (pending_binding, pending_value, pending_native) = property.pending();
        operation
            .0
            .pending_step(pending_binding, pending_value, pending_native);
        let current: Rc<dyn Fn() -> Result<(), CommandError>> = {
            let pending = pending.clone();
            Rc::new(move || pending.borrow().as_ref().unwrap().send_result(step))
        };
        let mark: Rc<dyn Fn() -> Result<(), CommandError>> = {
            let pending = pending.clone();
            let binding = property.anchor();
            let work = work.clone();
            let id = id.to_owned();
            Rc::new(move || {
                pending
                    .borrow()
                    .as_ref()
                    .unwrap()
                    .mark_sent(step, binding)?;
                work.invalidate_shared_read_for_function(&id);
                Ok(())
            })
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
            let attempt = match &property {
                StepTarget::Property((binding, value)) => {
                    submit_properties(
                        &context,
                        &work,
                        &sink,
                        Batch {
                            function_id: id,
                            properties: vec![(*binding, value.clone())],
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
                }
                StepTarget::LowSpeed { speed, .. } => {
                    submit_low_speed(
                        &context,
                        &work,
                        &sink,
                        id,
                        speed,
                        &operation.0,
                        &mark,
                        &current,
                    )
                    .await
                }
            };
            match attempt {
                BatchAttempt::Withdrawn => {
                    if frame_current.as_ref().is_some_and(|current| !current()) {
                        let request = pending.borrow_mut().take().unwrap();
                        let planned = request.steps(&context, &sink).await;
                        pending.borrow_mut().replace(request);
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
                        operation.0.schedule_feedback(&context, &work, &sink);
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
                        operation.0.schedule_feedback(&context, &work, &sink);
                        operation.0.finish(Err(CommandError::Cancelled));
                        return Err(error);
                    }
                    first_step = false;
                    break;
                }
            }
        }
    }
    let request = pending.borrow_mut().take().unwrap();
    let result = if alive(&work, &sink) {
        request.accept()
    } else {
        request.reject(CommandError::Cancelled);
        Err(CommandError::Cancelled)
    };
    operation.0.schedule_feedback(&context, &work, &sink);
    operation.0.finish(result);
    Ok(())
}

/// One accepted low-speed action/command. High/off remain ordinary boolean
/// property writes on the standard path.
#[allow(clippy::too_many_arguments)]
async fn submit_low_speed(
    context: &Rc<Context>,
    work: &Rc<DeviceWork>,
    sink: &Rc<LogicalSink>,
    function_id: &str,
    speed: &SharedFanSpeedBinding,
    operation: &Rc<super::CommandOperation>,
    mark: &Rc<dyn Fn() -> Result<(), CommandError>>,
    current: &Rc<dyn Fn() -> Result<(), CommandError>>,
) -> BatchAttempt {
    use super::properties::{BatchAttempt, BatchReceipt, cloud_command_error, cloud_receipt};
    use crate::xiaomi::adapter::model::{PathKind, StepReceipt};
    let did = &work.key.id;
    if let Err(error) = current() {
        return BatchAttempt::Settled(BatchReceipt {
            result: Err(error),
            maintenance: None,
        });
    }
    if !alive(work, sink) {
        return BatchAttempt::Settled(BatchReceipt {
            result: Err(CommandError::Cancelled),
            maintenance: None,
        });
    }
    let Some(path) = context.low_speed_path(did, function_id) else {
        return BatchAttempt::Settled(BatchReceipt {
            result: Err(CommandError::Unavailable(
                "no usable low-speed control path".into(),
            )),
            maintenance: None,
        });
    };
    if let crate::xiaomi::adapter::routing::ControlPath::Direct(client) = &path {
        use super::properties::{direct_receipt, settle_direct_failure};
        use crate::xiaomi::adapter::model::PathKind;
        if !context
            .low_speed_path(did, function_id)
            .is_some_and(|selected| {
                matches!(
                    selected,
                    crate::xiaomi::adapter::routing::ControlPath::Direct(selected)
                        if selected.same(client)
                )
            })
        {
            return BatchAttempt::Settled(BatchReceipt {
                result: Err(CommandError::Unavailable("direct path changed".into())),
                maintenance: None,
            });
        }
        let guard = || {
            alive(work, sink)
                && current().is_ok()
                && context
                    .low_speed_path(did, function_id)
                    .is_some_and(|selected| {
                        matches!(
                            selected,
                            crate::xiaomi::adapter::routing::ControlPath::Direct(selected)
                                if selected.same(client)
                        )
                    })
        };
        let on_sent = || {
            let _ = mark();
            operation.submitted(PathKind::Direct);
        };
        use super::super::legacy_miio::CommandError as LegacyCommandError;
        let (method, params) = speed.channel.direct_low_speed();
        let result =
            super::super::legacy_miio::command(client, method, params, guard, on_sent).await;
        match result {
            Ok(()) => {
                operation.receipts(vec![crate::xiaomi::adapter::StepReceipt::Accepted]);
                return BatchAttempt::Settled(BatchReceipt {
                    result: Ok(()),
                    maintenance: None,
                });
            }
            Err(LegacyCommandError::Unexpected) => {
                operation.receipts(vec![crate::xiaomi::adapter::StepReceipt::Uncertain(
                    "unexpected direct command response".into(),
                )]);
                return BatchAttempt::Settled(BatchReceipt {
                    result: Err(CommandError::SendFailed(
                        "unexpected direct command response".into(),
                    )),
                    maintenance: None,
                });
            }
            Err(LegacyCommandError::Request(error)) => {
                if error.stage == crate::xiaomi::lan::SendStage::BeforeSend
                    && error.failure == crate::xiaomi::lan::Failure::Withdrawn
                {
                    return BatchAttempt::Withdrawn;
                }
                let sent = error.stage != crate::xiaomi::lan::SendStage::BeforeSend;
                if sent {
                    operation.receipts(vec![direct_receipt(&error)]);
                }
                return settle_direct_failure(context, did, client, &error);
            }
        }
    }
    let crate::xiaomi::adapter::routing::ControlPath::Cloud(credentials) = path else {
        return BatchAttempt::Settled(BatchReceipt {
            result: Err(CommandError::Unavailable(
                "low-speed path is unavailable".into(),
            )),
            maintenance: None,
        });
    };
    let Some(action) = speed.cloud_action else {
        return BatchAttempt::Settled(BatchReceipt {
            result: Err(CommandError::Unavailable(
                "cloud low-speed action is unavailable".into(),
            )),
            maintenance: None,
        });
    };
    if !context.current_mijia(&credentials) {
        return BatchAttempt::Settled(BatchReceipt {
            result: Err(CommandError::Unavailable("Mijia session changed".into())),
            maintenance: None,
        });
    }
    if let Err(error) = mark() {
        return BatchAttempt::Settled(BatchReceipt {
            result: Err(error),
            maintenance: None,
        });
    }
    operation.submitted(PathKind::Cloud);
    let result = context
        .mijia
        .action(
            &credentials.session,
            did,
            action.siid,
            action.aiid,
            Vec::new(),
        )
        .await;
    let receipt = match &result {
        Ok(action) if action.code == 0 => {
            operation.receipts(vec![StepReceipt::Accepted]);
            BatchReceipt {
                result: Ok(()),
                maintenance: None,
            }
        }
        Ok(action) => {
            let error = CloudError::new(
                "invoke Xiaomi action",
                crate::xiaomi::cloud::CloudErrorKind::Business(action.code),
            );
            operation.receipts(vec![cloud_receipt(&error)]);
            context.cloud_error(
                &credentials,
                alive(work, sink).then_some(did.as_str()),
                &error,
            );
            BatchReceipt {
                result: Err(CommandError::Rejected(format!(
                    "cloud code {}",
                    action.code
                ))),
                maintenance: None,
            }
        }
        Err(error) => {
            operation.receipts(vec![cloud_receipt(error)]);
            context.cloud_error(
                &credentials,
                alive(work, sink).then_some(did.as_str()),
                error,
            );
            BatchReceipt {
                result: Err(cloud_command_error(error)),
                maintenance: None,
            }
        }
    };
    BatchAttempt::Settled(receipt)
}
