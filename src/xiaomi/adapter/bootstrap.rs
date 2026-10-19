use super::{
    model::{AdapterError, AdapterStatus, GatewayStatus, Ready, TopologyReady},
    runner::XiaomiAdapter,
    state::{Backoff, Context, DeviceSlot, Live, LogicalSink},
    tasks::TaskSet,
    work::DeviceWork,
};
use crate::{
    configuration::DerivedConfiguration,
    device::{Availability, DeviceRegistry, PhysicalDevice},
    storage::Store,
    xiaomi::{auth::AuthService, cloud::CloudClient, discovery::Discovery},
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
        let stored = store.devices().load_configuration()?;
        let scope = credentials
            .as_ref()
            .and_then(|record| stored.scope.clone().filter(|scope| scope.uid == record.uid));
        let revocations = store.devices().load_revocations()?;
        let registry = DeviceRegistry::new();
        let mut restored = Vec::new();
        let mut restored_relations = Vec::new();
        let mut graph = DerivedConfiguration::default();
        if scope.is_some() {
            restored_relations = stored.groups.clone();
            for relation in &mut restored_relations {
                if let Some(revocation) = revocations
                    .iter()
                    .find(|revocation| revocation.key.id == relation.group_did)
                {
                    relation.active = false;
                    relation.error = Some(revocation.reason.clone());
                }
            }
            // The recovery graph is derived from the committed configuration
            // with persisted revocations already applied.
            let mut recovery = stored.clone();
            recovery.groups = restored_relations.clone();
            for device in &mut recovery.devices {
                if revocations
                    .iter()
                    .any(|revocation| revocation.key == device.key)
                {
                    device.native = None;
                }
            }
            graph = recovery.derive();
            // Every current non-revoked native entity is restored, including
            // currently unpublishable native groups; publication is decided by
            // the derived graph, not by what was serving before the restart.
            for device in &recovery.devices {
                let Some(native) = &device.native else {
                    continue;
                };
                let served = graph
                    .served
                    .get(&device.key)
                    .expect("a current native device has a served description");
                let (physical, receivers) = PhysicalDevice::new(served.description.clone());
                physical
                    .set_availability(Availability::Unavailable("restoring Xiaomi access".into()));
                restored.push((device.key.clone(), native.clone(), physical, receivers));
            }
        }
        let relations = scope
            .as_ref()
            .map(|scope| super::relations::Relations::restored(scope, &restored_relations))
            .unwrap_or_default();
        let ready = Rc::new(Ready::default());
        if scope.is_some() {
            ready.finish(Ok(()));
        }
        let report = auth.local_status()?;
        let context = Rc::new(Context {
            commands: Rc::new(super::commands::Commands::default()),
            light_writes: super::commands::LightWriteGates::default(),
            relations: RefCell::new(relations),
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
                devices: BTreeMap::new(),
                session: None,
            }),
            graph: RefCell::new(Rc::new(graph)),
            registry,
            store,
            tasks: TaskSet::default(),
            configuration_changed: Event::new(),
            #[cfg(test)]
            snapshot_observer: RefCell::new(None),
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
        for (key, native, physical, receivers) in restored {
            let did = key.id.clone();
            let sink = Rc::new(LogicalSink::new(physical));
            let light_write = native
                .device
                .functions()
                .iter()
                .any(|function| function.lighting().is_some())
                .then(|| context.light_writes.for_device(&did, context.clock.clone()));
            let work = Rc::new(DeviceWork::new(
                native,
                sink.clone(),
                context.refresh_changed.clone(),
                light_write,
            ));
            context.registry.insert(sink.physical.clone());
            let mut slot = DeviceSlot::restored(work.clone());
            slot.group = restored_relations
                .iter()
                .any(|group| group.group_did == did);
            let virtual_group = context
                .graph
                .borrow()
                .rules
                .get(&did)
                .is_some_and(|group| !group.is_physical);
            context.live.borrow_mut().devices.insert(did.clone(), slot);
            if !virtual_group {
                work.refresh();
            }
            context.install_work(&did);
            context.add_consumers(work, sink, receivers);
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
