use super::{
    model::{AdapterError, AdapterStatus, GatewayStatus, Ready, TopologyReady},
    runner::XiaomiAdapter,
    state::{Backoff, Context, DeviceSlot, GroupSource, Live},
    tasks::TaskSet,
};
use crate::{
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
        let scope = credentials
            .as_ref()
            .map(|record| store.devices().load_scope(&record.uid))
            .transpose()?
            .flatten();
        let registry = DeviceRegistry::new();
        let mut devices = BTreeMap::new();
        let mut consumers = Vec::new();
        let mut restored_relations = Vec::new();
        if let Some(scope) = &scope {
            let saved = store.devices().load_publication(Some(scope))?;
            restored_relations = saved.relations;
            for relation in &mut restored_relations {
                if let Some(revocation) = saved
                    .revocations
                    .iter()
                    .find(|revocation| revocation.key.id == relation.group_did)
                {
                    relation.active = false;
                    relation.error = Some(revocation.reason.clone());
                }
            }
            let hidden = restored_relations
                .iter()
                .filter(|relation| {
                    saved
                        .records
                        .iter()
                        .any(|record| record.key.id == relation.group_did && record.published)
                })
                .flat_map(|relation| relation.members.iter().cloned())
                .collect::<std::collections::BTreeSet<_>>();
            for metadata in saved.cached {
                let Some(stored) = saved
                    .records
                    .iter()
                    .find(|stored| stored.key == metadata.device.key)
                else {
                    return Err(crate::storage::StorageError::new(
                        store.path(),
                        "restore source configuration",
                        "Cached source has no reserved identity",
                    )
                    .into());
                };
                if saved
                    .revocations
                    .iter()
                    .any(|revocation| revocation.key == stored.key)
                    || (!stored.published && !hidden.contains(&stored.key.id))
                {
                    continue;
                }
                let (physical, receivers) = PhysicalDevice::new(stored.description());
                physical
                    .set_availability(Availability::Unavailable("restoring Xiaomi access".into()));
                registry.insert(physical.clone());
                let did = metadata.device.key.id.clone();
                let mut slot = DeviceSlot::restored(metadata, physical);
                if let Some(group) = restored_relations
                    .iter()
                    .find(|group| group.group_did == did)
                {
                    let group = GroupSource {
                        record_did: group.record_did.clone(),
                        model: group.model.clone(),
                        instance_type: group.instance_type.clone(),
                        members: group.members.clone(),
                        is_physical: group.is_physical,
                        online: false,
                    };
                    slot.group = Some(group.clone());
                    slot.effective_group = Some(group);
                }
                consumers.push((slot.admitted.as_ref().unwrap().clone(), receivers));
                devices.insert(did, slot);
            }
        }
        let relations = scope
            .as_ref()
            .map(|scope| super::relations::Relations::restored(scope, &restored_relations))
            .unwrap_or_default();
        let ready = Rc::new(Ready::default());
        if scope.is_some() {
            ready.finish(Ok(registry.clone()));
        }
        let report = auth.local_status()?;
        let context = Rc::new(Context {
            commands: Rc::new(super::commands::Commands::default()),
            light_writes: super::commands::LightWriteGates::default(),
            watches: RefCell::new(super::watch::Watches::default()),
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
                devices,
                session: None,
            }),
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
        for (record, receivers) in consumers {
            context.install_work(&record.physical.key().id);
            context.add_consumers(record, receivers);
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
