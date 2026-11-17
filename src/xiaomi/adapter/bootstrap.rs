use super::{
    credentials::Credentials,
    directory::{DescriptionOperation, DescriptionProblem, DeviceSlot, Directory},
    hub::Hub,
    model::{AdapterError, Ready, TopologyReady},
    runner::XiaomiAdapter,
    state::{Context, LogicalSink},
    tasks::TaskSet,
    work::DeviceWork,
};
use crate::{
    configuration::DerivedConfiguration,
    device::{Availability, PhysicalDevice},
    storage::Store,
    xiaomi::{auth::AuthService, cloud::CloudClient, discovery::Discovery},
};
use event_listener::Event;
use std::{
    cell::{Cell, RefCell},
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
    pub(super) fn with_services(
        store: Store,
        cloud: CloudClient,
        discovery: Discovery,
        clock: Arc<dyn Clock>,
        auth: AuthService,
        network: Rc<dyn super::network::Network>,
    ) -> Result<(Self, TopologyReady), AdapterError> {
        let credentials = auth.saved_bridge_credentials()?;
        let snapshot = store.xiaomi().current_snapshot();
        let mijia_generation = snapshot.mijia_generation();
        let ha_generation = snapshot.ha_generation();
        let stored = store.devices().load_configuration()?;
        let scope = credentials
            .as_ref()
            .and_then(|record| stored.scope.clone().filter(|scope| scope.uid == record.uid));
        let revocations = store.devices().load_revocations()?;
        let mut restored = Vec::new();
        let mut staged = Vec::new();
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
            recovery.descriptions.retain(|source| {
                !stored
                    .split_devices
                    .iter()
                    .any(|split| split.did == source.key.id)
            });
            for device in &mut recovery.devices {
                if revocations
                    .iter()
                    .any(|revocation| revocation.key == device.key)
                    || stored
                        .split_devices
                        .iter()
                        .any(|split| split.did == device.key.id)
                {
                    device.native = None;
                }
            }
            graph = recovery.derive();
            for committed in recovery.source_descriptions() {
                let raw = &committed.raw;
                if revocations
                    .iter()
                    .any(|revocation| revocation.key == raw.key)
                {
                    continue;
                }
                let group = restored_relations
                    .iter()
                    .any(|relation| relation.group_did == raw.key.id);
                let (candidate, failure) = match raw.recognize(group) {
                    Ok(description) => (description, None),
                    Err(error) => (committed.clone(), Some(error.to_string())),
                };
                staged.push((candidate, failure));
            }
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
        if credentials.is_some() {
            ready.finish(Ok(()));
        }
        let report = auth.local_status()?;
        let context = Rc::new(Context {
            commands: Rc::new(super::commands::Commands::default()),
            write_gates: super::commands::DeviceWriteGates::default(),
            notifications: RefCell::new(None),
            credentials_changed: Event::new(),
            mijia: auth.mijia_client(),
            network,
            lan_discovery: crate::xiaomi::lan::Discovery::default(),
            lan_scope_invalid: Cell::new(false),
            lan_network_changed: Event::new(),
            ha_changed: Event::new(),
            mijia_changed: Event::new(),
            refresh_changed: Rc::new(Event::new()),
            initial_reads_changed: Rc::new(Event::new()),
            credentials: RefCell::new(Credentials::restored(
                credentials,
                mijia_generation,
                ha_generation,
                report,
            )),
            directory: RefCell::new(Directory::restored(
                scope.clone(),
                relations,
                if scope.is_some() {
                    stored.split_devices.clone()
                } else {
                    Vec::new()
                },
            )),
            hub: RefCell::new(Hub::default()),
            graph: RefCell::new(Rc::new(graph)),
            store,
            tasks: TaskSet::default(),
            configuration_changed: Rc::new(Event::new()),
            changed: Event::new(),
            routing_history: RefCell::new(Default::default()),
            status_changed: Event::new(),
            ready: ready.clone(),
            clock,

            configuration: RefCell::new(Rc::new(stored)),
            sync_requested: Cell::new(true),
            sync_changed: Event::new(),
        });
        for (key, native, physical, receivers) in restored {
            let did = key.id.clone();
            let sink = Rc::new(LogicalSink::new(physical));
            let light_write = native
                .device()
                .functions()
                .iter()
                .any(|function| function.lighting().is_some())
                .then(|| context.write_gates.for_device(&did, context.clock.clone()));
            let work = Rc::new(DeviceWork::new(
                native,
                sink.clone(),
                context.refresh_changed.clone(),
                context.initial_reads_changed.clone(),
                context.configuration_changed.clone(),
                light_write,
            ));
            let slot = DeviceSlot {
                work: Some(work.clone()),
                ..Default::default()
            };
            let virtual_group = context
                .graph
                .borrow()
                .rules
                .get(&did)
                .is_some_and(|group| !group.is_physical);
            context
                .directory
                .borrow_mut()
                .devices
                .insert(did.clone(), slot);
            if !virtual_group {
                work.refresh();
            }
            context.install_work(&did);
            context.add_consumers(work, sink, receivers);
        }
        for (description, failure) in staged {
            let did = description.raw.key.id.clone();
            {
                let mut live = context.directory.borrow_mut();
                let slot = live.devices.entry(did.clone()).or_default();
                if let Some(reason) = &failure {
                    slot.description_failed(DescriptionProblem {
                        operation: DescriptionOperation::RecognizeStored,
                        target: Some((
                            description.raw.model.clone(),
                            description.raw.instance_type.clone(),
                        )),
                        reason: reason.clone(),
                    });
                }
                slot.latest_description = Some(description);
            }
            if failure.is_some() {
                context.status_changed.notify(usize::MAX);
            }
        }
        context.update_routing("stored configuration restored");
        Ok((
            Self {
                cloud,
                auth,
                discovery,
                context,
            },
            TopologyReady(ready),
        ))
    }
}
