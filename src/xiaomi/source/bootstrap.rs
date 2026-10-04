use super::{
    model::{GatewayStatus, Ready, SourceError, SourceStatus, TopologyReady},
    runner::XiaomiSource,
    state::{Context, DeviceRecord, Live},
};
use crate::{
    device::{Availability, Clock, DeviceRegistry, PhysicalDevice},
    storage::{StorageError, Store},
    xiaomi::{auth::AuthService, cloud::CloudClient, discovery::Discovery},
};
use event_listener::Event;
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet},
    future::Future,
    pin::Pin,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};
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
        let mut records = BTreeMap::new();
        let mut consumers = Vec::new();
        if let Some(scope) = &scope {
            let cached = store.devices().list_parsed(&scope.uid, &scope.home_id)?;
            let published = store.devices().list_published()?;
            for metadata in cached {
                let Some(stored) = published
                    .iter()
                    .find(|stored| stored.key == metadata.device.description.key)
                else {
                    continue;
                };
                if metadata.device.functions.len() != stored.functions.len()
                    || stored.functions.iter().any(|stored| {
                        !metadata.device.functions.iter().any(|function| {
                            function.id == stored.key.id
                                && metadata.device.description.category == stored.category
                                && metadata.device.description.capabilities == stored.capabilities
                        })
                    })
                {
                    return Err(StorageError::new(
                        store.path(),
                        "restore Xiaomi device topology",
                        "Cached functions differ from the published identity",
                    )
                    .into());
                }
                let mut description = metadata.physical_description(stored.label.clone());
                for function in &mut description.functions {
                    function.label = stored
                        .functions
                        .iter()
                        .find(|stored| stored.key.id == function.id)
                        .unwrap()
                        .label
                        .clone();
                }
                let (physical, receivers) = PhysicalDevice::new(description);
                physical.set_availability(Availability::Unavailable(
                    "restoring local gateway connection".into(),
                ));
                registry.insert(physical.clone());
                records.insert(
                    metadata.device.description.key.id.clone(),
                    DeviceRecord {
                        cached: metadata.clone(),
                        physical,
                        blocked: None,
                    },
                );
                consumers.push((metadata, receivers));
            }
        }
        let ready = Rc::new(Ready::default());
        if scope.is_some() {
            ready.finish(Ok(registry.clone()));
        }
        let report = auth.local_status()?;
        let members = records.keys().cloned().collect::<BTreeSet<_>>();
        let descriptions = records
            .iter()
            .map(|(did, record)| (did.clone(), record.cached.clone()))
            .collect();
        let context = Rc::new(Context {
            live: RefCell::new(Live {
                credentials,
                scope,
                members,
                records,
                descriptions,
                access: BTreeMap::new(),
                loading: BTreeMap::new(),
                session: None,
            }),
            registry,
            store,
            tasks: RefCell::new(Vec::new()),
            changed: Event::new(),
            status: RefCell::new(SourceStatus {
                authentication: report,
                gateway: GatewayStatus::Discovering,
                catalog_problem: None,
                pending: Vec::new(),
            }),
            status_changed: Event::new(),
            ready: ready.clone(),
            clock,
            rejected_token: RefCell::new(None),
            refreshing_token: RefCell::new(None),
            refreshed_token: RefCell::new(None),
            sync_requested: Cell::new(true),
            sync_changed: Event::new(),
            cloud_paused: Cell::new(false),
            fatal: RefCell::new(None),
        });
        for (metadata, receivers) in consumers {
            context.add_consumers(&metadata, receivers);
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
