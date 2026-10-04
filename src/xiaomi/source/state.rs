use super::model::{GatewayStatus, Ready, SourceError, SourceStatus};
use crate::{
    device::{Availability, Clock, Device},
    storage::{CachedDevice, Store, XiaomiRecord},
    xiaomi::{auth::BridgePreparation, central::SessionHandle},
};
use event_listener::Event;
use std::{cell::RefCell, rc::Rc, sync::Arc, time::Duration};

pub(super) const MAX_BACKOFF: Duration = Duration::from_secs(30);

pub(super) struct Topology {
    pub(super) device: Device,
    pub(super) metadata: CachedDevice,
    pub(super) dirty: bool,
}

pub(super) struct Session {
    pub(super) handle: SessionHandle,
    pub(super) refresh: AccessRefresh,
}
impl Session {
    pub(super) fn new(handle: SessionHandle) -> Self {
        Self {
            handle,
            refresh: AccessRefresh::Idle,
        }
    }
}

#[derive(Default)]
pub(super) enum AccessRefresh {
    #[default]
    Idle,
    Pending,
    Queued,
}
impl AccessRefresh {
    pub(super) fn request(&mut self) -> bool {
        match self {
            Self::Idle => {
                *self = Self::Pending;
                true
            }
            Self::Pending | Self::Queued => {
                *self = Self::Queued;
                false
            }
        }
    }
    pub(super) fn finish(&mut self) -> bool {
        matches!(std::mem::take(self), Self::Queued)
    }
}

#[derive(Default)]
pub(super) struct Preparation {
    pub(super) version: u64,
    pub(super) available_since: Option<Duration>,
}
impl Preparation {
    pub(super) fn invalidate(&mut self, now: Duration) -> bool {
        let stable = self
            .available_since
            .take()
            .is_some_and(|since| now.saturating_sub(since) >= MAX_BACKOFF);
        self.version = self.version.wrapping_add(1);
        stable
    }
}

pub(super) struct Live {
    pub(super) credentials: Option<XiaomiRecord>,
    pub(super) topology: Option<Topology>,
    pub(super) session: Option<Session>,
    pub(super) preparation: Preparation,
    pub(super) reset_backoff: bool,
    pub(super) blocked: Option<String>,
}
impl Live {
    pub(super) fn generation(&self) -> Option<u64> {
        self.session
            .as_ref()
            .map(|session| session.handle.generation())
    }
}

pub(super) struct Context {
    pub(super) live: RefCell<Live>,
    pub(super) changed: Event,
    pub(super) status: RefCell<SourceStatus>,
    pub(super) status_changed: Event,
    pub(super) ready: Rc<Ready>,
    pub(super) clock: Arc<dyn Clock>,
}
impl Context {
    pub(super) fn device(&self) -> Device {
        self.live.borrow().topology.as_ref().unwrap().device.clone()
    }
    pub(super) fn metadata(&self) -> CachedDevice {
        self.live
            .borrow()
            .topology
            .as_ref()
            .unwrap()
            .metadata
            .clone()
    }
    pub(super) fn current(&self, generation: u64, preparation: u64) -> bool {
        let live = self.live.borrow();
        live.generation() == Some(generation) && live.preparation.version == preparation
    }
    pub(super) fn session_current(&self, generation: u64) -> bool {
        self.live.borrow().generation() == Some(generation)
    }
    pub(super) fn set_gateway(&self, status: GatewayStatus) {
        let mut current = self.status.borrow_mut();
        if current.gateway != status {
            log::info!("Xiaomi gateway state: {status:?}");
            current.gateway = status;
            self.status_changed.notify(usize::MAX);
        }
    }
    pub(super) fn set_catalog_problem(&self, problem: Option<String>) {
        let mut status = self.status.borrow_mut();
        if status.catalog_problem != problem {
            status.catalog_problem = problem;
            self.status_changed.notify(usize::MAX);
        }
    }
    pub(super) fn unavailable(&self, reason: String) {
        let device = {
            let mut live = self.live.borrow_mut();
            live.reset_backoff |= live.preparation.invalidate(self.clock.now());
            live.topology
                .as_ref()
                .map(|topology| topology.device.clone())
        };
        if let Some(device) = device {
            device.set_availability(Availability::Unavailable(reason));
        }
        self.changed.notify(usize::MAX);
    }
    pub(super) fn disconnect(&self, reason: String) {
        let session = self.live.borrow_mut().session.take();
        self.unavailable(reason.clone());
        self.set_gateway(GatewayStatus::Unavailable(reason));
        if let Some(session) = session {
            session.handle.close();
        }
    }
    pub(super) fn block(&self, reason: String) {
        self.live.borrow_mut().blocked = Some(reason.clone());
        self.disconnect(reason);
    }
    pub(super) fn prepared(&self, generation: u64, version: u64, value: bool) {
        if !self.current(generation, version) {
            return;
        }
        let device = self.device();
        device.observe(value);
        device.set_availability(Availability::Available);
        self.live.borrow_mut().preparation.available_since = Some(self.clock.now());
        self.changed.notify(usize::MAX);
    }
    pub(super) async fn wait_reprepare(&self, generation: u64, version: u64) {
        loop {
            let listener = self.changed.listen();
            if !self.current(generation, version) {
                return;
            }
            listener.await;
        }
    }
    pub(super) fn persist_topology(&self, store: &Store) -> Result<(), SourceError> {
        let mut live = self.live.borrow_mut();
        let topology = live.topology.as_mut().unwrap();
        if topology.dirty {
            store.devices().save(&topology.metadata)?;
            topology.dirty = false;
        }
        Ok(())
    }
}

pub(super) fn update_auth(context: &Context, preparation: &BridgePreparation) {
    let credentials_changed = {
        let mut live = context.live.borrow_mut();
        let changed = live.credentials != preparation.credentials;
        live.credentials = preparation.credentials.clone();
        changed
    };
    context.status.borrow_mut().authentication = preparation.report.clone();
    context.status_changed.notify(usize::MAX);
    if credentials_changed {
        context.changed.notify(usize::MAX);
    }
}

pub(super) struct Backoff {
    delay: Duration,
}
impl Backoff {
    pub(super) fn new() -> Self {
        Self {
            delay: Duration::from_secs(1),
        }
    }
    pub(super) fn failed(&mut self, stable: bool) -> Duration {
        if stable {
            self.delay = Duration::from_secs(1);
        }
        let delay = self.delay;
        self.delay = (self.delay * 2).min(MAX_BACKOFF);
        delay
    }
}
