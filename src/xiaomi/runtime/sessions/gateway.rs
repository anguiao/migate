use super::{ActiveGateway, ConnectingGateway, DeviceSessions, SessionContext};
use crate::xiaomi::runtime::connection::gateway_failure_code;
use crate::xiaomi::runtime::gateway_connection::{
    GatewayConnectionConfig, GatewayConnectionControl, GatewayOperationResult, GatewayPublication,
    GatewayReady, gateway_connection_fact, gateway_full_connection_lifetime,
};
use crate::xiaomi::runtime::{
    XiaomiFailureStage, XiaomiRuntimeComponent, XiaomiRuntimeError, unix_time,
};
use crate::xiaomi::{
    gateway::{EventArguments, GatewayNotification},
    runtime::{AuthenticatedGateway, CloudEvidence, CloudStatus, PushSource},
};
#[cfg(test)]
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

impl DeviceSessions {
    pub(in crate::xiaomi::runtime) fn reconcile_gateway_candidates(
        &mut self,
        ctx: &SessionContext<'_>,
    ) -> Result<(), XiaomiRuntimeError> {
        let candidates = ctx.discovery.candidates();
        let stale_connecting = self
            .connecting_gateways
            .iter()
            .filter(|(did, connecting)| {
                !candidates.iter().any(|candidate| {
                    candidate.gateway_did == **did
                        && candidate.home_group == connecting.candidate.home_group
                })
            })
            .map(|(&did, _)| did)
            .collect::<Vec<_>>();
        for did in stale_connecting {
            if let Some(connecting) = self.connecting_gateways.remove(&did) {
                connecting.control.stop();
                connecting.authority.revoke();
            }
        }
        let stale = self
            .gateways
            .iter()
            .filter(|(_, gateway)| {
                !candidates.iter().any(|candidate| {
                    candidate.gateway_did == gateway.did
                        && candidate.home_group == gateway.proof.candidate.home_group
                        && candidate
                            .endpoints
                            .contains(&gateway.proof.selected_endpoint)
                })
            })
            .map(|(&did, _)| did)
            .collect::<Vec<_>>();
        for did in stale {
            self.drop_gateway(ctx, did)?;
        }
        Ok(())
    }

    pub(super) fn schedule_gateway(
        &mut self,
        ctx: &SessionContext<'_>,
    ) -> Result<(), XiaomiRuntimeError> {
        let (Some(catalog), Some(network)) = (self.catalog.as_ref(), ctx.discovery.network())
        else {
            return Ok(());
        };
        let Some(candidate) = ctx.discovery.candidates().into_iter().find(|candidate| {
            catalog
                .catalog
                .homes
                .iter()
                .any(|home| home.group_id == candidate.home_group)
                && !self.gateways.contains_key(&candidate.gateway_did)
                && !self
                    .connecting_gateways
                    .contains_key(&candidate.gateway_did)
        }) else {
            return Ok(());
        };
        let endpoints = candidate
            .endpoints
            .iter()
            .filter_map(|endpoint| {
                network
                    .snapshot
                    .interfaces_with_index(endpoint.interface_index)
                    .find(|interface| interface.address() == endpoint.source_address)
                    .cloned()
                    .map(|interface| (endpoint.clone(), interface))
            })
            .collect::<Vec<_>>();
        if endpoints.is_empty() {
            return Ok(());
        }
        let snapshot = ctx.auth.current_snapshot()?;
        let Some(record) = snapshot.record else {
            return Ok(());
        };
        if record.uid != catalog.account.as_str() {
            return Ok(());
        }
        let authority = ctx.gateway_authority();
        let config = GatewayConnectionConfig {
            candidate: candidate.clone(),
            endpoints,
            network: network.clone(),
            virtual_did: record.virtual_did,
            private_key_pem: record.private_key_pem,
            certificate_pem: record.certificate_pem,
            authority: authority.clone(),
            setup: self.gateway_setup.clone(),
            #[cfg(test)]
            startup: RefCell::new(None),
        };
        let control = GatewayConnectionControl::new();
        let (fact_sender, fact_receiver) = flume::bounded(256);
        self.connecting_gateways.insert(
            candidate.gateway_did,
            ConnectingGateway {
                candidate: candidate.clone(),
                network: network.clone(),
                account: catalog.account.clone(),
                session_generation: snapshot.session_generation,
                authority,
                task: gateway_full_connection_lifetime(config, control.clone(), fact_sender),
                fact: gateway_connection_fact(fact_receiver),
                control,
                attempt: None,
            },
        );
        Ok(())
    }

    pub(super) fn finish_gateway_ready(
        &mut self,
        ctx: &SessionContext<'_>,
        did: u64,
        ready: GatewayReady,
    ) -> Result<(), XiaomiRuntimeError> {
        let Some(connecting) = self.connecting_gateways.get_mut(&did) else {
            return Ok(());
        };
        if !ready.authority.check() {
            return Ok(());
        }
        connecting.attempt = Some(ready.attempt);
        let Some(catalog) = self.catalog.as_ref() else {
            connecting.control.publish(GatewayPublication::Rejected);
            return Ok(());
        };
        let proof = AuthenticatedGateway {
            account: connecting.account.clone(),
            session_generation: connecting.session_generation,
            candidate: connecting.candidate.clone(),
            selected_endpoint: ready.endpoint.clone(),
            network: connecting.network.snapshot.clone(),
            evidence: ready.evidence.clone(),
        };
        if let Err(error) = self.admission.observe_gateway(proof.clone(), catalog) {
            connecting.control.publish(GatewayPublication::Rejected);
            connecting.authority.revoke();
            return Err(error.into());
        }
        if self.cloud_validated
            && let Err(error) = self.admission.observe_cloud(&CloudEvidence {
                account: catalog.account.clone(),
                session_generation: connecting.session_generation,
                status: CloudStatus::Ready,
            })
        {
            connecting.control.publish(GatewayPublication::Rejected);
            connecting.authority.revoke();
            return Err(error.into());
        }
        let snapshot = match self.admission.snapshot() {
            Ok(snapshot) => snapshot,
            Err(error) => {
                connecting.control.publish(GatewayPublication::Rejected);
                connecting.authority.revoke();
                return Err(error.into());
            }
        };
        let auth = ctx.auth.current_snapshot()?;
        if auth.session_generation != connecting.session_generation || auth.record.is_none() {
            connecting.control.publish(GatewayPublication::Rejected);
            connecting.authority.revoke();
            return Ok(());
        }
        self.sync_cloud_routes(ctx)?;
        let connecting = self
            .connecting_gateways
            .remove(&did)
            .expect("connecting gateway remained present");
        connecting.control.publish(GatewayPublication::Active);
        self.gateways.insert(
            did,
            ActiveGateway {
                did,
                handle: ready.handle,
                authority: ready.authority,
                root_authority: connecting.authority,
                attempt: ready.attempt,
                wire_generation: 0,
                proof,
                control: connecting.control,
                task: connecting.task,
                fact: connecting.fact,
                publication: None,
                desired_dids: BTreeSet::new(),
                selected_dids: BTreeSet::new(),
                refresh_pending: false,
                tokens: BTreeMap::new(),
            },
        );
        if snapshot.status == crate::xiaomi::runtime::AdmissionStatus::SuspendedConflict {
            self.suspend_gateway_routes(ctx, &snapshot);
        } else {
            self.rebuild_gateway_routes(ctx, &snapshot);
        }
        ctx.state.reconcile(&snapshot);
        ctx.status.borrow_mut().admission = snapshot.clone();
        self.schedule_gateway_operations();
        self.update_status(ctx);
        Ok(())
    }

    pub(super) fn finish_gateway_disconnected(
        &mut self,
        ctx: &SessionContext<'_>,
        did: u64,
        attempt: u64,
    ) -> Result<(), XiaomiRuntimeError> {
        if let Some(connecting) = self.connecting_gateways.get_mut(&did) {
            if connecting.attempt != Some(attempt) {
                return Ok(());
            }
            return self.cleanup_disconnected_gateway(ctx, did);
        }
        let network = ctx
            .discovery
            .network()
            .cloned()
            .expect("an active gateway has a current network snapshot");
        let Some(gateway) = self.gateways.remove(&did) else {
            return Ok(());
        };
        if gateway.attempt != attempt {
            self.gateways.insert(did, gateway);
            return Ok(());
        }
        for (device, token) in gateway.tokens {
            ctx.state.source_failed(&token);
            ctx.registry.revoke_gateway(&device);
        }
        let stale = self
            .gateway_routes
            .iter()
            .filter_map(|(device, selected)| (*selected == did).then_some(device.clone()))
            .collect::<Vec<_>>();
        for device in stale {
            self.gateway_routes.remove(&device);
            ctx.registry.revoke_gateway(&device);
        }
        gateway.control.publish(GatewayPublication::Pending);
        self.connecting_gateways.insert(
            did,
            ConnectingGateway {
                candidate: gateway.proof.candidate.clone(),
                network,
                account: gateway.proof.account.clone(),
                session_generation: gateway.proof.session_generation,
                authority: gateway.root_authority,
                control: gateway.control,
                task: gateway.task,
                fact: gateway.fact,
                attempt: Some(attempt),
            },
        );
        self.cleanup_disconnected_gateway(ctx, did)
    }

    pub(super) fn cleanup_disconnected_gateway(
        &mut self,
        ctx: &SessionContext<'_>,
        did: u64,
    ) -> Result<(), XiaomiRuntimeError> {
        self.admission.remove_gateway(did)?;
        let snapshot = self.admission.snapshot()?;
        self.rebuild_gateway_routes(ctx, &snapshot);
        ctx.state.reconcile(&snapshot);
        self.schedule_gateway_operations();
        self.update_status(ctx);
        if let Some(connecting) = self.connecting_gateways.get_mut(&did) {
            connecting.attempt = None;
        }
        Ok(())
    }

    pub(super) fn rebuild_gateway_routes(
        &mut self,
        ctx: &SessionContext<'_>,
        snapshot: &crate::xiaomi::runtime::AdmissionSnapshot,
    ) {
        if snapshot.status == crate::xiaomi::runtime::AdmissionStatus::SuspendedConflict {
            self.suspend_gateway_routes(ctx, snapshot);
            return;
        }
        let mut routes = BTreeMap::new();
        let mut push = BTreeMap::new();
        for feature in &snapshot.features {
            if let Some(path) = feature
                .gateways
                .iter()
                .filter(|path| path.access && self.gateways.contains_key(&path.gateway_did))
                .min_by_key(|path| path.gateway_did)
            {
                routes.insert(feature.identity.physical.clone(), path.gateway_did);
            }
            if let Some(path) = feature
                .gateways
                .iter()
                .filter(|path| path.push && self.gateways.contains_key(&path.gateway_did))
                .min_by_key(|path| path.gateway_did)
            {
                push.insert(feature.identity.physical.clone(), path.gateway_did);
            }
        }
        let old_routes = std::mem::take(&mut self.gateway_routes);
        for (device, old_did) in &old_routes {
            if routes.get(device) != Some(old_did) {
                ctx.registry.revoke_gateway(device);
            }
        }
        for (device, did) in &routes {
            if old_routes.get(device) == Some(did) {
                continue;
            }
            let gateway = self.gateways.get(did).expect("selected gateway is live");
            ctx.registry.install_gateway(
                device.clone(),
                gateway.handle.clone(),
                gateway.authority.clone(),
            );
        }
        self.gateway_routes = routes;
        for gateway in self.gateways.values_mut() {
            let did = gateway.did;
            let desired = push
                .iter()
                .filter(|(_, selected)| **selected == did)
                .map(|(device, _)| device.parent_did.as_str().to_owned())
                .collect();
            gateway.desired_dids = desired;
            gateway.control.set_desired(gateway.desired_dids.clone());
            let stale = gateway
                .tokens
                .keys()
                .filter(|device| push.get(*device) != Some(&did))
                .cloned()
                .collect::<Vec<_>>();
            for device in stale {
                if let Some(token) = gateway.tokens.remove(&device) {
                    ctx.state.source_failed(&token);
                }
            }
        }
    }

    pub(super) fn suspend_gateway_routes(
        &mut self,
        ctx: &SessionContext<'_>,
        snapshot: &crate::xiaomi::runtime::AdmissionSnapshot,
    ) {
        for feature in &snapshot.features {
            ctx.registry.revoke_gateway(&feature.identity.physical);
        }
        self.gateway_routes.clear();
        for gateway in self.gateways.values_mut() {
            for (_, token) in std::mem::take(&mut gateway.tokens) {
                ctx.state.source_failed(&token);
            }
            gateway.desired_dids.clear();
            gateway.control.set_desired(BTreeSet::new());
        }
    }

    pub(super) fn schedule_gateway_operations(&mut self) {
        for gateway in self.gateways.values_mut() {
            gateway.control.set_desired(gateway.desired_dids.clone());
            if gateway.refresh_pending {
                gateway.refresh_pending = false;
                gateway.control.refresh();
            }
        }
    }

    pub(super) fn finish_gateway_operation(
        &mut self,
        ctx: &SessionContext<'_>,
        result: GatewayOperationResult,
    ) -> Result<(), XiaomiRuntimeError> {
        match result {
            GatewayOperationResult::Selected {
                did,
                desired,
                result,
            } => {
                let Some(gateway) = self.gateways.get_mut(&did) else {
                    return Ok(());
                };
                gateway.publication = None;
                let generation = match result {
                    Ok(generation) => generation,
                    Err(error)
                        if error.kind()
                            == &crate::xiaomi::gateway::GatewayErrorKind::Superseded =>
                    {
                        return Ok(());
                    }
                    Err(error) => {
                        ctx.record_boundary(
                            XiaomiRuntimeComponent::Gateway,
                            XiaomiFailureStage::Subscribe,
                            gateway_failure_code(error.kind()),
                            Some(did.to_string()),
                        );
                        gateway.selected_dids.clear();
                        for (_, token) in std::mem::take(&mut gateway.tokens) {
                            ctx.state.source_failed(&token);
                        }
                        self.select_fallback_push_sources(ctx)?;
                        return Ok(());
                    }
                };
                let snapshot = if gateway.desired_dids == desired {
                    Some(self.admission.snapshot()?)
                } else {
                    None
                };
                let generation_changed = gateway.wire_generation != generation;
                gateway.selected_dids = desired.clone();
                gateway.wire_generation = generation;
                if let Some(snapshot) = snapshot {
                    if generation_changed {
                        for (_, token) in std::mem::take(&mut gateway.tokens) {
                            ctx.state.source_failed(&token);
                        }
                    }
                    let physical = snapshot
                        .features
                        .iter()
                        .filter(|feature| {
                            desired.contains(feature.identity.physical.parent_did.as_str())
                        })
                        .map(|feature| feature.identity.physical.clone())
                        .collect::<BTreeSet<_>>();
                    for device in physical {
                        if gateway.tokens.contains_key(&device) {
                            continue;
                        }
                        if let Some(token) = ctx.state.select_push_source(
                            &device,
                            PushSource::Gateway(did),
                            did,
                            generation,
                        ) {
                            ctx.state.acknowledge(&token, did, generation);
                            gateway.tokens.insert(device, token);
                        }
                    }
                }
            }
            GatewayOperationResult::Refreshed { did, result } => {
                let Some(gateway) = self.gateways.get_mut(&did) else {
                    return Ok(());
                };
                gateway.publication = None;
                let evidence = match result {
                    Ok(evidence) => evidence,
                    Err(error) => {
                        ctx.record_boundary(
                            XiaomiRuntimeComponent::Gateway,
                            XiaomiFailureStage::Read,
                            gateway_failure_code(error.kind()),
                            Some(did.to_string()),
                        );
                        self.drop_gateway(ctx, did)?;
                        return Ok(());
                    }
                };
                gateway.proof.evidence = evidence;
                let Some(catalog) = self.catalog.as_ref() else {
                    return Ok(());
                };
                if let Err(error) = self
                    .admission
                    .observe_gateway(gateway.proof.clone(), catalog)
                {
                    self.discard_gateway_after_admission_failure(ctx, did);
                    return Err(error.into());
                }
                let snapshot = self.admission.snapshot()?;
                self.rebuild_gateway_routes(ctx, &snapshot);
                self.sync_cloud_routes(ctx)?;
                ctx.state.reconcile(&snapshot);
                ctx.status.borrow_mut().admission = snapshot.clone();
            }
        }
        self.schedule_gateway_operations();
        self.update_status(ctx);
        Ok(())
    }

    pub(super) fn apply_gateway_notification(
        &mut self,
        ctx: &SessionContext<'_>,
        gateway_did: u64,
        notification: GatewayNotification,
    ) -> Result<(), XiaomiRuntimeError> {
        let Some(gateway) = self.gateways.get(&gateway_did) else {
            return Ok(());
        };
        match notification {
            GatewayNotification::DeviceListChanged { .. } => {
                if let Some(gateway) = self.gateways.get_mut(&gateway_did) {
                    gateway.refresh_pending = true;
                }
                self.schedule_gateway_operations();
                ctx.refresh();
            }
            GatewayNotification::Property {
                did,
                siid,
                piid,
                value,
                generation,
                ..
            } => {
                if let Some((_, token)) = gateway
                    .tokens
                    .iter()
                    .find(|(device, _)| device.parent_did.as_str() == did)
                {
                    ctx.state.apply_property(
                        token,
                        gateway.did,
                        generation,
                        siid,
                        piid,
                        value.as_ref(),
                        unix_time(),
                        false,
                    );
                }
            }
            GatewayNotification::Event {
                did,
                siid,
                eiid,
                arguments,
                generation,
                ..
            } => {
                if let Some((_, token)) = gateway
                    .tokens
                    .iter()
                    .find(|(device, _)| device.parent_did.as_str() == did)
                {
                    match arguments {
                        EventArguments::Keyed(arguments) => {
                            ctx.state.apply_keyed_event(
                                token,
                                gateway.did,
                                generation,
                                siid,
                                eiid,
                                &arguments,
                                unix_time(),
                                false,
                            );
                        }
                        EventArguments::Positional(arguments) => {
                            ctx.state.apply_positional_event(
                                token,
                                gateway.did,
                                generation,
                                siid,
                                eiid,
                                &arguments,
                                unix_time(),
                                false,
                            );
                        }
                    }
                }
            }
        }
        Ok(())
    }

    pub(super) fn drop_gateway(
        &mut self,
        ctx: &SessionContext<'_>,
        did: u64,
    ) -> Result<(), XiaomiRuntimeError> {
        let Some(gateway) = self.gateways.remove(&did) else {
            return Ok(());
        };
        gateway.control.stop();
        gateway.authority.revoke();
        gateway.root_authority.revoke();
        for (device, token) in gateway.tokens {
            ctx.state.source_failed(&token);
            ctx.registry.revoke_gateway(&device);
        }
        self.admission.remove_gateway(gateway.did)?;
        let snapshot = self.admission.snapshot()?;
        self.rebuild_gateway_routes(ctx, &snapshot);
        ctx.state.reconcile(&snapshot);
        self.schedule_gateway_operations();
        self.update_status(ctx);
        Ok(())
    }

    pub(super) fn discard_gateway_after_admission_failure(
        &mut self,
        ctx: &SessionContext<'_>,
        did: u64,
    ) {
        let Some(gateway) = self.gateways.remove(&did) else {
            return;
        };
        gateway.control.stop();
        gateway.authority.revoke();
        gateway.root_authority.revoke();
        for (device, token) in gateway.tokens {
            ctx.state.source_failed(&token);
            ctx.registry.revoke_gateway(&device);
        }
        let stale = self
            .gateway_routes
            .iter()
            .filter_map(|(device, selected)| (*selected == did).then_some(device.clone()))
            .collect::<Vec<_>>();
        for device in stale {
            self.gateway_routes.remove(&device);
            ctx.registry.revoke_gateway(&device);
        }
        let _ = self.admission.remove_gateway(did);
    }

    pub(super) fn drop_all_gateways(
        &mut self,
        ctx: &SessionContext<'_>,
    ) -> Result<(), XiaomiRuntimeError> {
        for (_, connecting) in std::mem::take(&mut self.connecting_gateways) {
            connecting.control.stop();
            connecting.authority.revoke();
        }
        let dids = self.gateways.keys().copied().collect::<Vec<_>>();
        for did in dids {
            self.drop_gateway(ctx, did)?;
        }
        Ok(())
    }
}
