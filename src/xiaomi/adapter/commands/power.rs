use super::properties::{Batch, BatchAttempt, ReceiptHandler, alive, submit_properties};
use crate::{
    device::{CommandError, CommandRequest},
    xiaomi::adapter::{
        model::{AdapterError, PathKind},
        state::{Context, LogicalSink},
        work::DeviceWork,
    },
};
use std::{cell::RefCell, rc::Rc};

/// Home authority and relay mode are resolved once when MiGate issues the
/// command. The transport still checks the selected connection and runtime.
fn relay_allowed(
    context: &Rc<Context>,
    work: &DeviceWork,
    id: &str,
    request: &CommandRequest,
) -> bool {
    let Some((uid, home_id)) = request.relay_home() else {
        return true;
    };
    let scope = crate::configuration::Scope {
        uid: uid.to_owned(),
        home_id: home_id.to_owned(),
    };
    let key = crate::device::FunctionKey::new(work.key.clone(), id);
    super::super::publication::SourceHandle(context.clone()).relay_accessible(&scope, &key)
}

pub(super) async fn send(
    request: CommandRequest,
    context: Rc<Context>,
    work: Rc<DeviceWork>,
    sink: Rc<LogicalSink>,
    id: &str,
) -> Result<(), AdapterError> {
    let did = &work.key.id;
    let binding = work.binding(id);
    let target = request.target;
    let operation = context
        .commands
        .begin(did, id, binding, target, request.queued_at());
    let initial = if !alive(&work, &sink) {
        Err(CommandError::Cancelled)
    } else {
        request.current_result()
    };
    if let Err(error) = initial {
        operation.0.reject(request, error);
        return Ok(());
    }
    if !relay_allowed(&context, &work, id, &request) {
        operation.0.reject(
            request,
            CommandError::Unavailable("relay is not currently available to Home".into()),
        );
        return Ok(());
    }

    let pending = Rc::new(RefCell::new(Some(request)));
    let stamp = Rc::new(RefCell::new(None));
    let mark: Rc<dyn Fn() -> Result<(), CommandError>> = {
        let pending = pending.clone();
        let work = work.clone();
        let id = id.to_owned();
        let stamp = stamp.clone();
        Rc::new(move || {
            pending.borrow().as_ref().unwrap().mark_sent()?;
            work.invalidate_shared_read_for_function(&id);
            let read_pending = work.refresh.borrow().contains(&id, binding);
            *stamp.borrow_mut() = Some(work.observations.write_stamp(&id, binding, read_pending));
            Ok(())
        })
    };
    let current: Rc<dyn Fn() -> Result<(), CommandError>> = {
        let pending = pending.clone();
        Rc::new(move || pending.borrow().as_ref().unwrap().current_result())
    };
    let on_receipt: Rc<ReceiptHandler> = {
        let pending = pending.clone();
        let context = context.clone();
        let work = work.clone();
        let sink = sink.clone();
        let id = id.to_owned();
        let operation = operation.0.clone();
        Rc::new(move |result, observation_current| {
            let request = pending.borrow_mut().take().unwrap();
            let result = match result {
                Ok(()) if observation_current => {
                    let direct = operation.status.borrow().actual_path == Some(PathKind::Direct);
                    work.observations.accept_write(
                        &work,
                        &sink,
                        &id,
                        &stamp.borrow_mut().take().unwrap(),
                        direct || !super::refresh::notifications_ready(&context, &work, &id),
                        request,
                    )
                }
                Ok(()) => request.accept().map(|_| ()),
                Err(error) => {
                    request.reject(error.clone());
                    Err(error)
                }
            };
            if observation_current || result.is_err() {
                operation.schedule_feedback(&context, &work, &sink);
            }
            operation.finish(result.clone());
            result
        })
    };
    loop {
        match submit_properties(
            &context,
            &work,
            &sink,
            Batch {
                function_id: id,
                properties: vec![(binding, serde_json::Value::Bool(target))],
                retain_feedback: false,
                after_write: None,
                gate: None,
                operation: &operation.0,
                mark: &mark,
                current: &current,
                frame_current: None,
                on_receipt: Some(&on_receipt),
            },
        )
        .await
        {
            BatchAttempt::Withdrawn => continue,
            BatchAttempt::Settled(receipt) => return receipt.maintenance.map_or(Ok(()), Err),
        }
    }
}
