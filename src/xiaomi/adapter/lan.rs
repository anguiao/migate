use super::{
    model::{AdapterError, LanProtocol, LanState, LanStatus, RecoveryStatus},
    state::{Backoff, Context, Lifetime},
};
use crate::{
    configuration::Scope,
    storage::LanRecord,
    xiaomi::{
        capability::{NativeDescription, PropertyBinding},
        cloud::{LanCandidate, LanCredentials},
        lan::{Client, Failure, RequestError, SendStage},
        legacy_miio::Mapping,
    },
};
use futures_lite::future;
use std::{
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    rc::Rc,
    time::Duration,
};

mod identity;
mod properties;

use identity::probe;
pub(super) use properties::{read_property, set_properties};

pub(super) const MODEL: &str = "dmaker.fan.p5c";

pub(super) fn protocol(native: &NativeDescription) -> Option<LanProtocol> {
    if native.model() == MODEL {
        Some(LanProtocol::Miot)
    } else if native.legacy_profile().is_some() {
        Some(LanProtocol::LegacyMcn02)
    } else {
        None
    }
}

#[derive(Clone, Copy)]
struct LanProfile {
    binding: PropertyBinding,
    protocol: LanProtocol,
    mapping: Option<&'static Mapping>,
}
#[cfg(not(test))]
const PORT: u16 = 54321;

#[derive(Default)]
pub(super) struct LanSlot {
    pub(super) status: Option<LanStatus>,
    pub(super) active: Option<(LanRecord, Client)>,
    desired: Option<LanRecord>,
    task: Option<Rc<Lifetime>>,
}

pub(super) async fn watch_network(
    context: Rc<Context>,
    discovery: crate::xiaomi::discovery::Discovery,
) -> Result<(), AdapterError> {
    let mut changes = discovery.subscribe();
    loop {
        changes.changed().await?;
        context.lan_network_changed.notify(usize::MAX);
    }
}

pub(super) async fn run(context: Rc<Context>) -> Result<(), AdapterError> {
    loop {
        let changed = context.changed.listen();
        let dids = context
            .live
            .borrow()
            .devices
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        for did in dids {
            context.reconcile_lan(&did)?;
        }
        changed.await;
    }
}

impl Context {
    fn lan_port(&self) -> u16 {
        #[cfg(test)]
        {
            self.lan_port.get()
        }
        #[cfg(not(test))]
        {
            PORT
        }
    }
    fn lan_endpoint(&self, address: std::net::Ipv4Addr) -> SocketAddr {
        #[cfg(test)]
        if let Some(endpoint) = self.lan_test_endpoints.borrow().get(&address) {
            return *endpoint;
        }
        SocketAddrV4::new(address, self.lan_port()).into()
    }
    fn lan_timeout(&self) -> Duration {
        #[cfg(test)]
        {
            self.lan_timeout.get()
        }
        #[cfg(not(test))]
        {
            Duration::from_secs(10)
        }
    }
    pub(super) fn legacy_mapping(&self, did: &str) -> Option<&'static Mapping> {
        let live = self.live.borrow();
        live.devices
            .get(did)?
            .committed
            .as_ref()?
            .native
            .as_ref()?
            .legacy_profile()
            .map(|profile| profile.mapping())
    }

    pub(super) fn invalidate_lan_scope(&self) -> Result<(), AdapterError> {
        self.lan_scope_invalid.set(true);
        let (scope, dids) = {
            let live = self.live.borrow();
            (
                live.scope.clone(),
                live.devices.keys().cloned().collect::<Vec<_>>(),
            )
        };
        for did in dids {
            self.stop_lan(&did);
        }
        if let Some(scope) = scope {
            self.store.lan().remove_scope(&scope)?;
        }
        self.update_all_availability();
        self.update_routing("direct LAN scope revoked");
        self.refresh_changed.notify(usize::MAX);
        self.changed.notify(usize::MAX);
        Ok(())
    }
    pub(super) fn stop_lan(&self, did: &str) -> bool {
        let mut was_active = false;
        if let Some(slot) = self.live.borrow_mut().devices.get_mut(did) {
            if let Some(task) = slot.lan.task.take() {
                task.close();
            }
            was_active = slot.lan.active.take().is_some();
            slot.lan.desired = None;
            slot.lan.status = None;
        }
        if was_active {
            self.refresh_changed.notify(usize::MAX);
        }
        was_active
    }

    /// Start work only for the installed, non-retired member of the saved scope.
    pub(super) fn reconcile_lan(self: &Rc<Self>, did: &str) -> Result<(), AdapterError> {
        self.reconcile_lan_after(did, None)
    }

    fn reconcile_lan_after(
        self: &Rc<Self>,
        did: &str,
        failure: Option<RequestError>,
    ) -> Result<(), AdapterError> {
        let eligible = {
            let live = self.live.borrow();
            let Some(slot) = live.devices.get(did) else {
                return Ok(());
            };
            let Some(scope) = live.scope.clone() else {
                return Ok(());
            };
            let Some(work) = &slot.admitted else {
                return Ok(());
            };
            let Some(source) = &slot.committed else {
                return Ok(());
            };
            let native = source.native.as_ref();
            if self.lan_scope_invalid.get()
                || slot.group
                || work.life.is_closed()
                || native.and_then(protocol).is_none()
                || slot
                    .cloud_access
                    .as_ref()
                    .is_some_and(|info| info.model != source.raw.model)
            {
                None
            } else {
                let model = &source.raw.model;
                let native = native.unwrap();
                let legacy = native.legacy_profile();
                let binding = native.device().functions().iter().find_map(|function| {
                    if let Some(profile) = legacy {
                        (function.id() == profile.target())
                            .then(|| function.air_conditioner().unwrap().power())
                    } else {
                        function.fan().map(|fan| fan.power())
                    }
                });
                let cloud = slot
                    .cloud_access
                    .as_ref()
                    .filter(|info| info.model == *model)
                    .map(|info| info.lan.clone());
                binding.map(|binding| {
                    (
                        scope,
                        LanProfile {
                            binding,
                            protocol: protocol(native).unwrap(),
                            mapping: legacy.map(|profile| profile.mapping()),
                        },
                        cloud,
                        model.clone(),
                    )
                })
            }
        };
        let Some((scope, profile, cloud, model)) = eligible else {
            if self.stop_lan(did) {
                self.update_availability(did);
                self.update_routing("direct LAN eligibility ended");
                self.changed.notify(usize::MAX);
            }
            return Ok(());
        };
        if matches!(cloud, Some(LanCredentials::NotIp)) {
            if self.live.borrow().devices[did]
                .lan
                .status
                .as_ref()
                .is_some_and(|status| status.state == LanState::Ineligible)
            {
                return Ok(());
            }
            self.stop_lan(did);
            self.store.lan().remove(did)?;
            self.live
                .borrow_mut()
                .devices
                .get_mut(did)
                .unwrap()
                .lan
                .status = Some(LanStatus {
                protocol: profile.protocol,
                state: LanState::Ineligible,
                address: None,
                problem: None,
                recovery: RecoveryStatus::None,
            });
            self.status_changed.notify(usize::MAX);
            self.update_availability(did);
            self.update_routing("device is not eligible for direct LAN");
            self.changed.notify(usize::MAX);
            return Ok(());
        }
        let stored = self
            .store
            .lan()
            .load(&scope, did)?
            .filter(|record| record.model == model);
        let candidate = match &cloud {
            Some(LanCredentials::Candidate(LanCandidate { address, token })) => Some(LanRecord {
                scope: scope.clone(),
                did: did.into(),
                model: model.clone(),
                address: *address,
                token: token.clone(),
            }),
            _ => stored.clone(),
        };
        let Some(candidate) = candidate else {
            let state = if matches!(cloud, Some(LanCredentials::Invalid)) {
                LanState::InvalidCredentials
            } else {
                LanState::MissingCredentials
            };
            let mut live = self.live.borrow_mut();
            let lan = &mut live.devices.get_mut(did).unwrap().lan;
            let was_active = lan.active.is_some();
            if let Some(task) = lan.task.take() {
                task.close();
            }
            lan.active = None;
            lan.desired = None;
            lan.status = Some(LanStatus {
                protocol: profile.protocol,
                state,
                address: None,
                problem: None,
                recovery: RecoveryStatus::None,
            });
            drop(live);
            if was_active {
                self.update_availability(did);
                self.update_routing("direct LAN credentials unavailable");
                self.refresh_changed.notify(usize::MAX);
                self.changed.notify(usize::MAX);
            }
            self.status_changed.notify(usize::MAX);
            return Ok(());
        };
        if did
            .parse::<u64>()
            .ok()
            .filter(|did| *did != 0 && *did != u64::MAX)
            .is_none()
        {
            self.stop_lan(did);
            return Ok(());
        }
        let (life, active) = {
            let mut live = self.live.borrow_mut();
            let lan = &mut live.devices.get_mut(did).unwrap().lan;
            if lan.desired.as_ref() == Some(&candidate) {
                return Ok(());
            }
            if let Some(task) = lan.task.take() {
                task.close();
            }
            let life = Rc::new(Lifetime::default());
            lan.task = Some(life.clone());
            lan.desired = Some(candidate.clone());
            let active = lan.active.is_some();
            if !active {
                lan.status = Some(LanStatus {
                    protocol: profile.protocol,
                    state: LanState::Authenticating,
                    address: Some(candidate.address),
                    problem: None,
                    recovery: RecoveryStatus::Running,
                });
            }
            (life, active)
        };
        let context = self.clone();
        let did = did.to_owned();
        self.tasks.spawn(async move {
            future::or(
                async {
                    life.ended().await;
                    Ok(())
                },
                async {
                    context
                        .run_lan(
                            did,
                            candidate,
                            stored,
                            profile,
                            life.clone(),
                            (active, failure),
                        )
                        .await
                },
            )
            .await
        });
        self.status_changed.notify(usize::MAX);
        Ok(())
    }

    async fn run_lan(
        self: Rc<Self>,
        did: String,
        mut candidate: LanRecord,
        stored: Option<LanRecord>,
        profile: LanProfile,
        life: Rc<Lifetime>,
        start: (bool, Option<RequestError>),
    ) -> Result<(), AdapterError> {
        let (had_active, initial_failure) = start;
        let mut backoff = Backoff::new(1, 30);
        let mut fallback = stored.filter(|record| record != &candidate);
        // A transiently failed preferred candidate must survive fallback probing.
        let mut retry_candidate = None;
        let terminal = |error: &RequestError| {
            matches!(error.failure, Failure::Identity | Failure::Rejected(_))
        };
        if let Some(error) = initial_failure {
            let delay = backoff.failed();
            self.lan_failed(
                &did,
                candidate.address,
                &error,
                RecoveryStatus::Scheduled(self.clock.unix_now() + delay.as_secs() as i64),
            );
            let network_changed = self.lan_network_changed.listen();
            future::or(self.clock.wait_until(self.clock.now() + delay), async {
                network_changed.await;
            })
            .await;
            if !self.lan_current(&did, &candidate.scope, &life) {
                return Ok(());
            }
            if let Some(status) = &mut self
                .live
                .borrow_mut()
                .devices
                .get_mut(&did)
                .unwrap()
                .lan
                .status
            {
                status.recovery = RecoveryStatus::Running;
            }
            self.status_changed.notify(usize::MAX);
        }
        loop {
            let result = Box::pin(probe(
                &candidate,
                self.lan_endpoint(candidate.address),
                profile.binding,
                profile.mapping,
                self.lan_timeout(),
            ))
            .await;
            match result {
                Ok(client) => {
                    if !self.lan_current(&did, &candidate.scope, &life) {
                        return Ok(());
                    }
                    self.activate_lan(
                        &did,
                        candidate,
                        client,
                        profile.protocol,
                        "direct LAN authenticated",
                        retry_candidate.is_some(),
                    )?;
                    return Ok(());
                }
                Err(mut error) => {
                    if !self.lan_current(&did, &candidate.scope, &life) {
                        return Ok(());
                    }
                    if had_active && terminal(&error) {
                        let mut live = self.live.borrow_mut();
                        let lan = &mut live.devices.get_mut(&did).unwrap().lan;
                        lan.task = None;
                        if let Some(status) = &mut lan.status {
                            status.problem = Some(format!("candidate validation failed: {error}"));
                            status.recovery = RecoveryStatus::None;
                        }
                        drop(live);
                        self.status_changed.notify(usize::MAX);
                        return Ok(());
                    }
                    if !had_active && let Some(previous) = fallback.take() {
                        if !terminal(&error) {
                            retry_candidate = Some((candidate, error));
                        }
                        candidate = previous;
                        continue;
                    }
                    if !had_active
                        && matches!(
                            error.failure,
                            Failure::Identity | Failure::Io | Failure::Timeout
                        )
                        && let Some(address) =
                            self.lan_discovery.address(did.parse().unwrap()).await
                        && address != candidate.address
                    {
                        let mut moved = candidate.clone();
                        moved.address = address;
                        match Box::pin(probe(
                            &moved,
                            self.lan_endpoint(address),
                            profile.binding,
                            profile.mapping,
                            self.lan_timeout(),
                        ))
                        .await
                        {
                            Ok(client) => {
                                if !self.lan_current(&did, &candidate.scope, &life) {
                                    return Ok(());
                                }
                                self.activate_lan(
                                    &did,
                                    moved,
                                    client,
                                    profile.protocol,
                                    "direct LAN authenticated after discovery",
                                    retry_candidate.is_some(),
                                )?;
                                return Ok(());
                            }
                            Err(discovered_error) => {
                                candidate = moved;
                                error = discovered_error;
                            }
                        }
                    }
                    if !self.lan_current(&did, &candidate.scope, &life) {
                        return Ok(());
                    }
                    if let Some((next, previous_error)) = retry_candidate.take() {
                        if !terminal(&error) {
                            fallback = Some(candidate);
                        }
                        candidate = next;
                        error = previous_error;
                    }
                    if terminal(&error) {
                        self.lan_failed(&did, candidate.address, &error, RecoveryStatus::None);
                        self.live
                            .borrow_mut()
                            .devices
                            .get_mut(&did)
                            .unwrap()
                            .lan
                            .task = None;
                        return Ok(());
                    }
                    let delay = backoff.failed();
                    let deadline = self.clock.now() + delay;
                    let recovery =
                        RecoveryStatus::Scheduled(self.clock.unix_now() + delay.as_secs() as i64);
                    if had_active {
                        if let Some(status) = &mut self
                            .live
                            .borrow_mut()
                            .devices
                            .get_mut(&did)
                            .unwrap()
                            .lan
                            .status
                        {
                            status.problem = Some(format!("candidate validation failed: {error}"));
                            status.recovery = recovery;
                        }
                        self.status_changed.notify(usize::MAX);
                    } else {
                        self.lan_failed(&did, candidate.address, &error, recovery);
                    }
                    let network_changed = self.lan_network_changed.listen();
                    future::or(self.clock.wait_until(deadline), async {
                        network_changed.await;
                    })
                    .await;
                    if !self.lan_current(&did, &candidate.scope, &life) {
                        return Ok(());
                    }
                    if let Some(status) = &mut self
                        .live
                        .borrow_mut()
                        .devices
                        .get_mut(&did)
                        .unwrap()
                        .lan
                        .status
                    {
                        status.recovery = RecoveryStatus::Running;
                    }
                    self.status_changed.notify(usize::MAX);
                }
            }
        }
    }

    fn activate_lan(
        &self,
        did: &str,
        record: LanRecord,
        client: Client,
        protocol: LanProtocol,
        reason: &str,
        retry_desired: bool,
    ) -> Result<(), AdapterError> {
        self.store.lan().save(&record)?;
        let status = LanStatus {
            protocol,
            state: LanState::Ready,
            address: Some(record.address),
            problem: None,
            recovery: RecoveryStatus::None,
        };
        let mut live = self.live.borrow_mut();
        let lan = &mut live.devices.get_mut(did).unwrap().lan;
        if retry_desired {
            // Reconciliation will retry the preferred candidate while this client stays active.
            lan.desired = Some(record.clone());
        }
        lan.active = Some((record, client));
        lan.task = None;
        lan.status = Some(status);
        drop(live);
        self.update_availability(did);
        self.refresh_source(did);
        self.update_routing(reason);
        self.refresh_changed.notify(usize::MAX);
        self.status_changed.notify(usize::MAX);
        self.changed.notify(usize::MAX);
        Ok(())
    }

    fn lan_current(&self, did: &str, scope: &Scope, life: &Rc<Lifetime>) -> bool {
        let live = self.live.borrow();
        live.scope.as_ref() == Some(scope)
            && live.devices.get(did).is_some_and(|slot| {
                slot.admitted
                    .as_ref()
                    .is_some_and(|work| !work.life.is_closed())
                    && slot
                        .lan
                        .task
                        .as_ref()
                        .is_some_and(|task| Rc::ptr_eq(task, life))
            })
    }

    pub(super) fn lan_business_failure(
        self: &Rc<Self>,
        did: &str,
        client: &Client,
        error: &RequestError,
    ) -> Result<(), AdapterError> {
        if !matches!(
            error.failure,
            Failure::Io | Failure::Timeout | Failure::InvalidResponse | Failure::Identity
        ) {
            return Ok(());
        }
        {
            let mut live = self.live.borrow_mut();
            let Some(slot) = live.devices.get_mut(did) else {
                return Ok(());
            };
            if slot
                .admitted
                .as_ref()
                .is_none_or(|work| work.life.is_closed())
                || !slot
                    .lan
                    .active
                    .as_ref()
                    .is_some_and(|(_, active)| active.same(client))
            {
                return Ok(());
            }
            if let Some(task) = slot.lan.task.take() {
                task.close();
            }
            slot.lan.active = None;
            slot.lan.desired = None;
        }
        self.reconcile_lan_after(did, Some(error.clone()))?;
        self.update_availability(did);
        self.update_routing("direct LAN business request failed");
        self.refresh_changed.notify(usize::MAX);
        self.changed.notify(usize::MAX);
        Ok(())
    }

    fn lan_failed(
        &self,
        did: &str,
        address: Ipv4Addr,
        error: &RequestError,
        recovery: RecoveryStatus,
    ) {
        if let Some(status) = &mut self
            .live
            .borrow_mut()
            .devices
            .get_mut(did)
            .unwrap()
            .lan
            .status
        {
            status.state = LanState::Failed;
            status.address = Some(address);
            status.problem = Some(format!(
                "{} ({})",
                error,
                match error.stage {
                    SendStage::BeforeSend => "before business send",
                    SendStage::Sent => "business sent",
                }
            ));
            status.recovery = recovery;
        }
        self.status_changed.notify(usize::MAX);
    }
}
