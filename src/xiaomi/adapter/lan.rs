use super::{
    model::{AdapterError, LanProtocol, LanState, LanStatus, RecoveryStatus},
    state::{Backoff, Context, Lifetime},
    work::DeviceWork,
};
use crate::{
    configuration::Scope,
    storage::LanRecord,
    xiaomi::{
        capability::PropertyBinding,
        lan::{Client, Failure, RequestError, SendStage},
        legacy_miio::Mapping,
        mijia::{LanCandidate, LanCredentials},
    },
};
use futures_lite::future;
use std::{net::Ipv4Addr, rc::Rc};

mod identity;
mod properties;

use identity::probe;
pub(super) use properties::{read_property, set_properties};

#[derive(Clone, Copy)]
struct LanProfile {
    binding: PropertyBinding,
    protocol: LanProtocol,
    mapping: Option<&'static Mapping>,
}

#[derive(Default)]
pub(super) struct DirectLan {
    pub(super) status: Option<LanStatus>,
    pub(super) active: Option<(LanRecord, Client)>,
    desired: Option<LanRecord>,
    task: Option<Rc<Lifetime>>,
}

impl DirectLan {
    pub(super) fn stop(self) -> bool {
        let was_active = self.active.is_some();
        if let Some(task) = self.task {
            task.close();
        }
        was_active
    }
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
            .directory
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
    pub(super) fn legacy_mapping(&self, did: &str) -> Option<&'static Mapping> {
        let live = self.directory.borrow();
        live.devices
            .get(did)?
            .work
            .as_ref()?
            .native
            .legacy_profile()
            .map(|profile| profile.mapping())
    }

    pub(super) fn invalidate_lan_scope(&self) -> Result<(), AdapterError> {
        self.lan_scope_invalid.set(true);
        let (scope, dids) = {
            let live = self.directory.borrow();
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
        let work = self
            .directory
            .borrow()
            .devices
            .get(did)
            .and_then(|slot| slot.work.clone());
        let was_active = work.is_some_and(|work| work.lan.take().stop());
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
        let work = self
            .directory
            .borrow()
            .devices
            .get(did)
            .and_then(|slot| slot.work.clone());
        let Some(work) = work else {
            return Ok(());
        };
        let eligible = {
            let live = self.directory.borrow();
            let slot = &live.devices[did];
            let Some(scope) = live.scope.clone() else {
                return Ok(());
            };
            let native = &work.native;
            let configuration = self.configuration.borrow();
            let committed = configuration
                .device(&work.key)
                .and_then(|device| device.native.as_ref())
                .expect("an installed source has committed metadata");
            if self.lan_scope_invalid.get()
                || configuration.scope != live.scope
                || live.group(did).is_some()
                || work.life.is_closed()
                || native.direct_protocol().is_none()
                || slot
                    .cloud_access
                    .as_ref()
                    .is_some_and(|info| info.model != committed.model())
            {
                None
            } else {
                let model = committed.model();
                let legacy = native.legacy_profile();
                let binding = native.direct_probe_binding();
                let cloud = slot
                    .cloud_access
                    .as_ref()
                    .filter(|info| info.model == model)
                    .map(|info| info.lan.clone());
                binding.map(|binding| {
                    (
                        scope,
                        LanProfile {
                            binding,
                            protocol: native.direct_protocol().unwrap(),
                            mapping: legacy.map(|profile| profile.mapping()),
                        },
                        cloud,
                        model.to_owned(),
                    )
                })
            }
        };
        let Some((scope, profile, cloud, model)) = eligible else {
            if self.stop_lan(did) {
                self.update_availability(did);
                self.update_device_routing(did, "direct LAN eligibility ended");
                self.changed.notify(usize::MAX);
            }
            return Ok(());
        };
        if matches!(cloud, Some(LanCredentials::NotIp)) {
            if work
                .lan
                .borrow()
                .status
                .as_ref()
                .is_some_and(|status| status.state == LanState::Ineligible)
            {
                return Ok(());
            }
            self.stop_lan(did);
            self.store.lan().remove(did)?;
            work.lan.borrow_mut().status = Some(LanStatus {
                protocol: profile.protocol,
                state: LanState::Ineligible,
                address: None,
                problem: None,
                recovery: RecoveryStatus::None,
            });
            self.status_changed.notify(usize::MAX);
            self.update_availability(did);
            self.update_device_routing(did, "device is not eligible for direct LAN");
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
            let previous = work.lan.replace(DirectLan {
                status: Some(LanStatus {
                    protocol: profile.protocol,
                    state,
                    address: None,
                    problem: None,
                    recovery: RecoveryStatus::None,
                }),
                ..DirectLan::default()
            });
            let was_active = previous.stop();
            if was_active {
                self.update_availability(did);
                self.update_device_routing(did, "direct LAN credentials unavailable");
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
        let (life, active, old_task) = {
            let mut lan = work.lan.borrow_mut();
            if lan.desired.as_ref() == Some(&candidate) {
                return Ok(());
            }
            let life = Rc::new(Lifetime::default());
            let old_task = lan.task.replace(life.clone());
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
            (life, active, old_task)
        };
        if let Some(task) = old_task {
            task.close();
        }
        let context = self.clone();
        self.tasks.spawn(async move {
            future::or(
                async {
                    life.ended().await;
                    Ok(())
                },
                async {
                    context
                        .run_lan(
                            work,
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
        work: Rc<DeviceWork>,
        mut candidate: LanRecord,
        stored: Option<LanRecord>,
        profile: LanProfile,
        life: Rc<Lifetime>,
        start: (bool, Option<RequestError>),
    ) -> Result<(), AdapterError> {
        let did = &work.key.id;
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
                &work,
                candidate.address,
                &error,
                RecoveryStatus::Scheduled(self.clock.unix_now() + delay.as_secs() as i64),
            );
            let network_changed = self.lan_network_changed.listen();
            future::or(self.clock.wait_until(self.clock.now() + delay), async {
                network_changed.await;
            })
            .await;
            if !self.lan_current(&work, &candidate.scope, &life) {
                return Ok(());
            }
            if let Some(status) = &mut work.lan.borrow_mut().status {
                status.recovery = RecoveryStatus::Running;
            }
            self.status_changed.notify(usize::MAX);
        }
        loop {
            let result = Box::pin(probe(
                &candidate,
                self.network.lan_target(candidate.address),
                profile.binding,
                profile.mapping,
            ))
            .await;
            match result {
                Ok(client) => {
                    if !self.lan_current(&work, &candidate.scope, &life) {
                        return Ok(());
                    }
                    self.activate_lan(
                        &work,
                        candidate,
                        client,
                        profile.protocol,
                        "direct LAN authenticated",
                        retry_candidate.is_some(),
                    )?;
                    return Ok(());
                }
                Err(mut error) => {
                    if !self.lan_current(&work, &candidate.scope, &life) {
                        return Ok(());
                    }
                    if had_active && terminal(&error) {
                        let mut lan = work.lan.borrow_mut();
                        lan.task = None;
                        if let Some(status) = &mut lan.status {
                            status.problem = Some(format!("candidate validation failed: {error}"));
                            status.recovery = RecoveryStatus::None;
                        }
                        drop(lan);
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
                        && let Some(address) = self
                            .network
                            .discover_lan(&self.lan_discovery, did.parse().unwrap())
                            .await
                        && address != candidate.address
                    {
                        let mut moved = candidate.clone();
                        moved.address = address;
                        match Box::pin(probe(
                            &moved,
                            self.network.lan_target(address),
                            profile.binding,
                            profile.mapping,
                        ))
                        .await
                        {
                            Ok(client) => {
                                if !self.lan_current(&work, &candidate.scope, &life) {
                                    return Ok(());
                                }
                                self.activate_lan(
                                    &work,
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
                    if !self.lan_current(&work, &candidate.scope, &life) {
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
                        self.lan_failed(&work, candidate.address, &error, RecoveryStatus::None);
                        work.lan.borrow_mut().task = None;
                        return Ok(());
                    }
                    let delay = backoff.failed();
                    let deadline = self.clock.now() + delay;
                    let recovery =
                        RecoveryStatus::Scheduled(self.clock.unix_now() + delay.as_secs() as i64);
                    if had_active {
                        if let Some(status) = &mut work.lan.borrow_mut().status {
                            status.problem = Some(format!("candidate validation failed: {error}"));
                            status.recovery = recovery;
                        }
                        self.status_changed.notify(usize::MAX);
                    } else {
                        self.lan_failed(&work, candidate.address, &error, recovery);
                    }
                    let network_changed = self.lan_network_changed.listen();
                    future::or(self.clock.wait_until(deadline), async {
                        network_changed.await;
                    })
                    .await;
                    if !self.lan_current(&work, &candidate.scope, &life) {
                        return Ok(());
                    }
                    if let Some(status) = &mut work.lan.borrow_mut().status {
                        status.recovery = RecoveryStatus::Running;
                    }
                    self.status_changed.notify(usize::MAX);
                }
            }
        }
    }

    fn activate_lan(
        &self,
        work: &DeviceWork,
        record: LanRecord,
        client: Client,
        protocol: LanProtocol,
        reason: &str,
        retry_desired: bool,
    ) -> Result<(), AdapterError> {
        let did = &work.key.id;
        self.store.lan().save(&record)?;
        let status = LanStatus {
            protocol,
            state: LanState::Ready,
            address: Some(record.address),
            problem: None,
            recovery: RecoveryStatus::None,
        };
        let mut lan = work.lan.borrow_mut();
        if retry_desired {
            // Reconciliation will retry the preferred candidate while this client stays active.
            lan.desired = Some(record.clone());
        }
        lan.active = Some((record, client));
        lan.task = None;
        lan.status = Some(status);
        drop(lan);
        self.update_availability(did);
        self.refresh_source(did);
        self.update_device_routing(did, reason);
        self.refresh_changed.notify(usize::MAX);
        self.status_changed.notify(usize::MAX);
        self.changed.notify(usize::MAX);
        Ok(())
    }

    fn lan_current(&self, work: &Rc<DeviceWork>, scope: &Scope, life: &Rc<Lifetime>) -> bool {
        let directory = self.directory.borrow();
        directory.scope.as_ref() == Some(scope)
            && !work.life.is_closed()
            && directory
                .devices
                .get(&work.key.id)
                .and_then(|slot| slot.work.as_ref())
                .is_some_and(|current| Rc::ptr_eq(current, work))
            && work
                .lan
                .borrow()
                .task
                .as_ref()
                .is_some_and(|task| Rc::ptr_eq(task, life))
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
        let work = self
            .directory
            .borrow()
            .devices
            .get(did)
            .and_then(|slot| slot.work.clone());
        let Some(work) = work else {
            return Ok(());
        };
        let previous = {
            let mut lan = work.lan.borrow_mut();
            if work.life.is_closed()
                || !lan
                    .active
                    .as_ref()
                    .is_some_and(|(_, active)| active.same(client))
            {
                return Ok(());
            }
            std::mem::take(&mut *lan)
        };
        previous.stop();
        self.reconcile_lan_after(did, Some(error.clone()))?;
        self.update_availability(did);
        self.update_device_routing(did, "direct LAN business request failed");
        self.refresh_changed.notify(usize::MAX);
        self.changed.notify(usize::MAX);
        Ok(())
    }

    fn lan_failed(
        &self,
        work: &DeviceWork,
        address: Ipv4Addr,
        error: &RequestError,
        recovery: RecoveryStatus,
    ) {
        if let Some(status) = &mut work.lan.borrow_mut().status {
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
