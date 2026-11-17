use super::{bootstrap::Clock, model::Ready, tasks::TaskSet};
use crate::{
    configuration::{CommittedConfiguration, DerivedConfiguration},
    storage::Store,
    xiaomi::mijia::MijiaClient,
};
use event_listener::Event;
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::Arc,
    time::Duration,
};
#[derive(Default)]
pub(super) struct Lifetime {
    closed: Cell<bool>,
    changed: Event,
}
impl Lifetime {
    pub(super) fn close(&self) {
        self.closed.set(true);
        self.changed.notify(usize::MAX);
    }
    pub(super) fn is_closed(&self) -> bool {
        self.closed.get()
    }
    pub(super) async fn ended(&self) {
        loop {
            let listener = self.changed.listen();
            if self.closed.get() {
                return;
            }
            listener.await;
        }
    }
}
pub(super) use super::work::LogicalSink;
pub(super) struct Context {
    pub(super) commands: Rc<super::commands::Commands>,
    pub(super) write_gates: super::commands::DeviceWriteGates,
    pub(super) notifications: RefCell<Option<Rc<super::notifications::CloudSession>>>,
    pub(super) credentials_changed: Event,
    pub(super) mijia: MijiaClient,
    pub(super) network: Rc<dyn super::network::Network>,
    pub(super) lan_discovery: crate::xiaomi::lan::Discovery,
    pub(super) lan_scope_invalid: Cell<bool>,
    pub(super) lan_network_changed: Event,
    pub(super) ha_changed: Event,
    pub(super) mijia_changed: Event,
    pub(super) refresh_changed: Rc<Event>,
    pub(super) initial_reads_changed: Rc<Event>,
    pub(super) credentials: RefCell<super::credentials::Credentials>,
    pub(super) directory: RefCell<super::directory::Directory>,
    pub(super) hub: RefCell<super::hub::Hub>,
    /// Shared application input; runtime bindings remain on each DeviceWork.
    pub(super) configuration: RefCell<Rc<CommittedConfiguration>>,
    /// The one committed group graph installed with the serving configuration.
    pub(super) graph: RefCell<Rc<DerivedConfiguration>>,
    pub(super) store: Store,
    pub(super) tasks: TaskSet,
    pub(super) configuration_changed: Rc<Event>,
    pub(super) changed: Event,
    pub(super) routing_history: RefCell<super::routing::PriorityHistory>,
    pub(super) status_changed: Event,
    pub(super) ready: Rc<Ready>,
    pub(super) clock: Arc<dyn Clock>,
    pub(super) sync_requested: Cell<bool>,
    pub(super) sync_changed: Event,
}
pub(super) struct Backoff {
    delay: Duration,
    maximum: Duration,
}
impl Backoff {
    pub(super) fn new(minimum_secs: u64, maximum_secs: u64) -> Self {
        Self {
            delay: Duration::from_secs(minimum_secs),
            maximum: Duration::from_secs(maximum_secs),
        }
    }
    pub(super) fn failed(&mut self) -> Duration {
        let delay = self.delay;
        self.delay = (self.delay * 2).min(self.maximum);
        delay
    }
}
