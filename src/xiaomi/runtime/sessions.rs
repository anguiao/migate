mod catalog;
mod cloud;
mod gateway;
mod lan;

use super::cloud_connection::{
    CloudConnectionControl, CloudConnectionFact, CloudConnectionFactResult, CloudSelectionResult,
    cloud_connection_fact,
};
use super::connection::ConnectionSetup;
#[cfg(test)]
use super::gateway_connection::active_gateway_connection_lifetime;
use super::gateway_connection::{
    GatewayConnectionControl, GatewayConnectionFact, GatewayConnectionFactResult,
    GatewayOperationResult, gateway_connection_fact,
};
use super::lan_connection::{
    LanAttemptResources, LanConnectionControl, LanConnectionFact, LanConnectionFactResult,
    lan_connection_fact,
};
use super::{
    XiaomiBoundaryDiagnostic, XiaomiCandidateState, XiaomiCandidateStatus, XiaomiFailureStage,
    XiaomiGatewayStatus, XiaomiRuntimeComponent, XiaomiRuntimeDiagnostic, XiaomiRuntimeError,
    XiaomiRuntimeStatus, XiaomiSafeFailureCode, auth::AuthMaintenance, discovery::NetworkDiscovery,
    status::record_diagnostic,
};
use crate::{
    device::PhysicalDeviceId,
    storage::AuthSnapshot,
    xiaomi::{
        auth::AuthReport,
        cloud::CloudClient,
        discovery::{GatewayCandidate, NetworkEpoch, NetworkUpdate},
        lan::LanTarget,
        runtime::{
            AdmissionController, AuthenticatedGateway, CloudEvidence, CloudStatus,
            CurrentSessionRegistry, SessionAuthority, StateRuntime, SubscriptionToken,
        },
    },
};
use event_listener::Event;
use futures_util::{
    FutureExt,
    future::{LocalBoxFuture, select_all},
};
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet, VecDeque},
    rc::Rc,
};

/// Read-only inputs and effect sinks needed to publish device sessions.
/// Session policy cannot mutate authentication or discovery task ownership.
pub(super) struct SessionContext<'a> {
    pub(super) auth: &'a AuthMaintenance,
    pub(super) discovery: &'a NetworkDiscovery,
    pub(super) cloud: &'a Rc<CloudClient>,
    pub(super) state: &'a StateRuntime,
    pub(super) registry: &'a CurrentSessionRegistry,
    pub(super) status: &'a RefCell<XiaomiRuntimeStatus>,
    pub(super) diagnostics: &'a RefCell<VecDeque<XiaomiRuntimeDiagnostic>>,
    pub(super) wake: &'a Rc<Event>,
    pub(super) refresh_requested: &'a Cell<bool>,
    pub(super) devices: &'a crate::storage::DeviceStore,
    pub(super) certificate_expires: i64,
}

impl SessionContext<'_> {
    fn gateway_authority(&self) -> SessionAuthority {
        SessionAuthority::new_until(self.certificate_expires)
    }
    fn runner_path(&self) -> &std::path::Path {
        self.devices.path()
    }
    fn devices(&self) -> crate::storage::DeviceStore {
        self.devices.clone()
    }
    fn record_boundary(
        &self,
        component: XiaomiRuntimeComponent,
        stage: XiaomiFailureStage,
        code: XiaomiSafeFailureCode,
        subject: Option<String>,
    ) {
        record_diagnostic(
            self.diagnostics,
            XiaomiRuntimeDiagnostic::Boundary(XiaomiBoundaryDiagnostic {
                component,
                stage,
                code,
                subject,
            }),
        );
    }
    fn refresh(&self) {
        let devices = self
            .status
            .borrow()
            .admission
            .features
            .iter()
            .map(|feature| feature.identity.physical.clone())
            .collect::<BTreeSet<_>>();
        for device in devices {
            self.state.request_refresh(&device);
        }
        self.refresh_requested.set(true);
        self.wake.notify(usize::MAX);
    }
}

/// Owns admission, active connection handles, and route/publication selection.
pub(super) struct DeviceSessions {
    admission: AdmissionController,
    catalog: Option<super::AdmissionCatalog>,
    connecting_gateways: BTreeMap<u64, ConnectingGateway>,
    gateway_setup: ConnectionSetup,
    gateways: BTreeMap<u64, ActiveGateway>,
    gateway_routes: BTreeMap<PhysicalDeviceId, u64>,
    lans: BTreeMap<PhysicalDeviceId, ActiveLan>,
    lan_setup: ConnectionSetup,
    cloud_notifications: Option<ActiveCloudNotifications>,
    cloud_authority: Option<SessionAuthority>,
    cloud_validated: bool,
    cloud_routes: BTreeSet<PhysicalDeviceId>,
}

struct ConnectingGateway {
    candidate: GatewayCandidate,
    network: NetworkUpdate,
    account: crate::device::AccountId,
    session_generation: crate::storage::AuthSessionGeneration,
    authority: SessionAuthority,
    control: GatewayConnectionControl,
    task: LocalBoxFuture<'static, ()>,
    fact: LocalBoxFuture<'static, GatewayConnectionFactResult>,
    attempt: Option<u64>,
}

struct ActiveGateway {
    did: u64,
    handle: crate::xiaomi::gateway::GatewayHandle,
    authority: SessionAuthority,
    root_authority: SessionAuthority,
    attempt: u64,
    wire_generation: u64,
    proof: AuthenticatedGateway,
    control: GatewayConnectionControl,
    task: LocalBoxFuture<'static, ()>,
    fact: LocalBoxFuture<'static, GatewayConnectionFactResult>,
    publication: Option<LocalBoxFuture<'static, GatewayOperationResult>>,
    desired_dids: BTreeSet<String>,
    selected_dids: BTreeSet<String>,
    refresh_pending: bool,
    tokens: BTreeMap<PhysicalDeviceId, SubscriptionToken>,
}

struct ActiveLan {
    targets: Vec<LanTarget>,
    control: LanConnectionControl,
    current: Option<(u64, Rc<LanAttemptResources>)>,
    task: LocalBoxFuture<'static, ()>,
    fact: LocalBoxFuture<'static, LanConnectionFactResult>,
}

struct ActiveCloudNotifications {
    authority: SessionAuthority,
    control: CloudConnectionControl,
    task: LocalBoxFuture<'static, ()>,
    fact: LocalBoxFuture<'static, CloudConnectionFactResult>,
    desired: BTreeSet<String>,
    generation: u64,
    session_id: u64,
    tokens: BTreeMap<PhysicalDeviceId, SubscriptionToken>,
    publication: Option<LocalBoxFuture<'static, CloudSelectionResult>>,
}

pub(super) enum SessionEvent {
    GatewayStopped(u64),
    GatewayFact(u64, GatewayConnectionFactResult),
    GatewayPublication(GatewayOperationResult),
    LanStopped(PhysicalDeviceId),
    LanFact(PhysicalDeviceId, LanConnectionFactResult),
    CloudStopped,
    CloudFact(CloudConnectionFactResult),
    CloudPublication(CloudSelectionResult),
}

impl DeviceSessions {
    pub(super) fn new(admission: AdmissionController) -> Self {
        Self {
            admission,
            catalog: None,
            connecting_gateways: BTreeMap::new(),
            gateway_setup: ConnectionSetup::new(4),
            gateways: BTreeMap::new(),
            gateway_routes: BTreeMap::new(),
            lans: BTreeMap::new(),
            lan_setup: ConnectionSetup::new(4),
            cloud_notifications: None,
            cloud_authority: None,
            cloud_validated: false,
            cloud_routes: BTreeSet::new(),
        }
    }

    pub(super) fn schedule(&mut self, ctx: &SessionContext<'_>) -> Result<(), XiaomiRuntimeError> {
        self.schedule_gateway(ctx)?;
        self.schedule_lan(ctx)?;
        self.schedule_cloud_notifications(ctx)?;
        self.update_lan_push_policy();
        Ok(())
    }

    pub(super) async fn next_event(&mut self) -> SessionEvent {
        let mut waits = Vec::<LocalBoxFuture<'_, SessionEvent>>::new();
        if let Some(cloud) = self.cloud_notifications.as_mut() {
            waits.push(
                cloud
                    .task
                    .as_mut()
                    .map(|_| SessionEvent::CloudStopped)
                    .boxed_local(),
            );
            if let Some(publication) = cloud.publication.as_mut() {
                waits.push(
                    publication
                        .as_mut()
                        .map(SessionEvent::CloudPublication)
                        .boxed_local(),
                );
            }
            waits.push(
                cloud
                    .fact
                    .as_mut()
                    .map(SessionEvent::CloudFact)
                    .boxed_local(),
            );
        }
        for (device, lan) in &mut self.lans {
            let stopped = device.clone();
            waits.push(
                lan.task
                    .as_mut()
                    .map(move |_| SessionEvent::LanStopped(stopped))
                    .boxed_local(),
            );
            let fact_device = device.clone();
            waits.push(
                lan.fact
                    .as_mut()
                    .map(move |event| SessionEvent::LanFact(fact_device, event))
                    .boxed_local(),
            );
        }
        for (&did, gateway) in &mut self.gateways {
            waits.push(
                gateway
                    .task
                    .as_mut()
                    .map(move |_| SessionEvent::GatewayStopped(did))
                    .boxed_local(),
            );
            waits.push(
                gateway
                    .fact
                    .as_mut()
                    .map(move |fact| SessionEvent::GatewayFact(did, fact))
                    .boxed_local(),
            );
            if let Some(publication) = gateway.publication.as_mut() {
                waits.push(
                    publication
                        .as_mut()
                        .map(SessionEvent::GatewayPublication)
                        .boxed_local(),
                );
            }
        }
        for (&did, gateway) in &mut self.connecting_gateways {
            waits.push(
                gateway
                    .task
                    .as_mut()
                    .map(move |_| SessionEvent::GatewayStopped(did))
                    .boxed_local(),
            );
            waits.push(
                gateway
                    .fact
                    .as_mut()
                    .map(move |fact| SessionEvent::GatewayFact(did, fact))
                    .boxed_local(),
            );
        }
        if waits.is_empty() {
            std::future::pending().await
        } else {
            select_all(waits).await.0
        }
    }

    pub(super) fn handle_event(
        &mut self,
        ctx: &SessionContext<'_>,
        event: SessionEvent,
    ) -> Result<(), XiaomiRuntimeError> {
        match event {
            SessionEvent::GatewayStopped(did) => {
                if self.gateways.contains_key(&did) {
                    self.drop_gateway(ctx, did)?;
                } else if let Some(connecting) = self.connecting_gateways.remove(&did) {
                    connecting.control.stop();
                    connecting.authority.revoke();
                }
            }
            SessionEvent::GatewayFact(did, (receiver, fact)) => {
                if let Some(gateway) = self.gateways.get_mut(&did) {
                    gateway.fact = gateway_connection_fact(receiver);
                } else if let Some(gateway) = self.connecting_gateways.get_mut(&did) {
                    gateway.fact = gateway_connection_fact(receiver);
                }
                match fact {
                    Ok(GatewayConnectionFact::Ready(ready)) => {
                        self.finish_gateway_ready(ctx, did, *ready)?
                    }
                    Ok(GatewayConnectionFact::Disconnected { attempt }) => {
                        self.finish_gateway_disconnected(ctx, did, attempt)?
                    }
                    Ok(GatewayConnectionFact::Notification(notification)) => {
                        self.apply_gateway_notification(ctx, did, notification)?
                    }
                    Ok(GatewayConnectionFact::Operation(result)) => {
                        self.finish_gateway_operation(ctx, result)?
                    }
                    Ok(GatewayConnectionFact::Failure { stage, code }) => ctx.record_boundary(
                        XiaomiRuntimeComponent::Gateway,
                        stage,
                        code,
                        Some(did.to_string()),
                    ),
                    Err(_) => self.drop_gateway(ctx, did)?,
                }
            }
            SessionEvent::GatewayPublication(result) => {
                self.finish_gateway_operation(ctx, result)?
            }
            SessionEvent::LanStopped(device) => self.drop_lan(ctx, &device)?,
            SessionEvent::LanFact(device, (receiver, fact)) => {
                if let Some(lan) = self.lans.get_mut(&device) {
                    lan.fact = lan_connection_fact(receiver);
                }
                match fact {
                    Ok(LanConnectionFact::Ready(ready)) => {
                        self.finish_lan_ready(ctx, &device, *ready)?
                    }
                    Ok(LanConnectionFact::Disconnected {
                        generation,
                        attempt,
                    }) => self.finish_lan_disconnected(ctx, &device, generation, attempt)?,
                    Ok(LanConnectionFact::Failure { stage, code }) => ctx.record_boundary(
                        XiaomiRuntimeComponent::Lan,
                        stage,
                        code,
                        Some(device.parent_did.as_str().to_owned()),
                    ),
                    Err(_) => self.drop_lan(ctx, &device)?,
                }
            }
            SessionEvent::CloudStopped => self.drop_cloud_notifications(ctx),
            SessionEvent::CloudFact((receiver, fact)) => {
                if let Some(cloud) = self.cloud_notifications.as_mut() {
                    cloud.fact = cloud_connection_fact(receiver);
                }
                match fact {
                    Ok(CloudConnectionFact::Notification(notification)) => {
                        self.apply_cloud_notification(ctx, notification)?
                    }
                    Ok(CloudConnectionFact::Selection(selection)) => {
                        self.finish_cloud_selection(ctx, selection)?
                    }
                    Ok(CloudConnectionFact::Disconnected) => self.finish_cloud_disconnected(ctx)?,
                    Ok(CloudConnectionFact::Failure { stage, code }) => {
                        ctx.record_boundary(XiaomiRuntimeComponent::Cloud, stage, code, None)
                    }
                    Err(_) => self.drop_cloud_notifications(ctx),
                }
            }
            SessionEvent::CloudPublication(selection) => {
                self.finish_cloud_selection(ctx, selection)?
            }
        }
        Ok(())
    }

    pub(super) fn reset_auth(
        &mut self,
        ctx: &SessionContext<'_>,
        snapshot: &AuthSnapshot,
    ) -> Result<(), XiaomiRuntimeError> {
        ctx.registry.revoke_all();
        self.cloud_routes.clear();
        if let Some(authority) = self.cloud_authority.take() {
            authority.revoke();
        }
        self.drop_all_gateways(ctx)?;
        self.drop_all_lans(ctx)?;
        self.drop_cloud_notifications(ctx);
        self.catalog = None;
        self.cloud_validated = false;
        self.admission.observe_auth(snapshot)?;
        self.reconcile(ctx, &self.admission)
    }

    pub(super) fn network_changed(
        &mut self,
        ctx: &SessionContext<'_>,
        epoch: NetworkEpoch,
        available: bool,
    ) -> Result<(), XiaomiRuntimeError> {
        self.revoke_network(ctx)?;
        self.admission.invalidate(epoch);
        if available {
            self.sync_cloud_routes(ctx)?;
        }
        self.reconcile(ctx, &self.admission)
    }

    pub(super) fn apply_auth_revision(
        &mut self,
        ctx: &SessionContext<'_>,
        previous: &AuthSnapshot,
    ) -> Result<(), XiaomiRuntimeError> {
        let previous = previous.record.as_ref();
        let current = ctx.auth.snapshot().record.as_ref();
        let token_changed = previous.map(|record| &record.tokens.access_token)
            != current.map(|record| &record.tokens.access_token);
        let gateway_identity_changed = previous.map(|record| {
            (
                &record.virtual_did,
                &record.private_key_pem,
                &record.certificate_pem,
            )
        }) != current.map(|record| {
            (
                &record.virtual_did,
                &record.private_key_pem,
                &record.certificate_pem,
            )
        });
        if token_changed && let Some(account) = self.catalog.as_ref().map(|c| c.account.clone()) {
            self.drop_cloud_notifications(ctx);
            self.cloud_validated = true;
            self.admission.observe_cloud(&CloudEvidence {
                account,
                session_generation: ctx.auth.snapshot().session_generation,
                status: CloudStatus::Ready,
            })?;
            self.sync_cloud_routes(ctx)?;
            self.reconcile(ctx, &self.admission)?;
        }
        if gateway_identity_changed {
            self.drop_all_gateways(ctx)?;
        }
        Ok(())
    }

    pub(super) fn apply_auth_report(
        &mut self,
        ctx: &SessionContext<'_>,
        report: &AuthReport,
    ) -> Result<(), XiaomiRuntimeError> {
        if matches!(
            report.authentication,
            crate::xiaomi::auth::AuthenticationState::SignInRequired(_)
        ) {
            if let Some(catalog) = self.catalog.as_ref() {
                self.admission.observe_cloud(&CloudEvidence {
                    account: catalog.account.clone(),
                    session_generation: catalog.session_generation,
                    status: CloudStatus::InvalidToken,
                })?;
                let snapshot = self.admission.snapshot()?;
                for feature in &snapshot.features {
                    ctx.registry.revoke_cloud(&feature.identity.physical);
                }
                ctx.state.reconcile(&snapshot);
                ctx.status.borrow_mut().admission = snapshot;
            }
            self.cloud_routes.clear();
            if let Some(authority) = self.cloud_authority.take() {
                authority.revoke();
            }
            self.cloud_validated = false;
        }
        let certificate_current = report
            .certificate
            .is_some_and(|validity| validity.currently_valid(report.completed_at()));
        if !certificate_current {
            self.drop_all_gateways(ctx)?;
        }
        Ok(())
    }

    pub(super) fn drain_transport_failures(
        &mut self,
        ctx: &SessionContext<'_>,
    ) -> Result<(), XiaomiRuntimeError> {
        for failure in ctx.registry.drain_route_failures() {
            match failure {
                super::RouteFailure::Lan { device, lease } => {
                    if ctx.registry.revoke_lan_if_authority(&device, &lease) {
                        self.reconnect_lan(ctx, &device)?;
                    }
                }
                super::RouteFailure::CloudUnauthorized { device, lease } => {
                    if !ctx.registry.revoke_cloud_if_authority(&device, &lease) {
                        continue;
                    }
                    self.cloud_routes.remove(&device);
                    if let Some(catalog) = self.catalog.as_ref() {
                        self.admission.observe_cloud(&CloudEvidence {
                            account: catalog.account.clone(),
                            session_generation: ctx.auth.snapshot().session_generation,
                            status: CloudStatus::InvalidToken,
                        })?;
                        self.reconcile(ctx, &self.admission)?;
                    }
                    ctx.refresh_requested.set(true);
                }
            }
        }
        Ok(())
    }

    fn revoke_network(&mut self, ctx: &SessionContext<'_>) -> Result<(), XiaomiRuntimeError> {
        self.drop_all_gateways(ctx)?;
        self.drop_all_lans(ctx)?;
        self.drop_cloud_notifications(ctx);
        Ok(())
    }

    fn reconcile(
        &self,
        ctx: &SessionContext<'_>,
        admission: &AdmissionController,
    ) -> Result<(), XiaomiRuntimeError> {
        let snapshot = admission.snapshot()?;
        ctx.state.reconcile(&snapshot);
        ctx.status.borrow_mut().admission = snapshot;
        Ok(())
    }

    pub(super) fn update_status(&mut self, ctx: &SessionContext<'_>) {
        let admission = ctx.status.borrow().admission.clone();
        let candidates = self
            .catalog
            .as_ref()
            .map(|catalog| {
                catalog
                    .catalog
                    .devices
                    .iter()
                    .map(|device| {
                        let admitted = admission.features.iter().any(|feature| {
                            feature.identity.physical.account == catalog.account
                                && feature.identity.physical.home.as_str() == device.home_id
                                && feature.identity.physical.parent_did.as_str()
                                    == device.parent_did
                        });
                        let state = if admission.status == super::AdmissionStatus::SuspendedConflict
                            && admitted
                        {
                            XiaomiCandidateState::Conflict
                        } else if device.features.is_empty() {
                            if device.spec_type.as_ref().is_some_and(|type_urn| {
                                catalog.specifications.contains_key(type_urn)
                                    && !crate::xiaomi::catalog::supports_spec(
                                        &device.model,
                                        type_urn,
                                    )
                            }) {
                                XiaomiCandidateState::Unsupported
                            } else {
                                XiaomiCandidateState::Unrecognized
                            }
                        } else if admitted && admission.status == super::AdmissionStatus::Active {
                            XiaomiCandidateState::Ready
                        } else {
                            XiaomiCandidateState::WaitingForGateway
                        };
                        XiaomiCandidateStatus {
                            account_id: catalog.account.as_str().to_owned(),
                            home_id: device.home_id.clone(),
                            did: device.parent_did.clone(),
                            parent_did: device.parent_did.clone(),
                            name: device.name.clone(),
                            model: device.model.clone(),
                            state,
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        let gateways = ctx
            .discovery
            .candidates()
            .into_iter()
            .map(|candidate| XiaomiGatewayStatus {
                did: candidate.gateway_did,
                home_group: candidate.home_group,
                unverified: candidate.unverified,
                authenticated: self.gateways.contains_key(&candidate.gateway_did),
            })
            .collect();
        let mut status = ctx.status.borrow_mut();
        status.candidates = candidates;
        status.gateways = gateways;
    }
}

#[cfg(test)]
fn active_gateway(
    proof: AuthenticatedGateway,
    authority: SessionAuthority,
    parts: super::GatewayRuntimeParts,
) -> ActiveGateway {
    let did = parts.evidence.gateway_did;
    let control = GatewayConnectionControl::new();
    let (fact_sender, fact_receiver) = flume::bounded(256);
    ActiveGateway {
        did,
        handle: parts.handle.clone(),
        authority: authority.clone(),
        root_authority: authority.clone(),
        attempt: 1,
        wire_generation: 0,
        proof,
        task: active_gateway_connection_lifetime(
            did,
            parts.handle,
            authority,
            parts.notifications,
            parts.task,
            control.clone(),
            fact_sender,
        ),
        fact: gateway_connection_fact(fact_receiver),
        publication: None,
        control,
        desired_dids: BTreeSet::new(),
        selected_dids: BTreeSet::new(),
        refresh_pending: false,
        tokens: BTreeMap::new(),
    }
}

#[cfg(test)]
mod tests;
