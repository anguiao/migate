use super::*;
use crate::storage::XiaomiRecord;
use crate::xiaomi::{
    adapter::network::Network,
    central::{self, CentralError, SessionDriver, SessionHandle},
    cloud::{self, NotificationDriver, NotificationError, NotificationHandle},
    lan,
};
use futures_util::future::LocalBoxFuture;
use std::{cell::Cell, net::Ipv4Addr};

pub(in crate::xiaomi::adapter::tests) struct TestNetwork {
    gateway_ca: Option<String>,
    pub(in crate::xiaomi::adapter::tests) notification_results: Cell<usize>,
    gateway_request_timeout: Cell<Option<Duration>>,
    pub(in crate::xiaomi::adapter::tests) notification_address: Cell<Option<SocketAddr>>,
    pub(in crate::xiaomi::adapter::tests) lan_port: Cell<u16>,
    pub(in crate::xiaomi::adapter::tests) lan_timeout: Cell<Duration>,
    pub(in crate::xiaomi::adapter::tests) lan_endpoints: RefCell<BTreeMap<Ipv4Addr, SocketAddr>>,
    discovery_target: Cell<Option<(SocketAddr, Duration)>>,
}

impl TestNetwork {
    pub(in crate::xiaomi::adapter::tests) fn new(gateway_ca: Option<String>) -> Rc<Self> {
        Rc::new(Self {
            gateway_ca,
            notification_results: Cell::new(0),
            gateway_request_timeout: Cell::new(None),
            notification_address: Cell::new(None),
            lan_port: Cell::new(lan::PORT),
            lan_timeout: Cell::new(lan::TIMEOUT),
            lan_endpoints: RefCell::default(),
            discovery_target: Cell::new(None),
        })
    }

    pub(in crate::xiaomi::adapter::tests) fn use_short_gateway_request_timeout(&self) {
        self.gateway_request_timeout
            .set(Some(Duration::from_millis(400)));
    }

    pub(in crate::xiaomi::adapter::tests) fn discover_at(
        &self,
        address: SocketAddr,
        timeout: Duration,
    ) {
        self.discovery_target.set(Some((address, timeout)));
    }

    pub(in crate::xiaomi::adapter::tests) fn build(
        self: &Rc<Self>,
        store: Store,
        cloud: CloudClient,
        discovery: Discovery,
        clock: Arc<dyn Clock>,
    ) -> Result<(XiaomiAdapter, TopologyReady), AdapterError> {
        let auth = crate::xiaomi::auth::AuthService::for_adapter_test(
            store.xiaomi(),
            cloud.clone(),
            cloud.test_base_url(),
        );
        XiaomiAdapter::with_services(store, cloud, discovery, clock, auth, self.clone())
    }
}

impl Network for TestNetwork {
    fn gateway<'a>(
        &'a self,
        address: SocketAddr,
        credentials: &'a XiaomiRecord,
    ) -> LocalBoxFuture<'a, Result<(SessionHandle, SessionDriver), CentralError>> {
        match &self.gateway_ca {
            Some(ca) => Box::pin(central::tests::support::connect(
                address,
                credentials,
                ca,
                self.gateway_request_timeout.get(),
            )),
            None => Box::pin(central::connect(address, credentials)),
        }
    }
    fn notifications<'a>(
        &'a self,
        credentials: &'a XiaomiRecord,
    ) -> LocalBoxFuture<'a, Result<(NotificationHandle, NotificationDriver), NotificationError>>
    {
        match self.notification_address.get() {
            Some(address) => Box::pin(async move {
                let result =
                    cloud::tests::notification_support::connect(address, credentials).await;
                self.notification_results
                    .set(self.notification_results.get() + 1);
                result
            }),
            None => Box::pin(future::pending()),
        }
    }
    fn lan_target(&self, address: Ipv4Addr) -> (SocketAddr, Duration) {
        let target = self
            .lan_endpoints
            .borrow()
            .get(&address)
            .copied()
            .unwrap_or_else(|| (address, self.lan_port.get()).into());
        (target, self.lan_timeout.get())
    }
    fn discover_lan<'a>(
        &'a self,
        discovery: &'a lan::Discovery,
        did: u64,
    ) -> LocalBoxFuture<'a, Option<Ipv4Addr>> {
        match self.discovery_target.get() {
            Some((target, timeout)) => Box::pin(discovery.address_at(did, target, timeout)),
            None => Box::pin(future::ready(None)),
        }
    }
}
