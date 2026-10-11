use super::{
    model::{AdapterError, AdapterStatus, GatewayStatus, Ready, TopologyReady},
    runner::XiaomiAdapter,
    state::{Backoff, Context, DeviceSlot, Live},
    tasks::TaskSet,
};
use crate::{
    device::{Availability, Composition, DeviceRegistry, PhysicalDevice},
    storage::Store,
    xiaomi::{
        auth::AuthService, capability::is_light_group, cloud::CloudClient, discovery::Discovery,
    },
};
use event_listener::Event;
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    future::Future,
    pin::Pin,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};
pub(super) trait Clock: Send + Sync {
    fn now(&self) -> Duration;
    fn unix_now(&self) -> i64;
    fn wait_until(&self, deadline: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
}

pub(super) fn system_clock() -> Arc<dyn Clock> {
    Arc::new(SystemClock(Instant::now()))
}
struct SystemClock(Instant);
impl Clock for SystemClock {
    fn now(&self) -> Duration {
        self.0.elapsed()
    }
    fn unix_now(&self) -> i64 {
        time::OffsetDateTime::now_utc().unix_timestamp()
    }
    fn wait_until(&self, deadline: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            async_io::Timer::at(self.0 + deadline).await;
        })
    }
}
impl XiaomiAdapter {
    pub(super) fn build(
        store: Store,
        cloud: CloudClient,
        discovery: Discovery,
        clock: Arc<dyn Clock>,
    ) -> Result<(Self, TopologyReady), AdapterError> {
        let auth = AuthService::new(store.xiaomi(), cloud.clone());
        let credentials = auth.saved_bridge_credentials()?;
        let scope = credentials
            .as_ref()
            .map(|record| store.devices().load_scope(&record.uid))
            .transpose()?
            .flatten();
        let registry = DeviceRegistry::new();
        let mut devices = BTreeMap::new();
        let mut consumers = Vec::new();
        if let Some(scope) = &scope {
            for (metadata, stored) in store.devices().restore_cached(&scope.uid, &scope.home_id)? {
                if is_light_group(&metadata.device.key, &metadata.model) {
                    store.devices().remove(&metadata.device.key)?;
                    continue;
                }
                let (physical, receivers) = PhysicalDevice::new(stored.description());
                let did = metadata.device.key.id.clone();
                if let Some(reason) = super::membership::publication_problem(&metadata.device) {
                    // Valid but unpublishable: keep the reservation and reason
                    // without installing a registry endpoint or consumers.
                    physical.set_availability(Availability::Unavailable(reason.clone()));
                    drop(receivers);
                    devices.insert(did, DeviceSlot::unpublished(metadata, physical, reason));
                    continue;
                }
                physical
                    .set_availability(Availability::Unavailable("restoring Xiaomi access".into()));
                registry.insert(physical.clone());
                let slot = DeviceSlot::restored(metadata, physical);
                consumers.push((slot.admitted.as_ref().unwrap().clone(), receivers));
                devices.insert(did, slot);
            }
            let confirmed = store.devices().restore_groups(&scope.uid, &scope.home_id)?;
            if confirmed.version > 0 || !confirmed.groups.is_empty() {
                // Restored parents stay known placeholders until a fresh
                // directory read confirms their relation.
                for group in &confirmed.groups {
                    devices.entry(group.key.id.clone()).or_default();
                }
                let compositions = confirmed
                    .groups
                    .into_iter()
                    .map(|group| Composition {
                        key: group.key,
                        label: group.label,
                        members: group.members,
                    })
                    .collect::<Vec<_>>();
                registry
                    .update_composition_versioned(confirmed.version, compositions, &[])
                    .map_err(|error| AdapterError::Unavailable(error.to_string()))?;
            }
        }
        let ready = Rc::new(Ready::default());
        if scope.is_some() {
            ready.finish(Ok(registry.clone()));
        }
        let report = auth.local_status()?;
        let context = Rc::new(Context {
            commands: Rc::new(super::commands::Commands::default()),
            light_writes: super::commands::LightWriteGates::default(),
            notifications: RefCell::new(None),
            credentials_changed: Event::new(),
            #[cfg(test)]
            notification_connector: Cell::new(
                super::notifications::NotificationConnector::Disabled,
            ),
            priority: Cell::new(super::routing::PathPriority::LocalFirst),
            cloud: cloud.clone(),
            cloud_sync: RefCell::new(super::cloud::CloudSync::default()),
            http_paused: Cell::new(false),
            cloud_scope_valid: Cell::new(false),
            sync_not_before: Cell::new(None),
            sync_backoff: RefCell::new(Backoff::new(1, 30)),
            refresh_changed: Rc::new(Event::new()),
            live: RefCell::new(Live {
                credentials,
                scope,
                complete: false,
                devices,
                session: None,
            }),
            registry,
            store,
            tasks: TaskSet::default(),
            changed: Event::new(),
            status: RefCell::new(AdapterStatus {
                authentication: report,
                gateway: GatewayStatus::Discovering,
                catalog_problem: None,
                pending: Vec::new(),
                routing: Default::default(),
                devices: Vec::new(),
            }),
            status_changed: Event::new(),
            cloud_problem: RefCell::new(None),
            access_problem: RefCell::new(None),
            ready: ready.clone(),
            clock,

            sync_requested: Cell::new(true),
            sync_changed: Event::new(),
        });
        for (record, receivers) in consumers {
            context.install_work(&record.physical.key().id);
            context.add_consumers(record, receivers);
        }
        let unpublished = context
            .live
            .borrow()
            .devices
            .iter()
            .filter_map(|(did, slot)| slot.unpublished.clone().map(|reason| (did.clone(), reason)))
            .collect::<Vec<_>>();
        for (did, reason) in unpublished {
            context.problem(&did, Some(reason));
        }
        Ok((
            Self {
                cloud,
                auth,
                discovery,
                context,
                #[cfg(test)]
                test_ca: None,
                #[cfg(test)]
                request_timeout: Duration::from_secs(10),
            },
            TopologyReady(ready),
        ))
    }
}
