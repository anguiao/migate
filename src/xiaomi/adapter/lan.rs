use super::{
    model::{AdapterError, LanProtocol, LanState, LanStatus, RecoveryStatus},
    state::{Backoff, Context, Lifetime},
};
use crate::{
    configuration::Scope,
    storage::LanRecord,
    xiaomi::{
        capability::PropertyBinding,
        cloud::{LanCandidate, LanCredentials},
        lan::{Client, Failure, RequestError, SendStage},
        property::{self, PropertyReading, PropertyResults},
    },
};
use futures_lite::future;
use serde_json::{Value, json};
use std::{
    net::{SocketAddr, SocketAddrV4},
    rc::Rc,
    time::{Duration, Instant},
};

pub(super) const MODEL: &str = "dmaker.fan.p5c";

pub(super) fn protocol(model: &str) -> Option<LanProtocol> {
    if model == MODEL {
        Some(LanProtocol::Miot)
    } else if super::legacy_miio::mapping(model).is_some() {
        Some(LanProtocol::LegacyMcn02)
    } else {
        None
    }
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
    pub(super) fn legacy_mapping(&self, did: &str) -> Option<&'static super::legacy_miio::Mapping> {
        let live = self.live.borrow();
        let model = &live.devices.get(did)?.committed.as_ref()?.raw.model;
        super::legacy_miio::mapping(model)
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
            if self.lan_scope_invalid.get()
                || slot.group
                || work.life.is_closed()
                || protocol(&source.raw.model).is_none()
                || slot
                    .cloud_access
                    .as_ref()
                    .is_some_and(|info| info.model != source.raw.model)
            {
                None
            } else {
                let model = &source.raw.model;
                let mapping = super::legacy_miio::mapping(model);
                let binding = work
                    .native
                    .device()
                    .functions()
                    .iter()
                    .find_map(|function| {
                        if let Some(mapping) = mapping {
                            let ac = function.air_conditioner()?;
                            let bindings = ac.bindings();
                            (bindings.len() == mapping.fields().len()
                                && bindings.iter().all(|binding| mapping.contains(*binding)))
                            .then(|| ac.power())
                        } else {
                            function.fan().map(|fan| fan.power())
                        }
                    });
                let cloud = slot
                    .cloud_access
                    .as_ref()
                    .filter(|info| info.model == *model)
                    .map(|info| info.lan.clone());
                binding.map(|binding| (scope, binding, cloud, model.clone()))
            }
        };
        let Some((scope, binding, cloud, model)) = eligible else {
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
                protocol: protocol(&model).unwrap(),
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
                protocol: protocol(&model).unwrap(),
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
                    protocol: protocol(&model).unwrap(),
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
                            binding,
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
        binding: PropertyBinding,
        life: Rc<Lifetime>,
        start: (bool, Option<RequestError>),
    ) -> Result<(), AdapterError> {
        let (had_active, initial_failure) = start;
        let mut backoff = Backoff::new(1, 30);
        let mut fallback = stored.filter(|record| record != &candidate);
        if let Some(error) = initial_failure {
            let delay = backoff.failed();
            self.lan_failed(
                &did,
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
                binding,
                self.lan_timeout(),
            ))
            .await;
            match result {
                Ok(client) => {
                    if !self.lan_current(&did, &candidate.scope, &life) {
                        return Ok(());
                    }
                    self.store.lan().save(&candidate)?;
                    let mut live = self.live.borrow_mut();
                    let lan = &mut live.devices.get_mut(&did).unwrap().lan;
                    lan.active = Some((candidate.clone(), client));
                    lan.task = None;
                    lan.status = Some(LanStatus {
                        protocol: protocol(&candidate.model).unwrap(),
                        state: LanState::Ready,
                        address: Some(candidate.address),
                        problem: None,
                        recovery: RecoveryStatus::None,
                    });
                    drop(live);
                    self.update_availability(&did);
                    self.refresh_source(&did);
                    self.update_routing("direct LAN authenticated");
                    self.refresh_changed.notify(usize::MAX);
                    self.status_changed.notify(usize::MAX);
                    self.changed.notify(usize::MAX);
                    return Ok(());
                }
                Err(error) => {
                    if !self.lan_current(&did, &candidate.scope, &life) {
                        return Ok(());
                    }
                    if had_active {
                        let mut live = self.live.borrow_mut();
                        let lan = &mut live.devices.get_mut(&did).unwrap().lan;
                        lan.task = None;
                        if let Some(status) = &mut lan.status {
                            status.problem = Some(format!("candidate validation failed: {error}"));
                        }
                        drop(live);
                        self.status_changed.notify(usize::MAX);
                        return Ok(());
                    }
                    if let Some(previous) = fallback.take() {
                        candidate = previous;
                        continue;
                    }
                    if matches!(
                        error.failure,
                        Failure::Identity | Failure::Io | Failure::Timeout
                    ) && let Some(address) =
                        self.lan_discovery.address(did.parse().unwrap()).await
                        && address != candidate.address
                    {
                        let mut moved = candidate.clone();
                        moved.address = address;
                        if let Ok(client) = Box::pin(probe(
                            &moved,
                            self.lan_endpoint(address),
                            binding,
                            self.lan_timeout(),
                        ))
                        .await
                        {
                            if !self.lan_current(&did, &candidate.scope, &life) {
                                return Ok(());
                            }
                            self.store.lan().save(&moved)?;
                            let mut live = self.live.borrow_mut();
                            let lan = &mut live.devices.get_mut(&did).unwrap().lan;
                            lan.active = Some((moved, client));
                            lan.task = None;
                            lan.status = Some(LanStatus {
                                protocol: protocol(&candidate.model).unwrap(),
                                state: LanState::Ready,
                                address: Some(address),
                                problem: None,
                                recovery: RecoveryStatus::None,
                            });
                            drop(live);
                            self.update_availability(&did);
                            self.refresh_source(&did);
                            self.update_routing("direct LAN authenticated after discovery");
                            self.refresh_changed.notify(usize::MAX);
                            self.status_changed.notify(usize::MAX);
                            self.changed.notify(usize::MAX);
                            return Ok(());
                        }
                    }
                    if !self.lan_current(&did, &candidate.scope, &life) {
                        return Ok(());
                    }
                    if matches!(
                        error.failure,
                        Failure::Identity | Failure::Rejected(_) | Failure::InvalidResponse
                    ) {
                        self.lan_failed(&did, &error, RecoveryStatus::None);
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
                    self.lan_failed(
                        &did,
                        &error,
                        RecoveryStatus::Scheduled(self.clock.unix_now() + delay.as_secs() as i64),
                    );
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

    fn lan_failed(&self, did: &str, error: &RequestError, recovery: RecoveryStatus) {
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

fn invalid_response() -> RequestError {
    RequestError {
        stage: SendStage::Sent,
        failure: Failure::InvalidResponse,
    }
}

fn property_code(item: &Value) -> Result<(), RequestError> {
    match item.get("code").and_then(Value::as_i64) {
        Some(0) => Ok(()),
        Some(code) => Err(RequestError {
            stage: SendStage::Sent,
            failure: Failure::Rejected(code),
        }),
        None => Err(invalid_response()),
    }
}

pub(super) async fn set_properties(
    client: &Client,
    did: &str,
    properties: &[(PropertyBinding, Value)],
    guard: impl Fn() -> bool,
    on_sent: impl FnOnce(),
) -> Result<PropertyResults<(), RequestError>, RequestError> {
    let params = properties.iter().map(|(binding, value)| {
        json!({"did":did, "siid":binding.siid, "piid":binding.piid, "value":value})
    }).collect::<Vec<_>>();
    let result =
        Box::pin(client.request_guarded("set_properties", json!(params), guard, on_sent)).await?;
    let items = result.as_array().ok_or_else(invalid_response)?;
    let bindings = properties
        .iter()
        .map(|(binding, _)| *binding)
        .collect::<Vec<_>>();
    Ok(property::correlate(
        did,
        &bindings,
        items,
        invalid_response(),
        property_code,
    ))
}

pub(super) async fn read_property(
    client: &Client,
    did: &str,
    binding: PropertyBinding,
    guard: impl Fn() -> bool,
) -> Result<PropertyReading, RequestError> {
    let result = client
        .request_guarded(
            "get_properties",
            json!([{"did":did, "siid":binding.siid, "piid":binding.piid}]),
            guard,
            || {},
        )
        .await?;
    let items = result.as_array().ok_or_else(invalid_response)?;
    let results = property::correlate(did, &[binding], items, invalid_response(), |item| {
        property_code(item)?;
        let value = item
            .get("value")
            .filter(|value| property::valid_observation(value))
            .ok_or_else(invalid_response)?;
        Ok(PropertyReading {
            value: value.clone(),
            update_time: property::update_time(item),
        })
    });
    if results.protocol_error {
        return Err(invalid_response());
    }
    results.items.into_iter().next().unwrap()
}

async fn probe(
    record: &LanRecord,
    address: SocketAddr,
    binding: PropertyBinding,
    timeout: Duration,
) -> Result<Client, RequestError> {
    let client = Client::new(
        record.did.parse().expect("LAN DID validated"),
        address,
        record.token.clone(),
    );
    #[cfg(test)]
    let client = {
        let mut client = client;
        client.set_timeout(timeout);
        client
    };
    let invalid = |failure| RequestError {
        stage: SendStage::Sent,
        failure,
    };
    let started = Instant::now();
    {
        let info = Box::pin(client.request("miIO.info", json!([]))).await?;
        if info.get("model").and_then(Value::as_str) != Some(&record.model) {
            return Err(invalid(Failure::Identity));
        }
        let remaining = timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Err(RequestError {
                stage: SendStage::BeforeSend,
                failure: Failure::Timeout,
            });
        }
        if let Some(mapping) = super::legacy_miio::mapping(&record.model) {
            let result = Box::pin(client.request_with_timeout(
                "get_prop",
                json!(mapping.fields()),
                remaining,
            ))
            .await?;
            let values = mapping
                .decode(&result)
                .map_err(|_| invalid(Failure::InvalidResponse))?;
            if values.iter().any(Result::is_err) {
                return Err(invalid(Failure::InvalidResponse));
            }
            return Ok(client.clone());
        }
        let result = Box::pin(client.request_with_timeout(
            "get_properties",
            json!([{"did":record.did,"siid":binding.siid,"piid":binding.piid}]),
            remaining,
        ))
        .await?;
        let item = result
            .as_array()
            .and_then(|items| (items.len() == 1).then(|| &items[0]))
            .ok_or_else(|| invalid(Failure::InvalidResponse))?;
        if item.get("did").and_then(Value::as_str) != Some(&record.did)
            || item.get("siid").and_then(Value::as_u64) != Some(u64::from(binding.siid))
            || item.get("piid").and_then(Value::as_u64) != Some(u64::from(binding.piid))
        {
            return Err(invalid(Failure::Identity));
        }
        let code = item
            .get("code")
            .and_then(Value::as_i64)
            .ok_or_else(|| invalid(Failure::InvalidResponse))?;
        if code != 0 {
            return Err(invalid(Failure::Rejected(code)));
        }
        if item.get("value").and_then(Value::as_bool).is_none() {
            return Err(invalid(Failure::InvalidResponse));
        }
        Ok(client.clone())
    }
}
