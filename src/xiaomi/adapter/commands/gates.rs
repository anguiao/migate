use super::feedback::LightFeedback;
use crate::xiaomi::{adapter::bootstrap::Clock, capability::PropertyBinding};
use event_listener::Event;
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
    sync::Arc,
};

/// Receipts and recent numeric feedback retain their gate across device replacements.
#[derive(Default)]
pub(in crate::xiaomi::adapter) struct DeviceWriteGates {
    pub(super) gates: RefCell<BTreeMap<String, Rc<DeviceWriteGate>>>,
}
impl DeviceWriteGates {
    pub(in crate::xiaomi::adapter) fn for_device(
        &self,
        did: &str,
        clock: Arc<dyn Clock>,
    ) -> Rc<DeviceWriteGate> {
        let mut gates = self.gates.borrow_mut();
        gates.retain(|_, gate| {
            Rc::strong_count(gate) > 1 || !gate.feedback.borrow_mut().is_empty(clock.now())
        });
        gates
            .entry(did.into())
            .or_insert_with(|| Rc::new(DeviceWriteGate::new(clock)))
            .clone()
    }
    /// A binding-authority replacement retires every sent/history
    /// entry so a late old receipt cannot reintroduce feedback.
    pub(in crate::xiaomi::adapter) fn clear_authority(&self, did: &str) {
        if let Some(gate) = self.gates.borrow().get(did) {
            gate.clear_authority();
        }
    }
}
pub(in crate::xiaomi::adapter) struct DeviceWriteGate {
    pub(super) busy: Cell<bool>,
    pub(super) changed: Event,
    pub(super) clock: Arc<dyn Clock>,
    pub(super) feedback: RefCell<LightFeedback>,
    /// Binding-authority epoch; a replacement retires all old feedback.
    pub(super) authority: Cell<u64>,
}
impl DeviceWriteGate {
    pub(super) fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            busy: Cell::new(false),
            changed: Event::new(),
            clock,
            feedback: RefCell::default(),
            authority: Cell::new(0),
        }
    }
    pub(super) async fn acquire(self: &Rc<Self>) -> DeviceWritePermit {
        loop {
            let changed = self.changed.listen();
            if !self.busy.replace(true) {
                return DeviceWritePermit(self.clone());
            }
            changed.await;
        }
    }
    pub(in crate::xiaomi::adapter) fn authority(&self) -> u64 {
        self.authority.get()
    }
    pub(super) fn clear_authority(&self) {
        self.feedback.borrow_mut().clear_all();
        self.authority.set(self.authority.get() + 1);
    }
    pub(super) fn register_feedback_for(
        &self,
        authority: u64,
        properties: &[(PropertyBinding, serde_json::Value)],
    ) {
        if authority == self.authority.get() {
            self.feedback.borrow_mut().sent(properties);
        }
    }
    pub(super) fn finish_feedback_for<E>(
        &self,
        authority: u64,
        properties: &[(PropertyBinding, serde_json::Value)],
        results: &crate::xiaomi::property::PropertyResults<(), E>,
        retain_feedback: bool,
    ) {
        if !retain_feedback {
            return;
        }
        if authority != self.authority.get() {
            // Consume the late receipt without writing it into the new authority.
            self.feedback.borrow_mut().cancel();
            return;
        }
        let accepted = properties
            .iter()
            .zip(&results.items)
            .filter_map(|((binding, _), result)| result.is_ok().then_some(*binding));
        self.feedback
            .borrow_mut()
            .complete(accepted, self.clock.now());
    }
    pub(super) fn cancel_feedback(&self) {
        self.feedback.borrow_mut().cancel();
    }
    pub(in crate::xiaomi::adapter) fn is_feedback(
        &self,
        binding: PropertyBinding,
        value: &serde_json::Value,
    ) -> bool {
        self.feedback
            .borrow_mut()
            .matches(binding, value, self.clock.now())
    }
}
pub(super) struct DeviceWritePermit(pub(super) Rc<DeviceWriteGate>);
impl Drop for DeviceWritePermit {
    fn drop(&mut self) {
        self.0.cancel_feedback();
        self.0.busy.set(false);
        self.0.changed.notify(usize::MAX);
    }
}
