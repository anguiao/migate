use super::{ActiveLan, DeviceSessions, SessionContext};
use crate::xiaomi::runtime::XiaomiRuntimeError;
use crate::xiaomi::runtime::lan_connection::{
    LanConnectionConfig, LanConnectionControl, LanReady, lan_connection_fact,
    lan_connection_lifetime,
};
use crate::{
    device::PhysicalDeviceId,
    xiaomi::{
        lan::{LanProperty, LanTarget},
        runtime::{AuthenticatedLan, LanOperationEvidence, LegacyOperationEvidence},
    },
};
#[cfg(test)]
use std::cell::RefCell;
use std::rc::Rc;

impl DeviceSessions {
    pub(super) fn schedule_lan(
        &mut self,
        ctx: &SessionContext<'_>,
    ) -> Result<(), XiaomiRuntimeError> {
        let (Some(catalog), Some(network), Some(record)) = (
            self.catalog.as_ref(),
            ctx.discovery.network(),
            ctx.auth.snapshot().record.as_ref(),
        ) else {
            return Ok(());
        };
        let account = catalog.account.clone();
        let devices = catalog.catalog.devices.clone();
        let network = network.clone();
        let virtual_did = record.virtual_did.clone();
        let binding_snapshot = self.admission.snapshot()?;
        let Some(binding) = binding_snapshot.binding else {
            return Ok(());
        };
        let stale = self
            .lans
            .iter()
            .filter_map(|(physical, lan)| {
                let current = devices.iter().find(|device| {
                    device.home_id == binding.home.as_str()
                        && device.parent_did == physical.parent_did.as_str()
                });
                (!current.is_some_and(|device| {
                    lan.targets
                        .iter()
                        .any(|target| target.matches_catalog(device, binding.home.as_str()))
                }))
                .then_some(physical.clone())
            })
            .collect::<Vec<_>>();
        for physical in stale {
            self.drop_lan(ctx, &physical)?;
        }
        let virtual_did = virtual_did.parse::<u64>().ok().filter(|did| *did != 0);
        let Some(virtual_did) = virtual_did else {
            return Ok(());
        };
        for device in &devices {
            let Ok(parent_did) = crate::device::DeviceDid::new(device.parent_did.clone()) else {
                continue;
            };
            let physical = crate::device::PhysicalDeviceId {
                account: account.clone(),
                home: binding.home.clone(),
                parent_did,
            };
            if device.home_id != binding.home.as_str() || self.lans.contains_key(&physical) {
                continue;
            }
            let Some(descriptor) = device
                .features
                .iter()
                .find(|feature| {
                    feature
                        .binding
                        .properties
                        .iter()
                        .any(|property| property.readable)
                })
                .cloned()
            else {
                continue;
            };
            let Some(property) = descriptor
                .binding
                .properties
                .iter()
                .find(|property| property.readable)
                .map(|property| LanProperty {
                    siid: property.siid,
                    piid: property.piid,
                })
            else {
                continue;
            };
            let targets = network
                .snapshot
                .interfaces()
                .iter()
                .filter_map(|interface| {
                    LanTarget::from_catalog(
                        device,
                        binding.home.as_str(),
                        interface.clone(),
                        network.epoch,
                    )
                    .ok()
                })
                .collect::<Vec<_>>();
            if targets.is_empty() {
                continue;
            }
            let control = LanConnectionControl::new(ctx.wake.clone());
            let (fact_sender, fact_receiver) = flume::bounded(2);
            let config = LanConnectionConfig {
                physical: physical.clone(),
                targets: targets.clone(),
                network: network.clone(),
                account: account.clone(),
                session_generation: ctx.auth.snapshot().session_generation,
                descriptor,
                property,
                virtual_did,
                setup: self.lan_setup.clone(),
                #[cfg(test)]
                startup: RefCell::new(None),
            };
            let task = lan_connection_lifetime(
                config,
                control.clone(),
                fact_sender,
                ctx.registry.clone(),
                ctx.state.clone(),
            );
            self.lans.insert(
                physical,
                ActiveLan {
                    targets,
                    control,
                    current: None,
                    task,
                    fact: lan_connection_fact(fact_receiver),
                },
            );
        }
        Ok(())
    }

    pub(super) fn finish_lan_ready(
        &mut self,
        ctx: &SessionContext<'_>,
        device: &PhysicalDeviceId,
        ready: LanReady,
    ) -> Result<(), XiaomiRuntimeError> {
        #[cfg(test)]
        if let Some(active) = self.lans.get(device) {
            active
                .control
                .ready_seen
                .set(active.control.ready_seen.get().saturating_add(1));
        }
        let Some(active) = self.lans.get(device) else {
            ready.resources.close();
            return Ok(());
        };
        if active.control.generation() != ready.generation || active.control.is_stopped() {
            ready.resources.close();
            return Ok(());
        }
        if ready.resources.is_closed() {
            return Ok(());
        }
        if let Some((attempt, resources)) = active.current.as_ref() {
            if Rc::ptr_eq(resources, &ready.resources) {
                active.control.activate();
                return Ok(());
            }
            if *attempt >= ready.attempt {
                ready.resources.close();
                return Ok(());
            }
        }
        let legacy_operation = match ready.operation {
            LanOperationEvidence::Mcn02Legacy => LegacyOperationEvidence::SuccessfulRead,
            _ => LegacyOperationEvidence::Unverified,
        };
        let proof = AuthenticatedLan {
            account: ready.account,
            session_generation: ready.session_generation,
            target: ready.target,
            network: ready.network.snapshot,
            evidence: ready.evidence.clone(),
            legacy_operation,
        };
        let Some(catalog) = self.catalog.as_ref() else {
            ready.resources.close();
            active.control.reject();
            return Ok(());
        };
        let confirmed = self.admission.confirm_lan(&proof, catalog)?;
        if !confirmed {
            #[cfg(test)]
            active.control.ready_stage.set(3);
            ready.resources.close();
            active.control.reject();
            return Ok(());
        }
        let snapshot = self.admission.snapshot()?;
        ready.resources.install_route(
            (!proof.evidence.native_supported
                && proof.legacy_operation == LegacyOperationEvidence::SuccessfulRead)
                .then_some(ready.descriptor),
        );
        self.sync_cloud_routes(ctx)?;
        ctx.state.reconcile(&snapshot);
        ctx.status.set_admission(snapshot);
        let active = self
            .lans
            .get_mut(device)
            .expect("LAN connection remained registered while publishing");
        if let Some((_, previous)) = active.current.replace((ready.attempt, ready.resources)) {
            previous.close();
        }
        active.control.activate();
        #[cfg(test)]
        active.control.ready_stage.set(4);
        self.update_lan_push_policy();
        Ok(())
    }

    pub(super) fn finish_lan_disconnected(
        &mut self,
        ctx: &SessionContext<'_>,
        device: &PhysicalDeviceId,
        generation: u64,
        attempt: u64,
    ) -> Result<(), XiaomiRuntimeError> {
        let Some(active) = self.lans.get_mut(device) else {
            return Ok(());
        };
        if generation != active.control.generation()
            || active
                .current
                .as_ref()
                .is_some_and(|(current, _)| *current != attempt)
        {
            return Ok(());
        }
        if let Some((_, resources)) = active.current.take() {
            resources.close();
        }
        active.control.set_push_active(false);
        self.admission.remove_lan(device);
        let snapshot = self.admission.snapshot()?;
        ctx.state.reconcile(&snapshot);
        ctx.status.set_admission(snapshot);
        self.select_fallback_push_sources(ctx)?;
        Ok(())
    }

    pub(super) fn update_lan_push_policy(&mut self) {
        for (device, lan) in &self.lans {
            let gateway_selected = self
                .gateways
                .values()
                .any(|gateway| gateway.tokens.contains_key(device));
            lan.control.set_desired_push(!gateway_selected);
        }
    }

    pub(super) fn drop_lan(
        &mut self,
        ctx: &SessionContext<'_>,
        device: &PhysicalDeviceId,
    ) -> Result<(), XiaomiRuntimeError> {
        let Some(lan) = self.lans.remove(device) else {
            return Ok(());
        };
        lan.control.stop();
        if let Some((_, resources)) = lan.current {
            resources.close();
        }
        self.admission.remove_lan(device);
        let snapshot = self.admission.snapshot()?;
        ctx.state.reconcile(&snapshot);
        ctx.status.set_admission(snapshot);
        self.select_fallback_push_sources(ctx)?;
        Ok(())
    }

    pub(super) fn reconnect_lan(
        &mut self,
        ctx: &SessionContext<'_>,
        device: &PhysicalDeviceId,
    ) -> Result<(), XiaomiRuntimeError> {
        let Some(lan) = self.lans.get_mut(device) else {
            return Ok(());
        };
        if let Some((_, resources)) = lan.current.take() {
            resources.close();
        }
        lan.control.set_push_active(false);
        lan.control.reconnect();
        self.admission.remove_lan(device);
        let snapshot = self.admission.snapshot()?;
        ctx.state.reconcile(&snapshot);
        ctx.status.set_admission(snapshot);
        self.select_fallback_push_sources(ctx)?;
        Ok(())
    }

    pub(super) fn drop_all_lans(
        &mut self,
        ctx: &SessionContext<'_>,
    ) -> Result<(), XiaomiRuntimeError> {
        let devices = self.lans.keys().cloned().collect::<Vec<_>>();
        for device in devices {
            self.drop_lan(ctx, &device)?;
        }
        Ok(())
    }
}
