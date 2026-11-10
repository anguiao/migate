use super::{CommandOperation, PathKind, StepReceipt};
use crate::xiaomi::adapter::{
    state::{Context, LogicalSink},
    work::DeviceWork,
};
use std::rc::Rc;

pub(super) fn notifications_ready(context: &Context, work: &DeviceWork, id: &str) -> bool {
    work.function_observer(id).is_some_and(|source| {
        context.source_valid_for(&work.key.id, id, &source) && work.source_ready(id, &source)
    })
}

impl CommandOperation {
    pub(super) fn schedule_feedback(
        &self,
        context: &Context,
        work: &Rc<DeviceWork>,
        sink: &LogicalSink,
    ) {
        let target = self.status.borrow().target.clone();
        let scope = work.related_scope_for_command(&self.channel, &target);
        let status = self.status.borrow();
        let accepted = status
            .steps
            .iter()
            .any(|step| matches!(step.receipt, StepReceipt::Accepted));
        let uncertain = status
            .steps
            .iter()
            .any(|step| matches!(step.receipt, StepReceipt::Uncertain(_)));
        if !(accepted || scope.is_some() && uncertain) {
            return;
        }
        let direct = status.steps.iter().any(|step| {
            step.path == PathKind::Direct && matches!(step.receipt, StepReceipt::Accepted)
        });
        drop(status);

        let current = !work.life.is_closed()
            && !sink.life.is_closed()
            && context
                .live
                .borrow()
                .devices
                .get(&work.key.id)
                .and_then(|slot| slot.admitted.as_ref())
                .is_some_and(|current| Rc::ptr_eq(current, work));
        if !current {
            return;
        }
        if let Some(scope) = scope {
            work.refresh_related_inputs(scope);
            return;
        }
        if !direct && notifications_ready(context, work, &self.channel) {
            return;
        }

        let function = work
            .native
            .device()
            .functions()
            .iter()
            .find(|function| function.id() == self.channel)
            .unwrap();
        for binding in function.observation_bindings() {
            work.observations.invalidate_write(&self.channel, binding);
            work.request_dirty_read(&self.channel, binding);
        }
    }
}
