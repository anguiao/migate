use std::{
    cell::RefCell,
    collections::{BTreeMap, VecDeque},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use futures_util::{FutureExt, future::LocalBoxFuture};

use crate::{
    device::{DeviceCommand, PhysicalDeviceId, Property, PropertyValue},
    xiaomi::{
        catalog::{FeatureDescriptor, WireOperation, WireValue},
        cloud::{
            CloudAction, CloudClient, CloudErrorKind, CloudProperty, PropertyReadOutcome,
            PropertyWrite, PropertyWriteOutcome,
        },
        gateway::{GatewayErrorKind, GatewayHandle},
        lan::{
            LanErrorKind, LanHandle, LanProperty, LanPropertyWrite, LanReadOutcome, LanSendGuard,
            LanWriteOutcome,
        },
        mqtt::MqttSendGuard,
    },
};

use super::{
    CommandTransport, ControlPath, ReadTarget, SendAuthorization, SendGuard, StateReadFailure,
    StateReadGuard, StateReadRequest, StateReadResult, StateReadTransport, TransportCommand,
    TransportFailure,
};

#[derive(Clone)]
pub struct SessionAuthority {
    active: Arc<AtomicBool>,
    expires_at: i64,
}

impl SessionAuthority {
    pub(crate) fn fresh_lease(&self) -> Self {
        Self {
            active: Arc::new(AtomicBool::new(true)),
            expires_at: self.expires_at,
        }
    }

    pub(crate) fn same_lease(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.active, &other.active)
    }

    pub fn new() -> Self {
        Self::new_until(i64::MAX)
    }

    pub(crate) fn new_until(expires_at: i64) -> Self {
        Self {
            active: Arc::new(AtomicBool::new(true)),
            expires_at,
        }
    }

    pub fn revoke(&self) {
        self.active.store(false, Ordering::Release);
    }

    pub fn mqtt_guard(&self) -> MqttSendGuard {
        let authority = self.clone();
        MqttSendGuard::with_check(move || Ok(authority.check()))
    }

    pub fn lan_guard(&self) -> LanSendGuard {
        let authority = self.clone();
        LanSendGuard::with_check(move || Ok(authority.check()))
    }

    pub fn check(&self) -> bool {
        self.active.load(Ordering::Acquire) && unix_time() < self.expires_at
    }
}

impl Default for SessionAuthority {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone)]
struct GatewayRoute {
    handle: GatewayHandle,
    authority: RouteAuthority,
}

#[derive(Clone)]
struct LanRoute {
    handle: LanHandle,
    authority: RouteAuthority,
    legacy_descriptor: Option<FeatureDescriptor>,
}

#[derive(Clone)]
struct CloudRoute {
    client: Rc<CloudClient>,
    access_token: String,
    authority: RouteAuthority,
    credential_active: Arc<AtomicBool>,
    credential_expires_at: i64,
}

#[derive(Clone)]
struct CloudCredential {
    account: crate::device::AccountId,
    access_token: String,
    active: Arc<AtomicBool>,
}

struct LanAttemptHealth {
    registry: CurrentSessionRegistry,
    device: PhysicalDeviceId,
    guard: SendGuard,
    lease: RouteLease,
    completed: bool,
}

struct LanReadHealth {
    registry: CurrentSessionRegistry,
    device: PhysicalDeviceId,
    guard: StateReadGuard,
    sent: Arc<AtomicBool>,
    lease: RouteLease,
    completed: bool,
}

impl Drop for LanReadHealth {
    fn drop(&mut self) {
        if !self.completed && self.guard.expired() && self.sent.load(Ordering::Acquire) {
            self.registry.report_route_failure(RouteFailure::Lan {
                device: self.device.clone(),
                lease: self.lease.clone(),
            });
        }
    }
}

impl Drop for LanAttemptHealth {
    fn drop(&mut self) {
        if !self.completed && self.guard.shared_state().expired() && self.guard.may_have_been_sent()
        {
            self.registry.report_route_failure(RouteFailure::Lan {
                device: self.device.clone(),
                lease: self.lease.clone(),
            });
        }
    }
}

#[derive(Clone)]
struct RouteAuthority {
    session: SessionAuthority,
    active: Arc<AtomicBool>,
}

#[derive(Clone)]
pub(crate) struct RouteLease {
    active: Arc<AtomicBool>,
}

impl RouteAuthority {
    fn new(session: SessionAuthority) -> Self {
        Self {
            session,
            active: Arc::new(AtomicBool::new(true)),
        }
    }
    fn check(&self) -> bool {
        if !self.active.load(Ordering::Acquire) {
            return false;
        }
        let allowed = self.session.check();
        allowed && self.active.load(Ordering::Acquire)
    }
    fn revoke(&self) {
        self.active.store(false, Ordering::Release);
    }
    fn lease(&self) -> RouteLease {
        RouteLease {
            active: self.active.clone(),
        }
    }
}

#[derive(Default)]
struct Routes {
    gateway: Option<GatewayRoute>,
    lan: Option<LanRoute>,
    cloud: Option<CloudRoute>,
}

type AuthRefresh = Rc<dyn Fn()>;
type RouteWake = Rc<dyn Fn()>;

#[derive(Clone, Default)]
pub struct CurrentSessionRegistry {
    routes: Rc<RefCell<BTreeMap<PhysicalDeviceId, Routes>>>,
    cloud_credential: Rc<RefCell<Option<CloudCredential>>>,
    auth_refresh: Rc<RefCell<Option<AuthRefresh>>>,
    route_failures: Rc<RefCell<VecDeque<RouteFailure>>>,
    route_wake: Rc<RefCell<Option<RouteWake>>>,
}

#[derive(Clone)]
pub(crate) enum RouteFailure {
    Lan {
        device: PhysicalDeviceId,
        lease: RouteLease,
    },
    CloudUnauthorized {
        device: PhysicalDeviceId,
        lease: RouteLease,
    },
}

impl RouteFailure {
    fn revoke(&self) {
        match self {
            Self::Lan { lease, .. } | Self::CloudUnauthorized { lease, .. } => {
                lease.active.store(false, Ordering::Release);
            }
        }
    }

    fn same_lease(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::Lan {
                    device: left,
                    lease: left_authority,
                },
                Self::Lan {
                    device: right,
                    lease: right_authority,
                },
            )
            | (
                Self::CloudUnauthorized {
                    device: left,
                    lease: left_authority,
                },
                Self::CloudUnauthorized {
                    device: right,
                    lease: right_authority,
                },
            ) => left == right && Arc::ptr_eq(&left_authority.active, &right_authority.active),
            _ => false,
        }
    }
}

impl CurrentSessionRegistry {
    fn cloud_credential(
        &self,
        account: &crate::device::AccountId,
        access_token: &str,
    ) -> Arc<AtomicBool> {
        let mut current = self.cloud_credential.borrow_mut();
        if let Some(credential) = current.as_ref()
            && &credential.account == account
            && credential.access_token == access_token
        {
            return credential.active.clone();
        }
        let active = Arc::new(AtomicBool::new(true));
        *current = Some(CloudCredential {
            account: account.clone(),
            access_token: access_token.to_owned(),
            active: active.clone(),
        });
        active
    }

    pub(crate) fn set_route_wake(&self, wake: impl Fn() + 'static) {
        *self.route_wake.borrow_mut() = Some(Rc::new(wake));
    }

    pub(crate) fn drain_route_failures(&self) -> Vec<RouteFailure> {
        self.route_failures.borrow_mut().drain(..).collect()
    }

    fn report_route_failure(&self, failure: RouteFailure) {
        failure.revoke();
        let mut failures = self.route_failures.borrow_mut();
        if !failures.iter().any(|current| current.same_lease(&failure)) {
            failures.push_back(failure);
        }
        drop(failures);
        if let Some(wake) = self.route_wake.borrow().as_ref() {
            wake();
        }
    }

    pub(crate) fn revoke_lan_if_authority(
        &self,
        device: &PhysicalDeviceId,
        lease: &RouteLease,
    ) -> bool {
        let mut routes = self.routes.borrow_mut();
        let matches = routes
            .get(device)
            .and_then(|routes| routes.lan.as_ref())
            .is_some_and(|route| Arc::ptr_eq(&route.authority.active, &lease.active));
        if !matches {
            return false;
        }
        let route = routes
            .get_mut(device)
            .and_then(|routes| routes.lan.take())
            .expect("matching LAN route exists");
        route.authority.revoke();
        true
    }

    pub(crate) fn revoke_cloud_if_authority(
        &self,
        device: &PhysicalDeviceId,
        lease: &RouteLease,
    ) -> bool {
        let mut routes = self.routes.borrow_mut();
        let matches = routes
            .get(device)
            .and_then(|routes| routes.cloud.as_ref())
            .is_some_and(|route| Arc::ptr_eq(&route.authority.active, &lease.active));
        if !matches {
            return false;
        }
        let route = routes
            .get_mut(device)
            .and_then(|routes| routes.cloud.take())
            .expect("matching cloud route exists");
        route.authority.revoke();
        true
    }
    pub fn set_auth_refresh(&self, refresh: impl Fn() + 'static) {
        *self.auth_refresh.borrow_mut() = Some(Rc::new(refresh));
    }

    fn request_auth_refresh(&self) {
        if let Some(refresh) = self.auth_refresh.borrow().as_ref() {
            refresh();
        }
    }

    pub fn install_gateway(
        &self,
        device: PhysicalDeviceId,
        handle: GatewayHandle,
        authority: SessionAuthority,
    ) {
        let old = self
            .routes
            .borrow_mut()
            .entry(device)
            .or_default()
            .gateway
            .replace(GatewayRoute {
                handle,
                authority: RouteAuthority::new(authority),
            });
        if let Some(old) = old {
            old.authority.revoke();
        }
    }

    pub(crate) fn install_lan(
        &self,
        device: PhysicalDeviceId,
        handle: LanHandle,
        authority: SessionAuthority,
        legacy_descriptor: Option<FeatureDescriptor>,
    ) -> RouteLease {
        let authority = RouteAuthority::new(authority);
        let lease = authority.lease();
        let old = self
            .routes
            .borrow_mut()
            .entry(device)
            .or_default()
            .lan
            .replace(LanRoute {
                handle,
                authority,
                legacy_descriptor,
            });
        if let Some(old) = old {
            old.authority.revoke();
        }
        lease
    }

    pub fn install_cloud(
        &self,
        device: PhysicalDeviceId,
        client: Rc<CloudClient>,
        access_token: impl Into<String>,
        expires_at: i64,
        authority: SessionAuthority,
    ) {
        let access_token = access_token.into();
        let credential_active = self.cloud_credential(&device.account, &access_token);
        let old = self
            .routes
            .borrow_mut()
            .entry(device)
            .or_default()
            .cloud
            .replace(CloudRoute {
                client,
                access_token,
                authority: RouteAuthority::new(authority),
                credential_active,
                credential_expires_at: expires_at,
            });
        if let Some(old) = old {
            old.authority.revoke();
        }
    }

    pub(crate) fn install_cloud_if_changed(
        &self,
        device: PhysicalDeviceId,
        client: Rc<CloudClient>,
        access_token: String,
        expires_at: i64,
        authority: SessionAuthority,
    ) -> bool {
        let unchanged = self
            .routes
            .borrow()
            .get(&device)
            .and_then(|routes| routes.cloud.as_ref())
            .is_some_and(|route| {
                Rc::ptr_eq(&route.client, &client)
                    && route.access_token == access_token
                    && route.authority.session.same_lease(&authority)
                    && route.authority.active.load(Ordering::Acquire)
                    && route.credential_active.load(Ordering::Acquire)
                    && route.credential_expires_at == expires_at
            });
        if unchanged {
            return false;
        }
        let credential_active = self.cloud_credential(&device.account, &access_token);
        let new_authority = RouteAuthority::new(authority);
        let old = self
            .routes
            .borrow_mut()
            .entry(device)
            .or_default()
            .cloud
            .replace(CloudRoute {
                client,
                access_token,
                authority: new_authority,
                credential_active,
                credential_expires_at: expires_at,
            });
        if let Some(old) = old {
            old.authority.revoke();
        }
        true
    }

    pub fn revoke_device(&self, device: &PhysicalDeviceId) {
        if let Some(routes) = self.routes.borrow_mut().remove(device) {
            revoke_routes(routes);
        }
    }

    pub fn revoke_gateway(&self, device: &PhysicalDeviceId) {
        if let Some(route) = self
            .routes
            .borrow_mut()
            .get_mut(device)
            .and_then(|routes| routes.gateway.take())
        {
            route.authority.revoke();
        }
    }

    pub fn revoke_lan(&self, device: &PhysicalDeviceId) {
        if let Some(route) = self
            .routes
            .borrow_mut()
            .get_mut(device)
            .and_then(|routes| routes.lan.take())
        {
            route.authority.revoke();
        }
    }

    pub fn revoke_cloud(&self, device: &PhysicalDeviceId) {
        if let Some(route) = self
            .routes
            .borrow_mut()
            .get_mut(device)
            .and_then(|routes| routes.cloud.take())
        {
            route.authority.revoke();
        }
    }

    pub fn revoke_all(&self) {
        for (_, routes) in std::mem::take(&mut *self.routes.borrow_mut()) {
            revoke_routes(routes);
        }
    }
}

fn revoke_routes(routes: Routes) {
    if let Some(route) = routes.gateway {
        route.authority.revoke();
    }
    if let Some(route) = routes.lan {
        route.authority.revoke();
    }
    if let Some(route) = routes.cloud {
        route.authority.revoke();
    }
}

fn cloud_route_live(route: &CloudRoute) -> bool {
    route.authority.check()
        && route.credential_active.load(Ordering::Acquire)
        && unix_time() < route.credential_expires_at
}

fn unix_time() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is before Unix epoch")
        .as_secs() as i64
}

#[derive(Clone, Default)]
pub struct RuntimeTransports {
    registry: CurrentSessionRegistry,
}

impl RuntimeTransports {
    pub fn new(registry: CurrentSessionRegistry) -> Self {
        Self { registry }
    }

    pub fn registry(&self) -> &CurrentSessionRegistry {
        &self.registry
    }

    pub fn available_paths(&self, device: &PhysicalDeviceId) -> super::OperationPaths {
        let routes = self.registry.routes.borrow();
        let routes = routes.get(device);
        super::OperationPaths {
            gateway: routes
                .and_then(|routes| routes.gateway.as_ref())
                .is_some_and(|route| route.authority.check()),
            lan: routes
                .and_then(|routes| routes.lan.as_ref())
                .is_some_and(|route| route.authority.check()),
            cloud: routes
                .and_then(|routes| routes.cloud.as_ref())
                .is_some_and(cloud_route_live),
        }
    }
}

impl CommandTransport for RuntimeTransports {
    fn available_paths(&self, device: &PhysicalDeviceId) -> super::OperationPaths {
        RuntimeTransports::available_paths(self, device)
    }

    fn send(
        &self,
        path: ControlPath,
        command: TransportCommand,
        timeout: Duration,
        guard: SendGuard,
    ) -> LocalBoxFuture<'static, Result<(), TransportFailure>> {
        let registry = self.registry.clone();
        async move {
            if timeout.is_zero() || guard.authorization() != SendAuthorization::Allowed {
                return Err(TransportFailure::Unavailable);
            }
            let deadline = Instant::now() + timeout;
            match path {
                ControlPath::Gateway => {
                    let route = registry
                        .routes
                        .borrow()
                        .get(&command.device)
                        .and_then(|r| r.gateway.clone())
                        .ok_or(TransportFailure::Unavailable)?;
                    if !route.authority.check() {
                        return Err(TransportFailure::Unavailable);
                    }
                    let nested = delivery_mqtt_guard(&guard);
                    let result = match command.operation {
                        WireOperation::SetProperty { siid, piid, value } => {
                            route
                                .handle
                                .set_property(
                                    command.device.parent_did.as_str(),
                                    siid,
                                    piid,
                                    value,
                                    deadline,
                                    nested,
                                )
                                .await
                        }
                        WireOperation::InvokeAction { siid, aiid, input } => {
                            route
                                .handle
                                .invoke_action(
                                    command.device.parent_did.as_str(),
                                    siid,
                                    aiid,
                                    input,
                                    deadline,
                                    nested,
                                )
                                .await
                        }
                    };
                    result.map_err(map_gateway_error)
                }
                ControlPath::Lan => {
                    let route = registry
                        .routes
                        .borrow()
                        .get(&command.device)
                        .and_then(|r| r.lan.clone())
                        .ok_or(TransportFailure::Unavailable)?;
                    if !route.authority.check() {
                        return Err(TransportFailure::Unavailable);
                    }
                    let nested = delivery_lan_guard(&guard);
                    let mut health = LanAttemptHealth {
                        registry: registry.clone(),
                        device: command.device.clone(),
                        guard: guard.clone(),
                        lease: route.authority.lease(),
                        completed: false,
                    };
                    let result =
                        if route.legacy_descriptor.is_some() {
                            let result = route
                                .handle
                                .execute_mcn02(&command.typed, deadline, nested)
                                .await;
                            if result.as_ref().err().is_some_and(|error| {
                                should_report_lan_failure(error.kind(), &guard)
                            }) {
                                registry.report_route_failure(RouteFailure::Lan {
                                    device: command.device.clone(),
                                    lease: route.authority.lease(),
                                });
                            }
                            result.map_err(map_lan_error)
                        } else {
                            match command.operation {
                                WireOperation::SetProperty { siid, piid, value } => {
                                    let result = route
                                        .handle
                                        .set_properties(
                                            &[LanPropertyWrite {
                                                property: LanProperty { siid, piid },
                                                value,
                                            }],
                                            deadline,
                                            nested,
                                        )
                                        .await;
                                    if result.as_ref().err().is_some_and(|error| {
                                        should_report_lan_failure(error.kind(), &guard)
                                    }) {
                                        registry.report_route_failure(RouteFailure::Lan {
                                            device: command.device.clone(),
                                            lease: route.authority.lease(),
                                        });
                                    }
                                    match result {
                                        Ok(outcomes) => match outcomes.as_slice() {
                                            [LanWriteOutcome::Accepted] => Ok(()),
                                            [LanWriteOutcome::Error(code)] => {
                                                Err(TransportFailure::Rejected(*code))
                                            }
                                            _ => Err(TransportFailure::Unavailable),
                                        },
                                        Err(error) => Err(map_lan_error(error)),
                                    }
                                }
                                WireOperation::InvokeAction { siid, aiid, input } => {
                                    let result = route
                                        .handle
                                        .invoke_action(siid, aiid, &input, deadline, nested)
                                        .await;
                                    if result.as_ref().err().is_some_and(|error| {
                                        should_report_lan_failure(error.kind(), &guard)
                                    }) {
                                        registry.report_route_failure(RouteFailure::Lan {
                                            device: command.device.clone(),
                                            lease: route.authority.lease(),
                                        });
                                    }
                                    result.map_err(map_lan_error)
                                }
                            }
                        };
                    health.completed = true;
                    result
                }
                ControlPath::Cloud => {
                    let route = registry
                        .routes
                        .borrow()
                        .get(&command.device)
                        .and_then(|r| r.cloud.clone())
                        .ok_or(TransportFailure::Unavailable)?;
                    if !cloud_route_live(&route) {
                        return Err(TransportFailure::Unavailable);
                    }
                    guard.shared_state().mark_sent();
                    let did = command.device.parent_did.as_str();
                    match command.operation {
                        WireOperation::SetProperty { siid, piid, value } => {
                            let property = CloudProperty::new(did, siid, piid);
                            let values = route
                                .client
                                .set_properties(
                                    &route.access_token,
                                    &[PropertyWrite::new(property, value)],
                                    deadline,
                                )
                                .await
                                .map_err(|error| {
                                    if error.kind() == &CloudErrorKind::Unauthorized {
                                        route.credential_active.store(false, Ordering::Release);
                                        registry.request_auth_refresh();
                                        registry.report_route_failure(
                                            RouteFailure::CloudUnauthorized {
                                                device: command.device.clone(),
                                                lease: route.authority.lease(),
                                            },
                                        );
                                    }
                                    map_cloud_error(error, guard.may_have_been_sent())
                                })?;
                            match values.as_slice() {
                                [value] if value.outcome() == &PropertyWriteOutcome::Accepted => {
                                    Ok(())
                                }
                                [value] => match value.outcome() {
                                    PropertyWriteOutcome::Error(code) => {
                                        Err(TransportFailure::Rejected(*code))
                                    }
                                    PropertyWriteOutcome::Accepted => unreachable!(),
                                },
                                _ => Err(if guard.may_have_been_sent() {
                                    TransportFailure::Ambiguous
                                } else {
                                    TransportFailure::Unavailable
                                }),
                            }
                        }
                        WireOperation::InvokeAction { siid, aiid, input } => {
                            let result = route
                                .client
                                .invoke_action(
                                    &route.access_token,
                                    &CloudAction::new(did, siid, aiid, input),
                                    deadline,
                                )
                                .await;
                            if result
                                .as_ref()
                                .err()
                                .is_some_and(|error| error.kind() == &CloudErrorKind::Unauthorized)
                            {
                                route.credential_active.store(false, Ordering::Release);
                                registry.request_auth_refresh();
                                registry.report_route_failure(RouteFailure::CloudUnauthorized {
                                    device: command.device.clone(),
                                    lease: route.authority.lease(),
                                });
                            }
                            result
                                .map_err(|error| map_cloud_error(error, guard.may_have_been_sent()))
                        }
                    }
                }
            }
        }
        .boxed_local()
    }
}

impl StateReadTransport for RuntimeTransports {
    fn available_paths(&self, device: &PhysicalDeviceId) -> super::OperationPaths {
        RuntimeTransports::available_paths(self, device)
    }

    fn read(
        &self,
        request: StateReadRequest,
        timeout: Duration,
        guard: StateReadGuard,
    ) -> LocalBoxFuture<'static, Result<Vec<StateReadResult>, StateReadFailure>> {
        let registry = self.registry.clone();
        async move {
            if timeout.is_zero() || !guard.permitted() {
                return Err(StateReadFailure::Unavailable);
            }
            let deadline = Instant::now() + timeout;
            match request.path {
                ControlPath::Gateway => {
                    let route = registry
                        .routes
                        .borrow()
                        .get(&request.device)
                        .and_then(|r| r.gateway.clone())
                        .ok_or(StateReadFailure::Unavailable)?;
                    let mut results = Vec::with_capacity(request.targets.len());
                    for target in request.targets {
                        if !guard.permitted() {
                            return Err(StateReadFailure::Unavailable);
                        }
                        if !route.authority.check() {
                            return Err(StateReadFailure::Unavailable);
                        }
                        let nested = MqttSendGuard::new();
                        let value = match route
                            .handle
                            .get_property(
                                request.device.parent_did.as_str(),
                                target.siid,
                                target.piid,
                                deadline,
                                nested,
                            )
                            .await
                        {
                            Ok(value) => value,
                            Err(error) if matches!(error.kind(), GatewayErrorKind::Business(_)) => {
                                None
                            }
                            Err(error) => return Err(map_gateway_read(error)),
                        };
                        results.push(StateReadResult {
                            siid: target.siid,
                            piid: target.piid,
                            value,
                        });
                    }
                    Ok(results)
                }
                ControlPath::Lan => {
                    let route = registry
                        .routes
                        .borrow()
                        .get(&request.device)
                        .and_then(|r| r.lan.clone())
                        .ok_or(StateReadFailure::Unavailable)?;
                    let sent = Arc::new(AtomicBool::new(false));
                    let sent_observer = sent.clone();
                    if !route.authority.check() {
                        return Err(StateReadFailure::Unavailable);
                    }
                    let nested = LanSendGuard::new()
                        .with_send_observer(move || sent_observer.store(true, Ordering::Release));
                    let mut health = LanReadHealth {
                        registry: registry.clone(),
                        device: request.device.clone(),
                        guard: guard.clone(),
                        sent,
                        lease: route.authority.lease(),
                        completed: false,
                    };
                    if let Some(descriptor) = &route.legacy_descriptor {
                        let result = route.handle.read_mcn02(deadline, nested).await;
                        health.completed = true;
                        let values = result.map_err(|error| {
                            let sent = error.may_have_been_sent();
                            if lan_connection_failed(error.kind(), sent)
                                || (sent && guard.expired())
                            {
                                registry.report_route_failure(RouteFailure::Lan {
                                    device: request.device.clone(),
                                    lease: route.authority.lease(),
                                });
                            }
                            map_lan_read(error)
                        })?;
                        request
                            .targets
                            .into_iter()
                            .map(|target| {
                                let value = values.iter().find_map(|(property, value)| {
                                    value.as_ref().and_then(|value| {
                                        normalize_legacy(descriptor, target, *property, value)
                                    })
                                });
                                Ok(StateReadResult {
                                    siid: target.siid,
                                    piid: target.piid,
                                    value,
                                })
                            })
                            .collect()
                    } else {
                        let properties = request
                            .targets
                            .iter()
                            .map(|target| LanProperty {
                                siid: target.siid,
                                piid: target.piid,
                            })
                            .collect::<Vec<_>>();
                        let result = route
                            .handle
                            .read_properties(&properties, deadline, nested)
                            .await;
                        health.completed = true;
                        result
                            .map_err(|error| {
                                let sent = error.may_have_been_sent();
                                if lan_connection_failed(error.kind(), sent)
                                    || (sent && guard.expired())
                                {
                                    registry.report_route_failure(RouteFailure::Lan {
                                        device: request.device.clone(),
                                        lease: route.authority.lease(),
                                    });
                                }
                                map_lan_read(error)
                            })?
                            .into_iter()
                            .map(|value| {
                                Ok(StateReadResult {
                                    siid: value.property.siid,
                                    piid: value.property.piid,
                                    value: match value.outcome {
                                        LanReadOutcome::Value(value) => Some(value),
                                        LanReadOutcome::Unknown | LanReadOutcome::Error(_) => None,
                                    },
                                })
                            })
                            .collect()
                    }
                }
                ControlPath::Cloud => {
                    let route = registry
                        .routes
                        .borrow()
                        .get(&request.device)
                        .and_then(|r| r.cloud.clone())
                        .ok_or(StateReadFailure::Unavailable)?;
                    let properties = request
                        .targets
                        .iter()
                        .map(|target| {
                            CloudProperty::new(
                                request.device.parent_did.as_str(),
                                target.siid,
                                target.piid,
                            )
                        })
                        .collect::<Vec<_>>();
                    if !cloud_route_live(&route) {
                        return Err(StateReadFailure::Unavailable);
                    }
                    let values = route
                        .client
                        .read_properties(&route.access_token, &properties, deadline)
                        .await;
                    if values
                        .as_ref()
                        .err()
                        .is_some_and(|error| error.kind() == &CloudErrorKind::Unauthorized)
                    {
                        route.credential_active.store(false, Ordering::Release);
                        registry.request_auth_refresh();
                        registry.report_route_failure(RouteFailure::CloudUnauthorized {
                            device: request.device.clone(),
                            lease: route.authority.lease(),
                        });
                    }
                    values
                        .map_err(map_cloud_read)?
                        .into_iter()
                        .map(|value| {
                            Ok(StateReadResult {
                                siid: value.property().siid(),
                                piid: value.property().piid(),
                                value: match value.outcome() {
                                    PropertyReadOutcome::Value(value) => Some(value.clone()),
                                    PropertyReadOutcome::Unknown
                                    | PropertyReadOutcome::Error(_) => None,
                                },
                            })
                        })
                        .collect()
                }
            }
        }
        .boxed_local()
    }
}

fn delivery_mqtt_guard(guard: &SendGuard) -> MqttSendGuard {
    let shared = guard.shared_state();
    MqttSendGuard::new().with_send_observer(move || shared.mark_sent())
}

fn delivery_lan_guard(guard: &SendGuard) -> LanSendGuard {
    let shared = guard.shared_state();
    LanSendGuard::new().with_send_observer(move || shared.mark_sent())
}

fn map_gateway_error(error: crate::xiaomi::gateway::GatewayError) -> TransportFailure {
    match error.kind() {
        GatewayErrorKind::Business(code) => TransportFailure::Rejected(*code),
        _ if error.may_have_been_sent() => TransportFailure::Ambiguous,
        _ => TransportFailure::Unavailable,
    }
}
fn map_lan_error(error: crate::xiaomi::lan::LanError) -> TransportFailure {
    match error.kind() {
        LanErrorKind::Business(code) => TransportFailure::Rejected(*code),
        _ if error.may_have_been_sent() => TransportFailure::Ambiguous,
        _ => TransportFailure::Unavailable,
    }
}

fn should_report_lan_failure(kind: &LanErrorKind, guard: &SendGuard) -> bool {
    match kind {
        LanErrorKind::Transport | LanErrorKind::NotAuthenticated => true,
        LanErrorKind::Timeout => guard.may_have_been_sent(),
        LanErrorKind::Cancelled => guard.may_have_been_sent() && guard.shared_state().expired(),
        LanErrorKind::InvalidInput
        | LanErrorKind::Protocol
        | LanErrorKind::Unsupported
        | LanErrorKind::RateLimited
        | LanErrorKind::Business(_) => false,
    }
}

fn lan_connection_failed(kind: &LanErrorKind, sent: bool) -> bool {
    match kind {
        LanErrorKind::NotAuthenticated => true,
        LanErrorKind::Timeout | LanErrorKind::Transport => sent,
        _ => false,
    }
}
fn map_cloud_error(error: crate::xiaomi::cloud::CloudError, sent: bool) -> TransportFailure {
    match error.kind() {
        CloudErrorKind::Business(code) => TransportFailure::Rejected(*code),
        CloudErrorKind::Unauthorized => TransportFailure::Rejected(401),
        CloudErrorKind::HttpStatus(_) if sent => TransportFailure::Ambiguous,
        CloudErrorKind::HttpStatus(_) => TransportFailure::Unavailable,
        _ if sent => TransportFailure::Ambiguous,
        _ => TransportFailure::Unavailable,
    }
}
fn map_gateway_read(error: crate::xiaomi::gateway::GatewayError) -> StateReadFailure {
    match error.kind() {
        GatewayErrorKind::Protocol | GatewayErrorKind::InvalidInput => StateReadFailure::Malformed,
        GatewayErrorKind::Business(code) => StateReadFailure::Rejected(*code),
        GatewayErrorKind::Timeout => StateReadFailure::Timeout,
        GatewayErrorKind::Transport | GatewayErrorKind::Superseded => StateReadFailure::Unavailable,
    }
}
fn map_lan_read(error: crate::xiaomi::lan::LanError) -> StateReadFailure {
    match error.kind() {
        LanErrorKind::Protocol | LanErrorKind::InvalidInput => StateReadFailure::Malformed,
        LanErrorKind::Business(code) => StateReadFailure::Rejected(*code),
        LanErrorKind::Timeout => StateReadFailure::Timeout,
        LanErrorKind::NotAuthenticated => StateReadFailure::Unauthorized,
        LanErrorKind::Transport
        | LanErrorKind::Unsupported
        | LanErrorKind::RateLimited
        | LanErrorKind::Cancelled => StateReadFailure::Unavailable,
    }
}
fn map_cloud_read(error: crate::xiaomi::cloud::CloudError) -> StateReadFailure {
    match error.kind() {
        CloudErrorKind::Unauthorized => StateReadFailure::Unauthorized,
        CloudErrorKind::Business(code) => StateReadFailure::Rejected(*code),
        CloudErrorKind::Protocol | CloudErrorKind::InvalidInput => StateReadFailure::Malformed,
        CloudErrorKind::Timeout => StateReadFailure::Timeout,
        CloudErrorKind::HttpStatus(code) => StateReadFailure::Rejected(i64::from(*code)),
        CloudErrorKind::Network => StateReadFailure::Unavailable,
    }
}

fn normalize_legacy(
    descriptor: &FeatureDescriptor,
    target: ReadTarget,
    property: crate::device::Property,
    value: &PropertyValue,
) -> Option<WireValue> {
    let command = match (property, value) {
        (Property::Power, PropertyValue::Power(value)) => DeviceCommand::SetPower(*value),
        (Property::HvacMode, PropertyValue::HvacMode(value)) => DeviceCommand::SetHvacMode(*value),
        (Property::TargetTemperature, PropertyValue::Temperature(value)) => {
            DeviceCommand::SetTargetTemperature(*value)
        }
        (Property::FanSpeed, PropertyValue::FanSpeed(value)) => DeviceCommand::SetFanSpeed(*value),
        (Property::SwingMode, PropertyValue::SwingMode(value)) => {
            DeviceCommand::SetSwingMode(*value)
        }
        _ => return None,
    };
    let operations = descriptor.encode(&command).ok()?;
    let [WireOperation::SetProperty { siid, piid, value }] = operations.as_slice() else {
        return None;
    };
    (*siid == target.siid && *piid == target.piid).then(|| value.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        device::{AccountId, CommandOutcome, DeviceDid, DeviceService, FeatureIdentity, HomeId},
        storage::{Store, TokenSet, XiaomiRecord},
        xiaomi::{
            catalog::compile_spec,
            test_support::{MockResponse, mock_server},
        },
    };
    use std::cell::Cell;

    fn record() -> XiaomiRecord {
        XiaomiRecord {
            uid: "10001".into(),
            region: "cn".into(),
            oauth_client_uuid: "123e4567-e89b-12d3-a456-426614174000".into(),
            redirect_uri: "http://127.0.0.1/callback".into(),
            tokens: TokenSet {
                access_token: "access-token".into(),
                refresh_token: "refresh-token".into(),
                expires_at: 2_000_000_000,
                refresh_at: 1_900_000_000,
            },
            virtual_did: "123456789012345".into(),
            private_key_pem: "test-key".into(),
            certificate_pem: "test-certificate".into(),
        }
    }

    fn light() -> (FeatureIdentity, FeatureDescriptor) {
        let document = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/miot_specs/yeelink.light.ml9.json"
        ))
        .unwrap();
        let descriptor = compile_spec("yeelink.light.ml9", &document)
            .unwrap()
            .features
            .remove(0);
        let identity = FeatureIdentity {
            physical: PhysicalDeviceId {
                account: AccountId::new("10001").unwrap(),
                home: HomeId::new("home-a").unwrap(),
                parent_did: DeviceDid::new("1234").unwrap(),
            },
            service_instance: descriptor.service_instance,
            role: descriptor.role,
        };
        (identity, descriptor)
    }

    fn run_cloud_command(
        response: MockResponse,
        change_account_after_delivery: bool,
    ) -> (
        CommandOutcome,
        Vec<crate::xiaomi::test_support::ReceivedRequest>,
        bool,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        store.xiaomi().replace(&record()).unwrap();
        let snapshot = store.xiaomi().snapshot().unwrap();
        let (base, requests) = if change_account_after_delivery {
            let path = directory.path().to_owned();
            crate::xiaomi::test_support::dynamic_mock_server(1, move |_| {
                let other = Store::open(&path).unwrap();
                other.xiaomi().logout().unwrap();
                other.xiaomi().replace(&record()).unwrap();
                response.clone()
            })
        } else {
            mock_server(vec![response])
        };
        let (identity, descriptor) = light();
        let service = DeviceService::new();
        service.publish(
            identity.clone(),
            "Transport light",
            descriptor.capabilities.clone(),
        );
        service.set_state_availability(&identity, true);
        let registry = CurrentSessionRegistry::default();
        let authority = SessionAuthority::new();
        let cloud = Rc::new(CloudClient::for_test(&base, Duration::from_millis(300)).unwrap());
        registry.install_cloud_if_changed(
            identity.physical.clone(),
            cloud.clone(),
            "access-token".into(),
            i64::MAX,
            authority.clone(),
        );
        let mut sibling = identity.physical.clone();
        sibling.parent_did = DeviceDid::new("5678").unwrap();
        registry.install_cloud_if_changed(
            sibling.clone(),
            cloud,
            "access-token".into(),
            i64::MAX,
            authority,
        );
        let transports = Rc::new(RuntimeTransports::new(registry));
        let runtime = super::super::CommandRuntime::with_limits(
            service.clone(),
            transports.clone(),
            super::super::CommandLimits {
                total: Duration::from_millis(500),
                cloud_attempt: Duration::from_millis(300),
                ..super::super::CommandLimits::default()
            },
        );
        runtime.register(super::super::RuntimeFeature {
            identity: identity.clone(),
            descriptor,
            authority_generation: 1,
            auth_session_generation: snapshot.session_generation,
        });
        let ticket = service.command(&identity, DeviceCommand::SetPower(false));
        async_io::block_on(runtime.run_until_idle());
        let outcome = async_io::block_on(ticket);
        let mut received = Vec::new();
        while let Ok(request) = requests.recv_timeout(Duration::from_millis(50)) {
            received.push(request);
        }
        let sibling_cloud_available = transports.available_paths(&sibling).cloud;
        (outcome, received, sibling_cloud_available)
    }

    #[test]
    fn actual_cloud_adapter_sends_one_exact_property_and_classifies_http_ambiguity() {
        let accepted = MockResponse::json(
            200,
            r#"{"code":0,"result":[{"did":"1234","siid":2,"piid":1,"code":0}]}"#,
        );
        let (outcome, request, _) = run_cloud_command(accepted, false);
        assert_eq!(outcome, CommandOutcome::Accepted);
        assert_eq!(request.len(), 1);
        let request = &request[0];
        assert_eq!(request.target, "/app/v2/miotspec/prop/set");
        let body: serde_json::Value = serde_json::from_str(&request.body).unwrap();
        assert_eq!(
            body,
            serde_json::json!({"params":[{"did":"1234","siid":2,"piid":1,"value":false}]})
        );

        let (outcome, request, _) =
            run_cloud_command(MockResponse::json(503, "unavailable"), false);
        assert_eq!(outcome, CommandOutcome::Ambiguous);
        assert_eq!(request.len(), 1);
    }

    #[test]
    fn current_cloud_unauthorized_response_closes_every_route_sharing_the_token() {
        let (outcome, requests, sibling_cloud_available) =
            run_cloud_command(MockResponse::json(401, "unauthorized"), false);

        assert_eq!(outcome, CommandOutcome::Rejected(401));
        assert_eq!(requests.len(), 1);
        assert!(
            !sibling_cloud_available,
            "a known-invalid credential must close all routes sharing its lease immediately"
        );
    }

    #[test]
    fn dropping_a_sent_lan_read_at_its_state_deadline_revokes_the_exact_route() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        store.xiaomi().replace(&record()).unwrap();
        let (identity, _) = light();
        let authority = SessionAuthority::new();
        let registry = CurrentSessionRegistry::default();
        let (session, handle, _notifications, device, token) =
            crate::xiaomi::lan::session_pair("lumi.acpartner.mcn02");
        let mut session = session.run().boxed_local();
        let sent = Rc::new(Cell::new(false));
        let observed_sent = sent.clone();
        async_io::block_on(futures_lite::future::or(
            async {
                futures_lite::future::zip(
                    handle.authenticate(
                        LanProperty { siid: 2, piid: 1 },
                        Instant::now() + Duration::from_secs(1),
                        LanSendGuard::new(),
                    ),
                    crate::xiaomi::lan::reject_native_authentication_probe(&device, &token),
                )
                .await
                .0
                .unwrap();
            },
            async {
                let stopped = session.as_mut().await;
                panic!("LAN session stopped during authentication: {stopped:?}")
            },
        ));
        registry.install_lan(identity.physical.clone(), handle, authority, None);
        let transport = RuntimeTransports::new(registry.clone());
        let request = StateReadRequest {
            device: identity.physical.clone(),
            path: ControlPath::Lan,
            targets: vec![ReadTarget { siid: 2, piid: 1 }],
        };
        let guard = StateReadGuard::for_test(Instant::now() + Duration::from_millis(40));
        async_io::block_on(futures_lite::future::or(
            async {
                futures_lite::future::race(
                    async {
                        let _ = transport
                            .read(request, Duration::from_millis(200), guard)
                            .await;
                    },
                    async {
                        async_io::Timer::after(Duration::from_millis(70)).await;
                    },
                )
                .await;
            },
            futures_lite::future::or(
                async {
                    let stopped = session.as_mut().await;
                    panic!("LAN session stopped during pending read: {stopped:?}")
                },
                async move {
                    let (request, _) =
                        crate::xiaomi::lan::receive_request_for_test(&device, &token).await;
                    assert_eq!(request["method"], "get_properties");
                    observed_sent.set(true);
                    futures_lite::future::pending::<()>().await;
                },
            ),
        ));
        assert!(sent.get(), "the LAN read did not reach the UDP boundary");
        let failures = registry.drain_route_failures();
        assert!(
            !transport.available_paths(&identity.physical).lan,
            "sent read retained its route, failures={}",
            failures.len()
        );
        assert_eq!(failures.len(), 1);
    }

    #[test]
    fn delivered_http_command_keeps_its_actual_reply_after_account_change() {
        let response = MockResponse::json(
            200,
            r#"{"code":0,"result":[{"did":"1234","siid":2,"piid":1,"code":0}]}"#,
        );
        let (outcome, request, _) = run_cloud_command(response, true);
        assert_eq!(outcome, CommandOutcome::Accepted);
        assert_eq!(request.len(), 1);
    }

    #[test]
    fn legacy_normalization_uses_the_descriptor_command_codec() {
        let document = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/miot_specs/lumi.acpartner.mcn02.json"
        ))
        .unwrap();
        let feature = compile_spec("lumi.acpartner.mcn02", &document)
            .unwrap()
            .features
            .into_iter()
            .find(|feature| feature.role == crate::device::FeatureRole::Climate)
            .unwrap();
        for (property, value) in [
            (Property::Power, PropertyValue::Power(true)),
            (
                Property::HvacMode,
                PropertyValue::HvacMode(crate::device::HvacMode::Cool),
            ),
            (
                Property::TargetTemperature,
                PropertyValue::Temperature(24.0),
            ),
            (Property::FanSpeed, PropertyValue::FanSpeed(2)),
            (
                Property::SwingMode,
                PropertyValue::SwingMode(crate::device::SwingMode::Vertical),
            ),
        ] {
            let mapping = feature
                .properties
                .iter()
                .find(|mapping| mapping.property == property)
                .unwrap();
            assert!(
                normalize_legacy(
                    &feature,
                    ReadTarget {
                        siid: mapping.siid,
                        piid: mapping.piid,
                    },
                    property,
                    &value,
                )
                .is_some()
            );
        }
    }

    #[test]
    fn stale_cloud_unauthorized_lease_cannot_revoke_replacement_token() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        store.xiaomi().replace(&record()).unwrap();
        let (identity, _) = light();
        let authority = SessionAuthority::new();
        let registry = CurrentSessionRegistry::default();
        let client = Rc::new(
            CloudClient::for_test("http://127.0.0.1:9", Duration::from_millis(20)).unwrap(),
        );
        registry.install_cloud_if_changed(
            identity.physical.clone(),
            client.clone(),
            "old-token".into(),
            i64::MAX,
            authority.clone(),
        );
        let mut second = identity.physical.clone();
        second.parent_did = DeviceDid::new("5678").unwrap();
        registry.install_cloud_if_changed(
            second.clone(),
            client.clone(),
            "old-token".into(),
            i64::MAX,
            authority.clone(),
        );
        let old_lease = registry.routes.borrow()[&identity.physical]
            .cloud
            .as_ref()
            .unwrap()
            .authority
            .lease();
        registry.routes.borrow()[&identity.physical]
            .cloud
            .as_ref()
            .unwrap()
            .credential_active
            .store(false, Ordering::Release);
        assert!(
            !registry.routes.borrow()[&second]
                .cloud
                .as_ref()
                .unwrap()
                .credential_active
                .load(Ordering::Acquire)
        );
        registry.install_cloud_if_changed(
            identity.physical.clone(),
            client.clone(),
            "old-token".into(),
            i64::MAX,
            authority.clone(),
        );
        let mut third = identity.physical.clone();
        third.parent_did = DeviceDid::new("9012").unwrap();
        registry.install_cloud_if_changed(
            third.clone(),
            client.clone(),
            "old-token".into(),
            i64::MAX,
            authority.clone(),
        );
        for device in [&identity.physical, &second, &third] {
            assert!(
                !registry.routes.borrow()[device]
                    .cloud
                    .as_ref()
                    .unwrap()
                    .credential_active
                    .load(Ordering::Acquire),
                "reconciling the rejected credential must not reopen it"
            );
        }

        registry.revoke_device(&identity.physical);
        registry.revoke_device(&second);
        registry.revoke_device(&third);
        registry.install_cloud_if_changed(
            third.clone(),
            client.clone(),
            "old-token".into(),
            i64::MAX,
            authority.clone(),
        );
        assert!(
            !registry.routes.borrow()[&third]
                .cloud
                .as_ref()
                .unwrap()
                .credential_active
                .load(Ordering::Acquire),
            "removing every device route must not revive a rejected credential"
        );
        registry.install_cloud_if_changed(
            identity.physical.clone(),
            client,
            "new-token".into(),
            i64::MAX,
            authority,
        );

        assert!(!registry.revoke_cloud_if_authority(&identity.physical, &old_lease));
        assert!(
            registry.routes.borrow()[&identity.physical]
                .cloud
                .as_ref()
                .unwrap()
                .authority
                .check()
        );
    }

    #[test]
    fn expired_credentials_are_rejected_from_memory_without_a_storage_check() {
        let (identity, _) = light();
        let registry = CurrentSessionRegistry::default();
        let client = Rc::new(
            CloudClient::for_test("http://127.0.0.1:9", Duration::from_millis(20)).unwrap(),
        );
        registry.install_cloud_if_changed(
            identity.physical.clone(),
            client,
            "expired-token".into(),
            unix_time() - 1,
            SessionAuthority::new(),
        );
        let transports = RuntimeTransports::new(registry);

        assert!(!transports.available_paths(&identity.physical).cloud);
        assert!(!SessionAuthority::new_until(unix_time() - 1).check());
    }

    #[test]
    fn removing_one_cloud_device_keeps_a_sibling_route_on_the_same_credential_live() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        store.xiaomi().replace(&record()).unwrap();
        let snapshot = store.xiaomi().snapshot().unwrap();
        let (identity, descriptor) = light();
        let authority = SessionAuthority::new();
        let registry = CurrentSessionRegistry::default();
        let (base, requests) = mock_server(vec![MockResponse::json(
            200,
            r#"{"code":0,"result":[{"did":"5678","siid":2,"piid":1,"code":0}]}"#,
        )]);
        let client = Rc::new(CloudClient::for_test(&base, Duration::from_millis(200)).unwrap());
        registry.install_cloud_if_changed(
            identity.physical.clone(),
            client.clone(),
            "token".into(),
            i64::MAX,
            authority.clone(),
        );
        let mut sibling = identity.physical.clone();
        sibling.parent_did = DeviceDid::new("5678").unwrap();
        registry.install_cloud_if_changed(
            sibling.clone(),
            client,
            "token".into(),
            i64::MAX,
            authority,
        );

        registry.revoke_cloud(&identity.physical);

        let transports = Rc::new(RuntimeTransports::new(registry));
        assert!(!transports.available_paths(&identity.physical).cloud);
        assert!(transports.available_paths(&sibling).cloud);
        let sibling_identity = FeatureIdentity {
            physical: sibling,
            service_instance: identity.service_instance,
            role: identity.role,
        };
        let service = DeviceService::new();
        service.publish(
            sibling_identity.clone(),
            "Sibling",
            descriptor.capabilities.clone(),
        );
        service.set_state_availability(&sibling_identity, true);
        let commands = super::super::CommandRuntime::new(service.clone(), transports);
        commands.register(super::super::RuntimeFeature {
            identity: sibling_identity.clone(),
            descriptor,
            authority_generation: 1,
            auth_session_generation: snapshot.session_generation,
        });
        let ticket = service.command(&sibling_identity, DeviceCommand::SetPower(false));
        async_io::block_on(commands.run_until_idle());
        assert_eq!(async_io::block_on(ticket), CommandOutcome::Accepted);
        assert_eq!(
            requests
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .target,
            "/app/v2/miotspec/prop/set"
        );
    }

    #[test]
    fn a_dead_gateway_session_selects_cloud_before_the_coordinator_drains_disconnect() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        store.xiaomi().replace(&record()).unwrap();
        let snapshot = store.xiaomi().snapshot().unwrap();
        let (identity, descriptor) = light();
        let root = SessionAuthority::new();
        let gateway_authority = root.fresh_lease();
        let (_connection, mqtt, messages) = crate::xiaomi::mqtt::MqttConnection::new(
            crate::xiaomi::mqtt::MqttConfig::new(
                "dead-gateway-test",
                None,
                Duration::from_secs(60),
            )
            .with_endpoint("127.0.0.1", 9),
        )
        .unwrap();
        let (_gateway, gateway_handle, _notifications) =
            crate::xiaomi::gateway::GatewaySession::new(
                "dead-gateway-test",
                1,
                "1",
                crate::xiaomi::discovery::NetworkEpoch::new(7),
                mqtt,
                messages,
            )
            .unwrap();
        let (base, requests) = mock_server(vec![MockResponse::json(
            200,
            r#"{"code":0,"result":[{"did":"1234","siid":2,"piid":1,"code":0}]}"#,
        )]);
        let registry = CurrentSessionRegistry::default();
        registry.install_gateway(
            identity.physical.clone(),
            gateway_handle,
            gateway_authority.clone(),
        );
        registry.install_cloud_if_changed(
            identity.physical.clone(),
            Rc::new(CloudClient::for_test(&base, Duration::from_millis(200)).unwrap()),
            "access-token".into(),
            i64::MAX,
            root,
        );
        gateway_authority.revoke();

        let service = DeviceService::new();
        service.publish(
            identity.clone(),
            "Fallback light",
            descriptor.capabilities.clone(),
        );
        service.set_state_availability(&identity, true);
        let transports = Rc::new(RuntimeTransports::new(registry));
        assert_eq!(
            transports.available_paths(&identity.physical),
            super::super::OperationPaths {
                gateway: false,
                lan: false,
                cloud: true,
            }
        );
        let commands = super::super::CommandRuntime::new(service.clone(), transports);
        commands.register(super::super::RuntimeFeature {
            identity: identity.clone(),
            descriptor,
            authority_generation: 1,
            auth_session_generation: snapshot.session_generation,
        });
        let ticket = service.command(&identity, DeviceCommand::SetPower(false));
        async_io::block_on(commands.run_until_idle());
        assert_eq!(async_io::block_on(ticket), CommandOutcome::Accepted);
        let request = requests.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(request.target.ends_with("/miotspec/prop/set"));
        assert!(requests.try_recv().is_err());
    }
}
