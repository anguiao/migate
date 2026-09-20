use super::super::{
    common,
    endpoint::{FeatureRuntime, build_runtime},
    reporting,
    storage::{StoreAdapter, TopologyStore},
};
use super::plan::hex_digest;
use crate::{
    device::{DeviceService, FeatureIdentity},
    storage::{DeviceStore, FeatureIdentity as AllocatedFeature, StorageError},
};
use event_listener::Event;
use rs_matter::dm::clusters::desc::ClusterHandler as _;
use rs_matter::{
    dm::{
        HandlerContext,
        clusters::{decl::bridged_device_basic_information as bridged, desc},
    },
    error::{Error, ErrorCode},
    persist::BASIC_INFO_KEY,
};
use sha1::{Digest, Sha1};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
};

pub(in crate::matter) struct TopologyRegistry {
    service: DeviceService,
    store: StoreAdapter,
    devices: DeviceStore,
    runtimes: RefCell<Rc<Vec<Rc<FeatureRuntime>>>>,
    rebuild_requested: Cell<bool>,
    rebuild_event: Event,
    topology_dirty: Cell<bool>,
    requested_topology_signature: RefCell<Option<String>>,
}

impl TopologyRegistry {
    pub(in crate::matter) fn new(
        service: DeviceService,
        devices: DeviceStore,
        adapter: StoreAdapter,
    ) -> Result<Self, StorageError> {
        let loaded = adapter.capture(devices.load_features(true))?;
        let allocations = loaded
            .into_iter()
            .filter(|allocation| allocation.active)
            .map(|allocation| (allocation.feature.clone(), allocation))
            .collect::<BTreeMap<_, _>>();
        let model = Self {
            service,
            store: adapter,
            devices,
            runtimes: RefCell::new(Rc::new(Vec::new())),
            rebuild_requested: Cell::new(false),
            rebuild_event: Event::new(),
            topology_dirty: Cell::new(false),
            requested_topology_signature: RefCell::new(None),
        };
        model.rebuild_initial(&allocations)?;
        let topology = model
            .store
            .capture(model.store.storage().topology_signature())?;
        model
            .topology_dirty
            .set(topology != model.topology_signature());
        Ok(model)
    }

    fn rebuild_initial(
        &self,
        allocations: &BTreeMap<FeatureIdentity, AllocatedFeature>,
    ) -> Result<(), StorageError> {
        let mut runtimes = Vec::new();
        for (feature, allocation) in allocations {
            if let Some(published) = self.service.feature(feature)
                && let Some(runtime) = build_runtime(
                    &self.service,
                    &self.store,
                    allocation.clone(),
                    published.name,
                    published.capabilities.0,
                )?
            {
                runtimes.push(Rc::new(runtime));
            }
        }
        runtimes.sort_by_key(|runtime| runtime.allocation.endpoint);
        *self.runtimes.borrow_mut() = Rc::new(runtimes);
        Ok(())
    }

    pub(in crate::matter) fn endpoint_for(&self, feature: &FeatureIdentity) -> Option<u16> {
        self.snapshot()
            .iter()
            .find(|runtime| &runtime.allocation.feature == feature)
            .map(|runtime| runtime.allocation.endpoint)
    }

    pub(in crate::matter) fn snapshot(&self) -> Rc<Vec<Rc<FeatureRuntime>>> {
        self.runtimes.borrow().clone()
    }

    pub(in crate::matter) fn take_rebuild_request(&self) -> bool {
        self.rebuild_requested.replace(false)
    }

    pub(in crate::matter) async fn rebuild_requested(&self) {
        loop {
            let listener = self.rebuild_event.listen();
            if self.rebuild_requested.get() {
                return;
            }
            listener.await;
        }
    }

    pub(in crate::matter) fn topology_signature(&self) -> String {
        let runtimes = self.snapshot();
        topology_signature_for(&runtimes)
    }

    pub(in crate::matter) fn reconcile(&self, ctx: &impl HandlerContext) -> Result<(), Error> {
        let loaded = self.store.record(self.devices.load_features(true))?;
        let allocations = loaded
            .into_iter()
            .filter(|allocation| allocation.active)
            .map(|allocation| (allocation.feature.clone(), allocation))
            .collect::<BTreeMap<_, _>>();
        let mut desired = Vec::new();
        for published in self.service.features() {
            let Some(allocation) = allocations.get(&published.identity).cloned() else {
                continue;
            };
            if let Some(runtime) = build_runtime(
                &self.service,
                &self.store,
                allocation,
                published.name,
                published.capabilities.0,
            )
            .map_err(|_| ErrorCode::Failure)?
            {
                desired.push(Rc::new(runtime));
            }
        }
        desired.sort_by_key(|runtime| runtime.allocation.endpoint);
        let current = self.snapshot();
        let shape_changed = desired.iter().any(|candidate| {
            current
                .iter()
                .find(|runtime| runtime.allocation.feature == candidate.allocation.feature)
                .is_some_and(|runtime| runtime.shape_signature != candidate.shape_signature)
        });
        if shape_changed {
            *self.requested_topology_signature.borrow_mut() =
                Some(topology_signature_for(&desired));
            self.rebuild_requested.set(true);
            self.rebuild_event.notify(usize::MAX);
            self.topology_dirty.set(true);
            return self.finish_topology_change(ctx);
        }

        *self.requested_topology_signature.borrow_mut() = None;
        let mut updated = Vec::with_capacity(desired.len());
        for candidate in desired {
            let Some(runtime) = current
                .iter()
                .find(|runtime| runtime.allocation.feature == candidate.allocation.feature)
            else {
                updated.push(candidate);
                self.topology_dirty.set(true);
                continue;
            };
            reporting::report_reachability(
                ctx,
                runtime,
                self.service.is_available(&runtime.allocation.feature),
            )?;
            if runtime
                .common
                .set_default_label(&candidate.common.default_label())
            {
                ctx.notify_attr_changed(
                    runtime.allocation.endpoint,
                    common::BRIDGED_CLUSTER.id,
                    bridged::AttributeId::NodeLabel as _,
                );
            }
            let config_changed =
                *candidate.config_signature.borrow() != *runtime.config_signature.borrow();
            let capabilities = candidate.sensor.capabilities();
            let capabilities_changed = capabilities != runtime.sensor.capabilities();
            runtime.sensor.set_capabilities(capabilities.clone());
            if let Some(lighting) = &runtime.lighting {
                lighting.set_capabilities(capabilities.clone());
            }
            if let Some(fan) = &runtime.fan {
                fan.set_capabilities(capabilities.clone());
            }
            if let Some(thermostat) = &runtime.thermostat {
                thermostat.set_capabilities(capabilities.clone());
            }
            if let Some(curtain) = &runtime.curtain {
                curtain.set_capabilities(capabilities.clone());
            }
            if let Some(rvc) = &runtime.rvc {
                rvc.set_capabilities(capabilities.clone());
            }
            reporting::reconcile_values(ctx, &self.service, runtime, &capabilities)?;
            if capabilities_changed {
                reporting::notify_configuration(ctx, runtime);
            }
            if config_changed {
                *runtime.config_signature.borrow_mut() =
                    candidate.config_signature.borrow().clone();
                self.topology_dirty.set(true);
            }
            updated.push(runtime.clone());
        }
        if updated.len() != current.len() {
            self.topology_dirty.set(true);
        }
        *self.runtimes.borrow_mut() = Rc::new(updated);
        self.finish_topology_change(ctx)
    }

    fn finish_topology_change(&self, ctx: &impl HandlerContext) -> Result<(), Error> {
        if self.topology_dirty.replace(false) {
            self.persist_configuration_change(ctx)?;
            ctx.notify_attr_changed(
                0,
                desc::DescHandler::CLUSTER.id,
                desc::AttributeId::PartsList as _,
            );
            ctx.notify_attr_changed(
                1,
                desc::DescHandler::CLUSTER.id,
                desc::AttributeId::PartsList as _,
            );
        }
        Ok(())
    }

    fn persist_configuration_change(&self, ctx: &impl HandlerContext) -> Result<(), Error> {
        let signature = self
            .requested_topology_signature
            .borrow()
            .clone()
            .unwrap_or_else(|| self.topology_signature());
        let stored_signature = self
            .store
            .record(self.store.storage().topology_signature())?;
        if stored_signature.is_empty() {
            let bytes = self
                .store
                .record(self.store.storage().get(BASIC_INFO_KEY))?
                .ok_or(ErrorCode::InvalidData)?;
            self.store.record(self.store.storage().save_topology(
                BASIC_INFO_KEY,
                &bytes,
                &signature,
            ))?;
            return Ok(());
        }
        if stored_signature == signature {
            return Ok(());
        }
        ctx.matter().bump_configuration_version(
            ctx.matter()
                .kv(TopologyStore::new(self.store.clone(), signature)),
            ctx,
        )?;
        Ok(())
    }
}

fn topology_signature_for(runtimes: &[Rc<FeatureRuntime>]) -> String {
    let mut digest = Sha1::new();
    for runtime in runtimes {
        digest.update(runtime.allocation.endpoint.to_be_bytes());
        digest.update(runtime.shape_signature.as_bytes());
        digest.update(runtime.config_signature.borrow().as_bytes());
    }
    hex_digest(digest.finalize().as_slice())
}
