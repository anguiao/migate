//! The current gateway connection and the access facts obtained from it.

use super::{bootstrap::Clock, model::GatewayStatus, routing::PathPriority, tasks::TaskSet};
use crate::xiaomi::central::{
    CentralError, DeviceAccess, DeviceEntry, RequestTimeout, SessionHandle,
};
use event_listener::Event;
use futures_lite::future;
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet},
    rc::Rc,
    time::Duration,
};

pub(super) struct Hub {
    session: Option<Session>,
    access: BTreeMap<String, Result<DeviceAccess, CentralError>>,
    priority: PathPriority,
    problem: Option<String>,
    gateway: GatewayStatus,
}

impl Default for Hub {
    fn default() -> Self {
        Self {
            session: None,
            access: BTreeMap::new(),
            priority: PathPriority::LocalFirst,
            problem: None,
            gateway: GatewayStatus::Discovering,
        }
    }
}

impl Hub {
    pub(super) fn session(&self) -> Option<&Session> {
        self.session.as_ref()
    }

    pub(super) fn install(&mut self, session: Session) {
        self.session = Some(session);
    }

    pub(super) fn priority(&self) -> PathPriority {
        self.priority
    }

    pub(super) fn degrade(&mut self) -> bool {
        let changed = self.priority != PathPriority::CloudFirst;
        self.priority = PathPriority::CloudFirst;
        changed
    }

    pub(super) fn restore_priority(&mut self) -> bool {
        let changed = self.priority != PathPriority::LocalFirst;
        self.priority = PathPriority::LocalFirst;
        changed
    }

    pub(super) fn disconnect(&mut self) -> (Option<Session>, bool) {
        let changed = self.degrade();
        self.access.clear();
        (self.session.take(), changed)
    }

    pub(super) fn access(&self, did: &str) -> Option<&Result<DeviceAccess, CentralError>> {
        self.access.get(did)
    }

    pub(super) fn observe_access(&mut self, dids: &[String], entries: Vec<DeviceEntry>) {
        let mut entries = entries
            .into_iter()
            .map(|entry| (entry.did, entry.access))
            .collect::<BTreeMap<_, _>>();
        for did in dids {
            if let Some(access) = entries.remove(did) {
                self.access.insert(did.clone(), access);
            } else {
                self.access.remove(did);
            }
        }
    }

    pub(super) fn device_offline(&mut self, did: &str) {
        if let Some(Ok(access)) = self.access.get_mut(did) {
            access.online = false;
        }
    }

    pub(super) fn forget_device(&mut self, did: &str) {
        self.access.remove(did);
    }

    pub(super) fn gateway(&self) -> &GatewayStatus {
        &self.gateway
    }

    pub(super) fn gateway_changed(&mut self, gateway: GatewayStatus) -> bool {
        if self.gateway == gateway {
            return false;
        }
        self.gateway = gateway;
        true
    }

    pub(super) fn problem(&self) -> Option<&str> {
        self.problem.as_deref()
    }

    pub(super) fn access_result(&mut self, problem: Option<String>) -> bool {
        if self.problem == problem {
            return false;
        }
        self.problem = problem;
        true
    }
}

pub(super) struct Session {
    pub(super) handle: SessionHandle,
    pub(super) access: Rc<AccessRefresh>,
    pub(super) tasks: Rc<TaskSet>,
}
impl Session {
    pub(super) fn new(handle: SessionHandle) -> Self {
        Self {
            handle,
            access: Rc::new(AccessRefresh::default()),
            tasks: Rc::new(TaskSet::default()),
        }
    }
}
#[derive(Default)]
pub(super) struct AccessRefresh {
    full_deadline: Cell<Option<Duration>>,
    pending: RefCell<BTreeSet<String>>,
    changed: Event,
}
pub(super) enum AccessRequest {
    Full,
    Filtered(Vec<String>),
}
impl AccessRefresh {
    pub(super) fn request_initial(&self, now: Duration) {
        self.full_deadline.set(Some(now));
        self.changed.notify(usize::MAX);
    }
    pub(super) fn request(&self, now: Duration) {
        if self.full_deadline.get().is_none() {
            self.full_deadline.set(Some(now + Duration::from_secs(3)));
        }
        self.changed.notify(usize::MAX);
    }
    pub(super) fn retry_full(&self, now: Duration, timeout: Option<RequestTimeout>) {
        // A stalled directory stays low-frequency without degrading the hub session.
        let delay = if timeout.is_some() { 60 } else { 3 };
        self.full_deadline
            .set(Some(now + Duration::from_secs(delay)));
        self.changed.notify(usize::MAX);
    }
    pub(super) fn request_filtered(&self, dids: Vec<String>) {
        self.pending.borrow_mut().extend(dids);
        self.changed.notify(usize::MAX);
    }
    pub(super) async fn next(&self, clock: &dyn Clock) -> AccessRequest {
        loop {
            let listener = self.changed.listen();
            let deadline = self.full_deadline.get();
            if deadline.is_some_and(|deadline| clock.now() >= deadline) {
                self.full_deadline.set(None);
                self.pending.borrow_mut().clear();
                return AccessRequest::Full;
            }
            if !self.pending.borrow().is_empty() {
                return AccessRequest::Filtered(
                    std::mem::take(&mut *self.pending.borrow_mut())
                        .into_iter()
                        .collect(),
                );
            }
            if let Some(deadline) = deadline {
                future::or(listener, clock.wait_until(deadline)).await;
            } else {
                listener.await;
            }
        }
    }
}
