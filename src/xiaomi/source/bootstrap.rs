use super::{
    model::{GatewayStatus, Ready, SourceError, SourceStatus, TopologyReady},
    runner::XiaomiSource,
    state::{Context, DeviceSlot, Live},
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
    fn wait_until(&self, deadline: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            async_io::Timer::at(self.0 + deadline).await;
        })
    }
}
impl XiaomiSource {
    pub(super) fn build(
        store: Store,
        cloud: CloudClient,
        discovery: Discovery,
        clock: Arc<dyn Clock>,
    ) -> Result<(Self, TopologyReady), SourceError> {
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
                let (physical, receivers) = PhysicalDevice::new(stored.description());
                physical.set_availability(Availability::Unavailable(
                    "restoring local gateway connection".into(),
                ));
                registry.insert(physical.clone());
                let did = metadata.device.key.id.clone();
                let slot = DeviceSlot::restored(metadata, physical);
                consumers.push((slot.admitted.as_ref().unwrap().clone(), receivers));
                devices.insert(did, slot);
            }
        }
        let ready = Rc::new(Ready::default());
        if scope.is_some() {
            ready.finish(Ok(registry.clone()));
        }
        let report = auth.local_status()?;
        let context = Rc::new(Context {
            live: RefCell::new(Live {
                credentials,
                scope,
                devices,
                session: None,
            }),
            registry,
            store,
            tasks: TaskSet::default(),
            changed: Event::new(),
            status: RefCell::new(SourceStatus {
                authentication: report,
                gateway: GatewayStatus::Discovering,
                catalog_problem: None,
                pending: Vec::new(),
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
