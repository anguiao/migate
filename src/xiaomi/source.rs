use super::{
    auth::{AuthError, AuthReport, AuthService, AuthenticationState, FailureReason},
    catalog::{self, CatalogError},
    central::{self, CentralError, CentralEvent, DeviceAccess, SessionHandle},
    cloud::{CloudClient, CloudError},
    discovery::{Discovery, DiscoveryError, GatewayRoute},
};
use crate::{
    device::{Availability, Clock, CommandError, CommandReceiver, Device, DeviceKey},
    storage::{CachedDevice, StorageError, Store, XiaomiRecord},
};
use event_listener::Event;
use futures_lite::future;
use std::{
    cell::RefCell,
    error::Error,
    fmt,
    future::Future,
    pin::Pin,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

const DISCOVERY_WAIT: Duration = Duration::from_secs(30);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

#[derive(Clone, Debug)]
pub enum SourceError {
    Storage(StorageError),
    Cloud(CloudError),
    Discovery(DiscoveryError),
    Central(CentralError),
    Unavailable(String),
    NoLocalGateway,
    StalePreparation,
    Cancelled,
}
impl fmt::Display for SourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Storage(e) => e.fmt(f),
            Self::Cloud(e) => e.fmt(f),
            Self::Discovery(e) => e.fmt(f),
            Self::Central(e) => e.fmt(f),
            Self::Unavailable(e) => f.write_str(e),
            Self::NoLocalGateway => f.write_str("no current local gateway for the device home"),
            Self::StalePreparation => f.write_str("gateway preparation was superseded"),
            Self::Cancelled => f.write_str("Xiaomi device source stopped"),
        }
    }
}
impl Error for SourceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Storage(error) => Some(error),
            Self::Cloud(error) => Some(error),
            Self::Discovery(error) => Some(error),
            Self::Central(error) => Some(error),
            _ => None,
        }
    }
}
impl From<StorageError> for SourceError {
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}
impl From<CentralError> for SourceError {
    fn from(error: CentralError) -> Self {
        Self::Central(error)
    }
}
impl From<DiscoveryError> for SourceError {
    fn from(error: DiscoveryError) -> Self {
        Self::Discovery(error)
    }
}
impl From<AuthError> for SourceError {
    fn from(error: AuthError) -> Self {
        match error {
            AuthError::Storage(e) => Self::Storage(e),
            AuthError::Cloud(e) => Self::Cloud(e),
        }
    }
}
impl From<CatalogError> for SourceError {
    fn from(error: CatalogError) -> Self {
        match error {
            CatalogError::Cloud(e) => Self::Cloud(e),
            error => Self::Unavailable(error.to_string()),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GatewayStatus {
    Discovering,
    Connecting(String),
    Connected(String),
    Unavailable(String),
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceStatus {
    pub authentication: AuthReport,
    pub gateway: GatewayStatus,
    pub catalog_problem: Option<String>,
}
#[derive(Clone)]
pub struct PreparedDevice {
    pub device: Device,
    pub metadata: CachedDevice,
}
struct Ready {
    result: RefCell<Option<Result<PreparedDevice, SourceError>>>,
    completed: std::cell::Cell<bool>,
    changed: Event,
}
impl Ready {
    fn finish(&self, result: Result<PreparedDevice, SourceError>) {
        if !self.completed.replace(true) {
            *self.result.borrow_mut() = Some(result);
            self.changed.notify(usize::MAX);
        }
    }
}
/// Poll the source runner alongside this one-shot topology wait. The runner
/// continues owning the live session after the topology has been delivered.
pub struct TopologyReady(Rc<Ready>);
impl TopologyReady {
    pub async fn wait(self) -> Result<PreparedDevice, SourceError> {
        loop {
            let listener = self.0.changed.listen();
            if let Some(result) = self.0.result.borrow_mut().take() {
                return result;
            }
            listener.await;
        }
    }
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
#[cfg(test)]
struct SharedClock(Arc<dyn Clock>);
#[cfg(test)]
impl Clock for SharedClock {
    fn now(&self) -> Duration {
        self.0.now()
    }
    fn wait_until(&self, deadline: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        self.0.wait_until(deadline)
    }
}

struct Live {
    credentials: Option<XiaomiRecord>,
    metadata: Option<CachedDevice>,
    device: Option<Device>,
    generation: Option<u64>,
    handle: Option<SessionHandle>,
    preparation: u64,
    needs_prepare: bool,
    refresh_pending: bool,
    refresh_again: bool,
    available_since: Option<Duration>,
    stable: bool,
    reset_backoff: bool,
    blocked: Option<String>,
    dirty: bool,
}
struct Context {
    live: RefCell<Live>,
    changed: Event,
    status: RefCell<SourceStatus>,
    status_changed: Event,
    ready: Rc<Ready>,
    clock: Arc<dyn Clock>,
}
impl Context {
    fn device(&self) -> Device {
        self.live.borrow().device.as_ref().unwrap().clone()
    }
    fn metadata(&self) -> CachedDevice {
        self.live.borrow().metadata.as_ref().unwrap().clone()
    }
    fn current(&self, generation: u64, preparation: u64) -> bool {
        let live = self.live.borrow();
        live.generation == Some(generation)
            && live.preparation == preparation
            && live.blocked.is_none()
    }
    fn session_current(&self, generation: u64) -> bool {
        self.live.borrow().generation == Some(generation)
    }
    fn set_gateway(&self, status: GatewayStatus) {
        let mut current = self.status.borrow_mut();
        if current.gateway != status {
            current.gateway = status;
            self.status_changed.notify(usize::MAX);
        }
    }
    fn unavailable(&self, reason: String) {
        let device = {
            let mut live = self.live.borrow_mut();
            live.preparation = live.preparation.wrapping_add(1);
            live.needs_prepare = true;
            if live
                .available_since
                .is_some_and(|since| self.clock.now().saturating_sub(since) >= MAX_BACKOFF)
            {
                live.reset_backoff = true;
            }
            live.available_since = None;
            live.stable = false;
            live.device.clone()
        };
        if let Some(device) = device {
            device.set_availability(Availability::Unavailable(reason));
        }
        self.changed.notify(usize::MAX);
    }
    fn disconnect(&self, reason: String) {
        let handle = {
            let mut live = self.live.borrow_mut();
            live.generation = None;
            live.refresh_pending = false;
            live.refresh_again = false;
            live.handle.take()
        };
        self.unavailable(reason.clone());
        self.set_gateway(GatewayStatus::Unavailable(reason));
        if let Some(handle) = handle {
            handle.close();
        }
    }
    fn block(&self, reason: String) {
        self.live.borrow_mut().blocked = Some(reason.clone());
        self.disconnect(reason);
    }
    fn prepared(&self, generation: u64, version: u64, value: bool) {
        if !self.current(generation, version) {
            return;
        }
        let device = self.device();
        device.observe(value);
        device.set_availability(Availability::Available);
        let mut live = self.live.borrow_mut();
        live.needs_prepare = false;
        live.available_since = Some(self.clock.now());
        self.changed.notify(usize::MAX);
    }
    async fn wait_reprepare(&self, generation: u64, version: u64) {
        loop {
            let listener = self.changed.listen();
            if !self.current(generation, version) || self.live.borrow().needs_prepare {
                return;
            }
            listener.await;
        }
    }
}

/// A single device source. Network sessions can change; its Device and command
/// receiver remain alive for the complete bridge lifetime.
pub struct XiaomiSource {
    store: Store,
    cloud: CloudClient,
    auth: AuthService,
    discovery: Discovery,
    key: DeviceKey,
    context: Rc<Context>,
    receiver: Option<CommandReceiver>,
    cached: bool,
    #[cfg(test)]
    test_ca: Option<String>,
    #[cfg(test)]
    request_timeout: Duration,
}
impl XiaomiSource {
    pub fn new(
        store: Store,
        cloud: CloudClient,
        discovery: Discovery,
        key: DeviceKey,
    ) -> Result<(Self, TopologyReady), SourceError> {
        Self::build(
            store,
            cloud,
            discovery,
            key,
            Arc::new(SystemClock(Instant::now())),
        )
    }
    fn build(
        store: Store,
        cloud: CloudClient,
        discovery: Discovery,
        key: DeviceKey,
        clock: Arc<dyn Clock>,
    ) -> Result<(Self, TopologyReady), SourceError> {
        if key.source != "xiaomi"
            || key.id.is_empty()
            || key.id.trim() != key.id
            || key.id.chars().any(char::is_control)
        {
            return Err(SourceError::Unavailable("invalid Xiaomi device key".into()));
        }
        let auth = AuthService::new(store.xiaomi(), cloud.clone());
        let credentials = auth.saved_bridge_credentials()?;
        let cached = auth.load_bridge_cache(&key)?;
        let report = auth.local_status()?;
        let ready = Rc::new(Ready {
            result: RefCell::new(None),
            completed: std::cell::Cell::new(false),
            changed: Event::new(),
        });
        let (device, receiver) = if let Some(metadata) = &cached {
            let (device, receiver) = make_device(metadata, clock.clone());
            device.set_availability(Availability::Unavailable(
                "restoring local gateway connection".into(),
            ));
            ready.finish(Ok(PreparedDevice {
                device: device.clone(),
                metadata: metadata.clone(),
            }));
            (Some(device), Some(receiver))
        } else {
            (None, None)
        };
        let was_cached = cached.is_some();
        let context = Rc::new(Context {
            live: RefCell::new(Live {
                credentials,
                metadata: cached,
                device,
                generation: None,
                handle: None,
                preparation: 0,
                needs_prepare: true,
                refresh_pending: false,
                refresh_again: false,
                available_since: None,
                stable: false,
                reset_backoff: false,
                blocked: None,
                dirty: false,
            }),
            changed: Event::new(),
            status: RefCell::new(SourceStatus {
                authentication: report,
                gateway: GatewayStatus::Discovering,
                catalog_problem: None,
            }),
            status_changed: Event::new(),
            ready: ready.clone(),
            clock: clock.clone(),
        });
        Ok((
            Self {
                store,
                cloud,
                auth,
                discovery,
                key,
                context,
                receiver,
                cached: was_cached,
                #[cfg(test)]
                test_ca: None,
                #[cfg(test)]
                request_timeout: Duration::from_secs(5),
            },
            TopologyReady(ready),
        ))
    }
    #[cfg(test)]
    fn for_test(
        store: Store,
        cloud: CloudClient,
        discovery: Discovery,
        key: DeviceKey,
        ca: String,
        clock: Arc<dyn Clock>,
    ) -> Result<(Self, TopologyReady), SourceError> {
        let (mut source, ready) = Self::build(store, cloud, discovery, key, clock)?;
        source.test_ca = Some(ca);
        source.request_timeout = Duration::from_millis(200);
        Ok((source, ready))
    }
    pub async fn run(mut self, mut on_status: impl FnMut(SourceStatus)) -> Result<(), SourceError> {
        on_status(self.context.status.borrow().clone());
        let context = self.context.clone();
        let result = {
            let monitor = async {
                loop {
                    let listener = context.status_changed.listen();
                    on_status(context.status.borrow().clone());
                    listener.await;
                }
            };
            future::or(self.run_inner(), monitor).await
        };
        on_status(self.context.status.borrow().clone());
        if let Err(error) = &result {
            self.context.ready.finish(Err(error.clone()));
        }
        result
    }
    async fn run_inner(&mut self) -> Result<(), SourceError> {
        if !self.cached {
            let preparation = self.auth.prepare_bridge(&self.key).await?;
            update_auth(&self.context, &preparation);
            let credentials = preparation
                .credentials
                .ok_or_else(|| SourceError::Unavailable("Xiaomi sign-in is required".into()))?;
            if preparation.homes.is_empty() {
                return Err(authentication_error(&preparation.report));
            }
            let target_homes = preparation
                .homes
                .iter()
                .filter(|home| home.uid == credentials.uid && home.dids.contains(&self.key.id))
                .cloned()
                .collect::<Vec<_>>();
            if target_homes.is_empty() {
                return Err(CatalogError::TargetMissing.into());
            }
            let mut changes = self.discovery.subscribe();
            let mut discovery_deadline = None;
            loop {
                if discovery_deadline.is_some_and(|deadline| self.context.clock.now() >= deadline) {
                    return Err(SourceError::NoLocalGateway);
                }
                let snapshot = self.discovery.snapshot();
                match catalog::load_scoped(
                    &snapshot,
                    &target_homes,
                    &credentials,
                    &self.key,
                    &self.cloud,
                )
                .await
                {
                    Ok(metadata) => {
                        let (device, receiver) = make_device(&metadata, self.context.clock.clone());
                        self.receiver = Some(receiver);
                        let mut live = self.context.live.borrow_mut();
                        live.device = Some(device);
                        live.metadata = Some(metadata);
                        live.dirty = true;
                        break;
                    }
                    Err(CatalogError::NoLocalHome) => {
                        let deadline = *discovery_deadline
                            .get_or_insert_with(|| self.context.clock.now() + DISCOVERY_WAIT);
                        if self.context.clock.now() >= deadline {
                            return Err(SourceError::NoLocalGateway);
                        }
                        future::or(
                            async { changes.changed().await.map(|_| ()).map_err(Into::into) },
                            async {
                                self.context.clock.wait_until(deadline).await;
                                Err(SourceError::NoLocalGateway)
                            },
                        )
                        .await?;
                    }
                    Err(error) => return Err(error.into()),
                }
            }
        }
        let context = self.context.clone();
        let store = self.store.clone();
        let discovery = self.discovery.clone();
        let cached = self.cached;
        #[cfg(test)]
        let test = (self.test_ca.clone(), self.request_timeout);
        let receiver = self.receiver.as_mut().unwrap();
        let local = local_run(
            receiver,
            context.clone(),
            store.clone(),
            discovery.clone(),
            cached,
            #[cfg(test)]
            test,
        );
        if cached {
            future::or(
                local,
                cached_auth(
                    &self.auth,
                    &self.cloud,
                    &self.key,
                    &discovery,
                    &context,
                    &store,
                ),
            )
            .await
        } else {
            local.await
        }
    }
}
impl Drop for XiaomiSource {
    fn drop(&mut self) {
        drop(self.receiver.take());
        self.context
            .disconnect("Xiaomi device source stopped".into());
        self.context.ready.finish(Err(SourceError::Cancelled));
    }
}

fn make_device(metadata: &CachedDevice, clock: Arc<dyn Clock>) -> (Device, CommandReceiver) {
    #[cfg(test)]
    {
        Device::for_test(
            metadata.outlet.description.clone(),
            Box::new(SharedClock(clock)),
        )
    }
    #[cfg(not(test))]
    {
        let _ = clock;
        Device::new(metadata.outlet.description.clone())
    }
}
fn update_auth(context: &Context, preparation: &super::auth::BridgePreparation) {
    let credentials_changed = {
        let mut live = context.live.borrow_mut();
        let changed = live.credentials != preparation.credentials;
        live.credentials = preparation.credentials.clone();
        changed
    };
    context.status.borrow_mut().authentication = preparation.report.clone();
    context.status_changed.notify(usize::MAX);
    if credentials_changed {
        context.changed.notify(usize::MAX);
    }
}
fn authentication_error(report: &AuthReport) -> SourceError {
    match &report.authentication {
        AuthenticationState::Unavailable(FailureReason::Cloud(error))
        | AuthenticationState::SignInRequired(FailureReason::Cloud(error)) => {
            SourceError::Cloud(error.clone())
        }
        AuthenticationState::Unavailable(reason) | AuthenticationState::SignInRequired(reason) => {
            SourceError::Unavailable(reason.to_string())
        }
        _ => SourceError::Unavailable("Xiaomi owned home directory is unavailable".into()),
    }
}
async fn cached_auth(
    auth: &AuthService,
    cloud: &CloudClient,
    key: &DeviceKey,
    discovery: &Discovery,
    context: &Rc<Context>,
    store: &Store,
) -> Result<(), SourceError> {
    let preparation = auth.prepare_bridge(key).await?;
    update_auth(context, &preparation);
    let metadata = context.metadata();
    let authoritative = !preparation.homes.is_empty()
        || matches!(
            preparation.report.authentication,
            AuthenticationState::Authenticated
        );
    if authoritative
        && !preparation
            .homes
            .iter()
            .any(|home| home.uid == metadata.uid && home.dids.contains(&key.id))
    {
        context.block("target device is missing from the current owned home directory".into());
        future::pending().await
    }
    let Some(credentials) = preparation.credentials else {
        context.block("Xiaomi sign-in is required".into());
        return future::pending().await;
    };
    if preparation.homes.is_empty() {
        return future::pending().await;
    }
    let target_homes = preparation
        .homes
        .iter()
        .filter(|home| home.uid == credentials.uid && home.dids.contains(&key.id))
        .cloned()
        .collect::<Vec<_>>();
    let moved = !target_homes
        .iter()
        .any(|home| home.home_id == metadata.home_id);
    if moved {
        context.block("waiting for the device's current owned home gateway".into());
    }
    let mut changes = discovery.subscribe();
    loop {
        match catalog::load_scoped(
            &discovery.snapshot(),
            &target_homes,
            &credentials,
            key,
            cloud,
        )
        .await
        {
            Ok(fresh) => {
                let changed = context.metadata() != fresh;
                {
                    let mut live = context.live.borrow_mut();
                    live.metadata = Some(fresh);
                    live.blocked = None;
                    live.dirty = true;
                }
                if changed {
                    context.disconnect("device description updated".into());
                } else if matches!(
                    context.device().snapshot().availability,
                    Availability::Available
                ) {
                    store.devices().save(&context.metadata())?;
                    context.live.borrow_mut().dirty = false;
                }
                break;
            }
            Err(CatalogError::NoLocalHome) => {
                changes.changed().await?;
            }
            Err(CatalogError::Cloud(error)) => {
                context.status.borrow_mut().catalog_problem = Some(error.to_string());
                if error.is_unauthorized() {
                    context.status.borrow_mut().authentication.authentication =
                        AuthenticationState::SignInRequired(FailureReason::Cloud(error.clone()));
                }
                context.status_changed.notify(usize::MAX);
                log::warn!("Xiaomi static description refresh failed: {error}");
                break;
            }
            Err(error) => {
                context.block(error.to_string());
                break;
            }
        }
    }
    future::pending().await
}

struct Backoff {
    delay: Duration,
}
impl Backoff {
    fn new() -> Self {
        Self {
            delay: Duration::from_secs(1),
        }
    }
    fn failed(&mut self, stable: bool) -> Duration {
        if stable {
            self.delay = Duration::from_secs(1);
        }
        let delay = self.delay;
        self.delay = (self.delay * 2).min(MAX_BACKOFF);
        delay
    }
}
async fn local_run(
    receiver: &mut CommandReceiver,
    context: Rc<Context>,
    store: Store,
    discovery: Discovery,
    cached: bool,
    #[cfg(test)] test: (Option<String>, Duration),
) -> Result<(), SourceError> {
    let mut backoff = Backoff::new();
    let mut route: Option<GatewayRoute> = None;
    let mut changes = discovery.subscribe();
    loop {
        if context.live.borrow().blocked.is_some() {
            let listener = context.changed.listen();
            listener.await;
            continue;
        }
        let metadata = context.metadata();
        let snapshot = discovery.snapshot();
        let selected = snapshot.route(&metadata.group_id, route.as_ref());
        let Some(selected) = selected else {
            context.disconnect(SourceError::NoLocalGateway.to_string());
            if !cached && !context.ready.completed.get() {
                return Err(SourceError::NoLocalGateway);
            }
            let listener = context.changed.listen();
            future::or(
                async {
                    changes
                        .changed()
                        .await
                        .map(|_| ())
                        .map_err(SourceError::Discovery)
                },
                async {
                    listener.await;
                    Ok(())
                },
            )
            .await?;
            continue;
        };
        let credentials = context.live.borrow().credentials.clone();
        let Some(credentials) = credentials else {
            context.block("Xiaomi sign-in is required".into());
            continue;
        };
        let addresses = snapshot
            .gateways
            .iter()
            .find(|gateway| gateway.did == selected.did && gateway.group_id == selected.group_id)
            .unwrap()
            .addresses
            .clone();
        context.set_gateway(GatewayStatus::Connecting(selected.did.clone()));
        let mut connected = None;
        let mut failure = SourceError::NoLocalGateway;
        for address in std::iter::once(selected.address).chain(
            addresses
                .into_iter()
                .filter(|address| *address != selected.address),
        ) {
            if !discovery.snapshot().gateways.iter().any(|gateway| {
                gateway.did == selected.did
                    && gateway.group_id == selected.group_id
                    && gateway.addresses.contains(&address)
            }) {
                continue;
            }
            #[cfg(test)]
            let result = if let Some(ca) = &test.0 {
                central::test_support::connect(address, &credentials, ca, test.1).await
            } else {
                central::connect(address, &credentials).await
            };
            #[cfg(not(test))]
            let result = central::connect(address, &credentials).await;
            match result {
                Ok(session) => {
                    let live = context.live.borrow();
                    let valid = live.blocked.is_none()
                        && live.metadata.as_ref() == Some(&metadata)
                        && credentials.uid == metadata.uid
                        && discovery.snapshot().gateways.iter().any(|gateway| {
                            gateway.did == selected.did
                                && gateway.group_id == selected.group_id
                                && gateway.addresses.contains(&address)
                        });
                    drop(live);
                    if !valid {
                        session.0.close();
                        failure = SourceError::StalePreparation;
                        break;
                    }
                    let mut selected = selected.clone();
                    selected.address = address;
                    connected = Some((selected, session));
                    break;
                }
                Err(error) => failure = error.into(),
            }
        }
        let result = if let Some((selected, (handle, driver))) = connected {
            route = Some(selected.clone());
            let generation = handle.generation();
            {
                let mut live = context.live.borrow_mut();
                live.generation = Some(generation);
                live.handle = Some(handle.clone());
                live.preparation = live.preparation.wrapping_add(1);
                live.needs_prepare = true;
                live.refresh_pending = false;
                live.refresh_again = false;
                live.stable = false;
            }
            context.set_gateway(GatewayStatus::Connected(selected.did.clone()));
            let events = context.clone();
            let event_handle = handle.clone();
            let driving = async {
                driver
                    .run(move |event| match event {
                        CentralEvent::Property {
                            did,
                            binding,
                            value,
                        } => {
                            if events.session_current(generation) {
                                let metadata = events.metadata();
                                if did == metadata.outlet.description.key.id
                                    && binding == metadata.outlet.main_power
                                {
                                    events.device().observe(value);
                                }
                            }
                        }
                        CentralEvent::DeviceListChanged { dids } => {
                            if dids.contains(&events.metadata().outlet.description.key.id) {
                                refresh_access(&event_handle, &events, generation);
                            }
                        }
                    })
                    .await
                    .map_err(SourceError::Central)
            };
            let working = session_work(receiver, &handle, &context, &store, generation);
            let routing = route_watch(&discovery, &mut changes, &context, &selected, generation);
            // Invalidate a removed route before polling any old session reply.
            future::or(routing, future::or(driving, working)).await
        } else {
            Err(failure)
        };
        let error = result.err().unwrap_or(SourceError::Cancelled);
        context.disconnect(error.to_string());
        let stable = std::mem::take(&mut context.live.borrow_mut().reset_backoff);
        if matches!(error, SourceError::Storage(_) | SourceError::Discovery(_))
            || !cached && !context.ready.completed.get()
        {
            return Err(error);
        }
        wait_retry(
            &context,
            &discovery,
            &mut changes,
            route.as_ref(),
            backoff.failed(stable),
        )
        .await?;
    }
}
async fn wait_retry(
    context: &Context,
    discovery: &Discovery,
    changes: &mut super::discovery::DiscoveryChanges,
    previous_route: Option<&GatewayRoute>,
    delay: Duration,
) -> Result<(), SourceError> {
    let listener = context.changed.listen();
    let group_id = context.metadata().group_id;
    let route = discovery.snapshot().route(&group_id, previous_route);
    future::or(
        async {
            context.clock.wait_until(context.clock.now() + delay).await;
            Ok(())
        },
        future::or(
            async {
                loop {
                    changes.changed().await?;
                    if discovery.snapshot().route(&group_id, route.as_ref()) != route {
                        return Ok(());
                    }
                }
            },
            async {
                listener.await;
                Ok(())
            },
        ),
    )
    .await
}
async fn route_watch(
    discovery: &Discovery,
    changes: &mut super::discovery::DiscoveryChanges,
    context: &Context,
    route: &GatewayRoute,
    generation: u64,
) -> Result<(), SourceError> {
    loop {
        let listener = context.changed.listen();
        if !context.session_current(generation)
            || discovery
                .snapshot()
                .route(&route.group_id, Some(route))
                .as_ref()
                != Some(route)
        {
            context.disconnect("current gateway route changed".into());
            return Err(SourceError::StalePreparation);
        }
        future::or(
            async { changes.changed().await.map(|_| ()).map_err(Into::into) },
            async {
                listener.await;
                Ok::<(), SourceError>(())
            },
        )
        .await?;
    }
}
fn access(
    result: Result<Option<DeviceAccess>, CentralError>,
    metadata: &CachedDevice,
) -> Result<(), SourceError> {
    let access = result?.ok_or_else(|| {
        SourceError::Unavailable("target is absent from the local gateway".into())
    })?;
    if access.model != metadata.model || access.instance_type != metadata.instance_type {
        return Err(SourceError::Unavailable(
            "local device description differs from the current static description".into(),
        ));
    }
    if !access.online || !access.spec_v2_access || !access.push_available {
        return Err(SourceError::Unavailable(
            "local device is offline or lacks property/push access".into(),
        ));
    }
    Ok(())
}
fn refresh_access(handle: &SessionHandle, context: &Rc<Context>, generation: u64) {
    if !context.session_current(generation) {
        return;
    }
    {
        let mut live = context.live.borrow_mut();
        if live.refresh_pending {
            live.refresh_again = true;
            return;
        }
        live.refresh_pending = true;
    }
    let metadata = context.metadata();
    let did = metadata.outlet.description.key.id.clone();
    let updated = context.clone();
    let next_handle = handle.clone();
    drop(handle.get_device_list(&did, move |result| {
        if !updated.session_current(generation) {
            return;
        }
        let outcome = access(result, &metadata);
        let again = {
            let mut live = updated.live.borrow_mut();
            live.refresh_pending = false;
            let again = live.refresh_again;
            live.refresh_again = false;
            again
        };
        match outcome {
            Err(error) => updated.unavailable(error.to_string()),
            Ok(()) => {
                if !matches!(
                    updated.device().snapshot().availability,
                    Availability::Available
                ) {
                    updated.live.borrow_mut().needs_prepare = true;
                    updated.changed.notify(usize::MAX);
                }
            }
        }
        if again {
            refresh_access(&next_handle, &updated, generation);
        }
    }));
}
async fn prepare(
    handle: &SessionHandle,
    context: &Rc<Context>,
    generation: u64,
) -> Result<(), SourceError> {
    let (version, metadata) = {
        let live = context.live.borrow();
        (live.preparation, live.metadata.as_ref().unwrap().clone())
    };
    let did = metadata.outlet.description.key.id.clone();
    let binding = metadata.outlet.main_power;
    let checked = Rc::new(RefCell::new(None));
    let outcome = checked.clone();
    let current = context.clone();
    let _ = handle
        .get_device_list(&did, move |result| {
            let result = access(result, &metadata);
            if current.current(generation, version)
                && let Err(error) = &result
            {
                current.unavailable(error.to_string());
            }
            *outcome.borrow_mut() = Some(result);
        })
        .await;
    checked.borrow_mut().take().unwrap()?;
    if !context.current(generation, version) {
        return Err(SourceError::StalePreparation);
    }
    handle.subscribe_property(&did, binding).await?;
    if !context.current(generation, version) {
        return Err(SourceError::StalePreparation);
    }
    let current = context.clone();
    handle
        .read_property(&did, binding, move |result| {
            if current.current(generation, version) {
                match result {
                    Ok(value) => current.prepared(generation, version, value),
                    Err(error) => current.unavailable(error.to_string()),
                }
            }
        })
        .await?;
    if !context.current(generation, version)
        || !matches!(
            context.device().snapshot().availability,
            Availability::Available
        )
    {
        return Err(SourceError::StalePreparation);
    }
    Ok(())
}
fn command_error(error: CentralError) -> CommandError {
    match error {
        CentralError::Timeout => CommandError::Timeout,
        CentralError::Rejected(code) => CommandError::Rejected(format!("gateway code {code}")),
        CentralError::Cancelled => CommandError::Cancelled,
        error => CommandError::SendFailed(error.to_string()),
    }
}
async fn session_work(
    receiver: &mut CommandReceiver,
    handle: &SessionHandle,
    context: &Rc<Context>,
    store: &Store,
    generation: u64,
) -> Result<(), SourceError> {
    let mut backoff = Backoff::new();
    loop {
        if let Err(error) = prepare(handle, context, generation).await {
            if !context.ready.completed.get() {
                return Err(error);
            }
            context.unavailable(error.to_string());
            let listener = context.changed.listen();
            future::or(
                context
                    .clock
                    .wait_until(context.clock.now() + backoff.failed(false)),
                async {
                    listener.await;
                },
            )
            .await;
            continue;
        }
        if context.live.borrow().dirty {
            store.devices().save(&context.metadata())?;
            context.live.borrow_mut().dirty = false;
        }
        context.ready.finish(Ok(PreparedDevice {
            device: context.device(),
            metadata: context.metadata(),
        }));
        let version = context.live.borrow().preparation;
        loop {
            if !context.current(generation, version) || context.live.borrow().needs_prepare {
                break;
            }
            enum Wake {
                Command(Option<crate::device::CommandRequest>),
                Prepare,
                Stable,
            }
            let since = context.live.borrow().available_since.unwrap();
            let wake = future::or(
                async { Wake::Command(receiver.next().await) },
                future::or(
                    async {
                        context.wait_reprepare(generation, version).await;
                        Wake::Prepare
                    },
                    async {
                        if context.live.borrow().stable {
                            future::pending::<()>().await
                        } else {
                            context.clock.wait_until(since + MAX_BACKOFF).await;
                        }
                        Wake::Stable
                    },
                ),
            )
            .await;
            match wake {
                Wake::Prepare => break,
                Wake::Stable => {
                    context.live.borrow_mut().stable = true;
                    context.live.borrow_mut().reset_backoff = true;
                    backoff = Backoff::new();
                }
                Wake::Command(Some(request)) => {
                    let remaining = request.remaining();
                    if remaining.is_zero() {
                        request.reject(CommandError::Timeout);
                        continue;
                    }
                    let metadata = context.metadata();
                    let current = context.clone();
                    let target = request.target;
                    let writing = handle.set_property(
                        &metadata.outlet.description.key.id,
                        metadata.outlet.main_power,
                        target,
                        remaining,
                        move |result| {
                            if !current.current(generation, version) {
                                request.reject(CommandError::Unavailable(
                                    "gateway preparation changed".into(),
                                ));
                            } else {
                                match result {
                                    Ok(()) => {
                                        let _ = request.accept();
                                    }
                                    Err(error) => request.reject(command_error(error)),
                                }
                            }
                        },
                    );
                    future::or(
                        async {
                            let _ = writing.await;
                        },
                        context.wait_reprepare(generation, version),
                    )
                    .await;
                }
                Wake::Command(None) => return Err(SourceError::Cancelled),
            }
        }
    }
}

#[cfg(test)]
mod tests;
