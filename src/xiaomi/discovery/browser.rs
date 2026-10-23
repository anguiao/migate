use std::sync::{Arc, Mutex};

use event_listener::Event;
use futures_lite::future;
use mdns_sd::{Receiver, ServiceDaemon, ServiceEvent};

use super::{
    announcement::{Registry, SERVICE_TYPE},
    model::{DiscoveryError, DiscoverySnapshot},
};

#[derive(Default)]
struct State {
    snapshot: DiscoverySnapshot,
    rediscover: Option<String>,
    stopped: bool,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    changed: Event,
}

/// The current LAN directory. It never restores network addresses from storage.
#[derive(Clone)]
pub struct Discovery(Arc<Shared>);

impl Discovery {
    pub fn start() -> Result<(Self, DiscoveryDriver), DiscoveryError> {
        let daemon =
            ServiceDaemon::new().map_err(|error| DiscoveryError::Backend(error.to_string()))?;
        let receiver = match daemon.browse(SERVICE_TYPE) {
            Ok(receiver) => receiver,
            Err(error) => {
                shutdown_daemon(&daemon);
                return Err(DiscoveryError::Backend(error.to_string()));
            }
        };
        let shutdown = daemon.clone();
        Ok(Self::from_events(
            receiver,
            Some(daemon),
            Some(Box::new(move || shutdown_daemon(&shutdown))),
        ))
    }

    pub(super) fn from_events(
        receiver: Receiver<ServiceEvent>,
        daemon: Option<ServiceDaemon>,
        shutdown: Option<Box<dyn FnOnce() + Send>>,
    ) -> (Self, DiscoveryDriver) {
        let discovery = Self(Arc::new(Shared::default()));
        let driver = DiscoveryDriver {
            discovery: discovery.clone(),
            receiver,
            daemon,
            registry: Registry::default(),
            shutdown,
        };
        (discovery, driver)
    }

    pub fn snapshot(&self) -> DiscoverySnapshot {
        self.0.state.lock().unwrap().snapshot.clone()
    }

    /// Retry discovery when the selected home has no current route.
    pub(crate) fn rediscover(&self, group_id: &str) {
        self.0.state.lock().unwrap().rediscover = Some(group_id.into());
        self.0.changed.notify(usize::MAX);
    }

    pub fn subscribe(&self) -> DiscoveryChanges {
        DiscoveryChanges {
            discovery: self.clone(),
            revision: self.snapshot().revision,
        }
    }

    fn publish(&self, snapshot: DiscoverySnapshot) {
        let mut state = self.0.state.lock().unwrap();
        if state.snapshot.gateways != snapshot.gateways {
            state.snapshot.gateways = snapshot.gateways;
            state.snapshot.revision = state.snapshot.revision.wrapping_add(1);
            self.0.changed.notify(usize::MAX);
        }
    }
}

pub struct DiscoveryChanges {
    discovery: Discovery,
    revision: u64,
}

impl DiscoveryChanges {
    /// Changes can coalesce. Dropping this wait does not consume a revision.
    pub async fn changed(&mut self) -> Result<DiscoverySnapshot, DiscoveryError> {
        loop {
            let listener = self.discovery.0.changed.listen();
            {
                let state = self.discovery.0.state.lock().unwrap();
                if state.stopped {
                    return Err(DiscoveryError::Stopped);
                }
                if state.snapshot.revision != self.revision {
                    self.revision = state.snapshot.revision;
                    return Ok(state.snapshot.clone());
                }
            }
            listener.await;
        }
    }
}

/// Consume all daemon updates for the full service lifetime, including TXT,
/// address changes and removals. First-start waiting belongs to the caller.
pub struct DiscoveryDriver {
    discovery: Discovery,
    receiver: Receiver<ServiceEvent>,
    daemon: Option<ServiceDaemon>,
    registry: Registry,
    shutdown: Option<Box<dyn FnOnce() + Send>>,
}

impl DiscoveryDriver {
    pub async fn run(mut self) -> Result<(), DiscoveryError> {
        loop {
            let changed = self.discovery.0.changed.listen();
            let rediscover = self.discovery.0.state.lock().unwrap().rediscover.take();
            if let Some(group_id) = rediscover
                && self.discovery.snapshot().routes(&group_id, None).is_empty()
                && let Some(daemon) = &self.daemon
            {
                // A fresh browse clears cached PTRs that can suppress resolution
                // after the shorter-lived SRV/address records have expired.
                daemon
                    .stop_browse(SERVICE_TYPE)
                    .map_err(|error| DiscoveryError::Backend(error.to_string()))?;
                self.receiver = daemon
                    .browse(SERVICE_TYPE)
                    .map_err(|error| DiscoveryError::Backend(error.to_string()))?;
                self.registry = Registry::default();
                self.discovery.publish(self.registry.snapshot());
                log::debug!("Restarted local gateway discovery");
            }
            let event = future::or(
                async {
                    self.receiver
                        .recv_async()
                        .await
                        .map(Some)
                        .map_err(|_| DiscoveryError::Stopped)
                },
                async {
                    changed.await;
                    Ok(None)
                },
            )
            .await?;
            let Some(event) = event else { continue };
            match event {
                ServiceEvent::ServiceResolved(service) if service.ty_domain == SERVICE_TYPE => {
                    if let Err(error) = self.registry.resolve(*service) {
                        log::warn!("Gateway discovery rejected an announcement: {error}");
                    }
                }
                ServiceEvent::ServiceRemoved(service_type, name)
                    if service_type == SERVICE_TYPE =>
                {
                    self.registry.remove(&name);
                }
                ServiceEvent::SearchStopped(service_type) if service_type == SERVICE_TYPE => {
                    return Err(DiscoveryError::Stopped);
                }
                _ => continue,
            }
            self.discovery.publish(self.registry.snapshot());
        }
    }
}

impl Drop for DiscoveryDriver {
    fn drop(&mut self) {
        let mut state = self.discovery.0.state.lock().unwrap();
        state.stopped = true;
        state.snapshot.gateways.clear();
        state.snapshot.revision = state.snapshot.revision.wrapping_add(1);
        self.discovery.0.changed.notify(usize::MAX);
        drop(state);
        if let Some(shutdown) = self.shutdown.take() {
            shutdown();
        }
    }
}

fn shutdown_daemon(daemon: &ServiceDaemon) {
    if let Err(error) = daemon.stop_browse(SERVICE_TYPE) {
        log::warn!("Failed to stop gateway discovery: {error}");
    }
    if let Err(error) = daemon.shutdown() {
        log::warn!("Failed to stop gateway discovery daemon: {error}");
    }
}
