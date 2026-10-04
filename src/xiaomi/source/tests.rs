use super::*;
use crate::{
    device::{Availability, Clock, DeviceKey, Power},
    storage::{CachedPhysicalDevice, CachedScope, Store},
    xiaomi::{
        capability::parse_device,
        central::test_support::TlsFixture,
        cloud::{CloudClient, group_id},
        discovery::Discovery,
        test_support::{MockResponse, dynamic_mock_server},
    },
};
use event_listener::Event;
use futures_lite::future::{block_on, poll_once};
use serde_json::Value;
use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};
fn cached(uid: &str, did: &str, multi: bool) -> CachedPhysicalDevice {
    let instance: Value = serde_json::from_str(if multi {
        include_str!("../../../tests/fixtures/miot/cuco-cp7pd.json")
    } else {
        include_str!("../../../tests/fixtures/miot/outlet.json")
    })
    .unwrap();
    CachedPhysicalDevice {
        uid: uid.into(),
        home_id: "local".into(),
        group_id: group_id(uid, "local"),
        model: if multi {
            "cuco.plug.cp7pd"
        } else {
            "cuco.plug.v3"
        }
        .into(),
        instance_type: instance["type"].as_str().unwrap().into(),
        device: parse_device(DeviceKey::new("xiaomi", did), &instance).unwrap(),
    }
}
fn scope(uid: &str) -> CachedScope {
    CachedScope {
        uid: uid.into(),
        home_id: "local".into(),
        group_id: group_id(uid, "local"),
    }
}
#[test]
fn restored_topology_is_the_selected_scope_and_published_intersection() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let tls = TlsFixture::new(false);
    store.xiaomi().replace(&tls.credentials).unwrap();
    store
        .devices()
        .save_scope(&scope(&tls.credentials.uid))
        .unwrap();
    let kept = cached(&tls.credentials.uid, "kept", true);
    store.devices().publish_cached(&kept, "saved name").unwrap();
    let deleted = cached(&tls.credentials.uid, "deleted", false);
    store.devices().publish_cached(&deleted, "deleted").unwrap();
    store
        .devices()
        .remove(&deleted.device.description.key)
        .unwrap();
    let mut other = cached(&tls.credentials.uid, "other-home", false);
    other.home_id = "other".into();
    other.group_id = group_id(&other.uid, &other.home_id);
    store.devices().publish_cached(&other, "other").unwrap();
    store
        .devices()
        .publish_cached(&cached("foreign", "foreign", false), "foreign")
        .unwrap();
    let (base, requests) = dynamic_mock_server(1, |_| MockResponse::json(503, "{}"));
    let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    let (_events, receiver) = flume::unbounded();
    let (discovery, _driver) = Discovery::with_events(receiver);
    let (source, ready) = XiaomiSource::new(store, cloud, discovery).unwrap();
    let registry = block_on(poll_once(ready.wait()))
        .expect("cache readiness is synchronous")
        .unwrap();
    assert_eq!(registry.list().len(), 1);
    let physical = registry.get(&DeviceKey::new("xiaomi", "kept")).unwrap();
    assert_eq!(physical.functions().len(), 3);
    assert_eq!(physical.description().label, "saved name");
    for function in physical.functions() {
        assert_eq!(function.snapshot().power, Power::Unknown);
        assert!(matches!(
            function.snapshot().availability,
            Availability::Unavailable(_)
        ));
    }
    assert_eq!(requests.try_iter().count(), 0);
    drop(source);
}
#[derive(Default)]
struct TestClock {
    elapsed: Mutex<Duration>,
    deadlines: Mutex<Vec<Duration>>,
    changed: Event,
}
impl TestClock {
    fn advance(&self, by: Duration) {
        *self.elapsed.lock().unwrap() += by;
        self.changed.notify(usize::MAX);
    }
}
impl Clock for TestClock {
    fn now(&self) -> Duration {
        *self.elapsed.lock().unwrap()
    }
    fn wait_until(&self, deadline: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        self.deadlines.lock().unwrap().push(deadline);
        Box::pin(async move {
            loop {
                let listener = self.changed.listen();
                if self.now() >= deadline {
                    return;
                }
                listener.await;
            }
        })
    }
}

use crate::device::{Command, DeviceRegistry};
use crate::xiaomi::{
    central::{decode_mips, encode_mips, test_support::Broker},
    discovery::SERVICE_TYPE,
};
use async_io::Async;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures_lite::future;
use futures_rustls::TlsAcceptor;
use mqttbytes::{QoS, v5::*};
use serde_json::json;
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    net::{SocketAddr, TcpListener},
    rc::Rc,
};
#[derive(Default)]
struct CloudState {
    members: Vec<String>,
    status: u16,
    home_delay: Duration,
    home_gate: Option<flume::Receiver<()>>,
    foreign: bool,
    partial: bool,
    unsupported: BTreeSet<String>,
    detail_status: u16,
    detail_delay: Duration,
    detail_gate: Option<flume::Receiver<()>>,
    details: usize,
    detail_done: usize,
    cert_status: u16,
    certs: usize,
    token_status: u16,
    token_access: Option<String>,
    token_delay: Duration,
    token_gate: Option<flume::Receiver<()>>,
    tokens: usize,
    token_done: usize,
    homes: usize,
}
struct Rig {
    _directory: tempfile::TempDir,
    source: XiaomiSource,
    ready: TopologyReady,
    store: Store,
    discovery: Discovery,
    driver: crate::xiaomi::discovery::DiscoveryDriver,
    events: flume::Sender<mdns_sd::ServiceEvent>,
    listener: Async<TcpListener>,
    acceptor: TlsAcceptor,
    data: Vec<CachedPhysicalDevice>,
    cloud: Arc<Mutex<CloudState>>,
    clock: Arc<TestClock>,
}
struct Control {
    ready: Option<TopologyReady>,
    store: Store,
    context: Rc<super::state::Context>,
    discovery: Discovery,
    events: flume::Sender<mdns_sd::ServiceEvent>,
    listener: Async<TcpListener>,
    acceptor: TlsAcceptor,
    data: Vec<CachedPhysicalDevice>,
    cloud: Arc<Mutex<CloudState>>,
    clock: Arc<TestClock>,
}
impl Rig {
    fn new(restore: bool, members: &[&str]) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let tls = TlsFixture::new(false);
        store.xiaomi().replace(&tls.credentials).unwrap();
        let data = vec![
            cached(&tls.credentials.uid, "one", false),
            cached(&tls.credentials.uid, "multi", true),
            cached(&tls.credentials.uid, "two", false),
        ];
        if restore {
            store
                .devices()
                .save_scope(&scope(&tls.credentials.uid))
                .unwrap();
            for record in &data {
                if members.contains(&record.device.description.key.id.as_str()) {
                    store
                        .devices()
                        .publish_cached(
                            record,
                            &format!("name {}", record.device.description.key.id),
                        )
                        .unwrap();
                }
            }
        }
        let cloud = Arc::new(Mutex::new(CloudState {
            members: members.iter().map(|v| v.to_string()).collect(),
            status: 200,
            detail_status: 200,
            cert_status: 200,
            token_status: 200,
            ..Default::default()
        }));
        let responses = cloud.clone();
        let records = data.clone();
        let certificate = tls.credentials.certificate_pem.clone();
        let (base, _requests) = dynamic_mock_server(200, move |request| {
            let detail_finished = responses.clone();
            let token_finished = responses.clone();
            let mut state = responses.lock().unwrap();
            if state.status != 200 {
                return MockResponse::json(state.status, "{}");
            }
            let value = if request.target.contains("get_token") {
                state.tokens += 1;
                if state.token_status != 200 {
                    return MockResponse::json(state.token_status, "{}");
                }
                json!({"code":0,"result":{"access_token":state.token_access.as_deref().unwrap_or("fresh-access"),"refresh_token":"fresh-refresh","expires_in":1000}})
            } else if request.target.ends_with("/get_central_crt") {
                state.certs += 1;
                if state.cert_status != 200 {
                    return MockResponse::json(state.cert_status, "{}");
                }
                json!({"code":0,"result":{"cert":certificate}})
            } else if request.target.ends_with("/gethome") {
                state.homes += 1;
                if state.partial {
                    json!({"code":0,"result":{"homelist":[],"has_more":true}})
                } else {
                    json!({"code":0,"result":{"homelist":[{"id":"local","uid":if state.foreign {"foreign"}else {&records[0].uid},"roomlist":[],"dids":state.members}]}})
                }
            } else if request.target.ends_with("/device_list_page") {
                state.details += 1;
                if state.detail_status != 200 {
                    let mut response = MockResponse::json(state.detail_status, "{}")
                        .delayed(state.detail_delay)
                        .on_sent(move || detail_finished.lock().unwrap().detail_done += 1);
                    if let Some(gate) = state.detail_gate.clone() {
                        response = response.gated(gate);
                    }
                    return response;
                }
                json!({"code":0,"result":{"list":records.iter().filter(|record|state.members.contains(&record.device.description.key.id)).map(|record|json!({"did":record.device.description.key.id,"model":record.model,"spec_type":record.instance_type})).collect::<Vec<_>>()}})
            } else if request.target.starts_with("/miot-spec-v2/instance?") {
                let multi = request.target.contains("cuco-cp7pd");
                let mut value: Value = serde_json::from_str(if multi {
                    include_str!("../../../tests/fixtures/miot/cuco-cp7pd.json")
                } else {
                    include_str!("../../../tests/fixtures/miot/outlet.json")
                })
                .unwrap();
                if state
                    .unsupported
                    .contains(if multi { "multi" } else { "one" })
                {
                    value["services"] = json!([]);
                }
                value
            } else {
                return MockResponse::json(503, "{}");
            };
            let response = MockResponse::json(200, &value.to_string());
            if request.target.ends_with("/gethome") {
                {
                    let mut response = response.delayed(state.home_delay);
                    if let Some(gate) = state.home_gate.clone() {
                        response = response.gated(gate);
                    }
                    response
                }
            } else if request.target.contains("get_token") {
                let mut response = response
                    .delayed(state.token_delay)
                    .on_sent(move || token_finished.lock().unwrap().token_done += 1);
                if let Some(gate) = state.token_gate.clone() {
                    response = response.gated(gate);
                }
                response
            } else if request.target.ends_with("/device_list_page") {
                response.delayed(state.detail_delay)
            } else {
                response
            }
        });
        let client = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
        let listener = Async::<TcpListener>::bind(([127, 0, 0, 1], 0)).unwrap();
        let (events, receiver) = flume::unbounded();
        let (discovery, driver) = Discovery::with_events(receiver);
        let clock = Arc::new(TestClock::default());
        let (mut source, ready) =
            XiaomiSource::build(store.clone(), client, discovery.clone(), clock.clone()).unwrap();
        source.test_ca = Some(tls.ca_pem.clone());
        source.request_timeout = Duration::from_millis(150);
        Self {
            _directory: directory,
            source,
            ready,
            store,
            discovery,
            driver,
            events,
            listener,
            acceptor: TlsAcceptor::from(tls.server_config()),
            data,
            cloud,
            clock,
        }
    }
    async fn run<F, Fut>(self, test: F)
    where
        F: FnOnce(Control) -> Fut,
        Fut: Future<Output = ()>,
    {
        self.run_result(test).await.unwrap();
    }
    async fn run_result<F, Fut>(self, test: F) -> Result<(), SourceError>
    where
        F: FnOnce(Control) -> Fut,
        Fut: Future<Output = ()>,
    {
        let keep_events = self.events.clone();
        let context = self.source.context.clone();
        let running = self.source.run(|_| {});
        let discovering = async {
            self.driver.run().await?;
            future::pending::<Result<(), SourceError>>().await
        };
        let testing = test(Control {
            ready: Some(self.ready),
            store: self.store,
            context,
            discovery: self.discovery,
            events: self.events,
            listener: self.listener,
            acceptor: self.acceptor,
            data: self.data,
            cloud: self.cloud,
            clock: self.clock,
        });
        let result = future::or(
            future::or(future::or(running, discovering), async {
                testing.await;
                Ok(())
            }),
            async {
                async_io::Timer::after(Duration::from_secs(5)).await;
                panic!("source scenario stalled");
            },
        )
        .await;
        drop(keep_events);
        result
    }
}
impl Control {
    fn announce(&self) {
        self.events
            .send(announcement(
                self.listener.get_ref().local_addr().unwrap(),
                &self.data[0].uid,
                "hub",
                123,
            ))
            .unwrap();
    }
    async fn accept(&self) -> Broker {
        let (socket, _) = self.listener.accept().await.unwrap();
        Broker::new(self.acceptor.accept(socket).await.unwrap())
    }
    async fn peer<F, Fut>(&self, peer: Rc<Peer>, test: F)
    where
        F: FnOnce(Rc<Peer>) -> Fut,
        Fut: Future<Output = ()>,
    {
        let mut broker = self.accept().await;
        broker.setup().await;
        peer.state.borrow_mut().client_id = broker.client_id.clone();
        // The peer remains alive until every scenario assertion has completed.
        future::or(
            peer_run(broker, peer.clone(), self.data.clone()),
            test(peer),
        )
        .await;
    }
}
fn announcement(address: SocketAddr, uid: &str, name: &str, hub: u64) -> mdns_sd::ServiceEvent {
    let mut profile = [0; 23];
    profile[1..9].copy_from_slice(&hub.to_be_bytes());
    profile[9..17].copy_from_slice(
        &u64::from_str_radix(&group_id(uid, "local"), 16)
            .unwrap()
            .to_le_bytes(),
    );
    profile[20] = 0x10;
    profile[22] = 2;
    let profile = STANDARD.encode(profile);
    mdns_sd::ServiceEvent::ServiceResolved(Box::new(
        mdns_sd::ServiceInfo::new(
            SERVICE_TYPE,
            name,
            "isolated.local.",
            address.ip().to_string(),
            address.port(),
            &[("profile", profile.as_str())][..],
        )
        .unwrap()
        .as_resolved_service(),
    ))
}
async fn until(mut predicate: impl FnMut() -> bool) {
    while !predicate() {
        future::yield_now().await;
    }
}
type Binding = (String, u32, u32);
#[derive(Default)]
struct PeerState {
    client_id: String,
    seen: Vec<(String, Value)>,
    online: BTreeMap<String, bool>,
    bad: BTreeSet<String>,
    omit: BTreeSet<String>,
    values: BTreeMap<Binding, bool>,
    sub_reject: BTreeSet<Binding>,
    sub_timeout: BTreeSet<Binding>,
    held_subs: Vec<Subscribe>,
    read_fail: BTreeSet<Binding>,
    read_timeout: BTreeSet<Binding>,
    list_error: bool,
    list_timeout: bool,
    hold_reads: bool,
    hold_writes: bool,
    held: Vec<(Publish, Value)>,
    send: Vec<Packet>,
    unsubscribes: Vec<String>,
}
#[derive(Default)]
struct Peer {
    state: RefCell<PeerState>,
    changed: Event,
}
impl Peer {
    fn notify(&self, did: &str, siid: u32, value: bool) {
        let client = self.state.borrow().client_id.clone();
        self.send(Packet::Publish(Publish::new(
            format!("{client}/appMsg/notify/iot/{did}/property/{siid}.1"),
            QoS::AtMostOnce,
            encode_mips(
                700,
                None,
                &json!({"did":did,"siid":siid,"piid":1,"value":value}),
            ),
        )));
    }
    fn list_changed(&self, dids: &[&str]) {
        let client = self.state.borrow().client_id.clone();
        self.send(Packet::Publish(Publish::new(
            format!("{client}/appMsg/devListChange"),
            QoS::AtMostOnce,
            encode_mips(701, None, &json!({"devList":dids})),
        )));
    }
    fn send(&self, packet: Packet) {
        self.state.borrow_mut().send.push(packet);
        self.changed.notify(usize::MAX);
    }
    async fn release(&self, did: &str, siid: u32, value: Value) {
        until(|| {
            self.state.borrow().held.iter().any(|(publish, _)| {
                let payload = decode_mips(&publish.payload).unwrap().payload;
                payload["did"] == did
                    && (payload["siid"] == siid || payload["rpc"]["params"][0]["siid"] == siid)
            })
        })
        .await;
        let index = self
            .state
            .borrow()
            .held
            .iter()
            .position(|(publish, _)| {
                let payload = decode_mips(&publish.payload).unwrap().payload;
                payload["did"] == did
                    && (payload["siid"] == siid || payload["rpc"]["params"][0]["siid"] == siid)
            })
            .unwrap();
        let (publish, _) = self.state.borrow_mut().held.remove(index);
        let client = self.state.borrow().client_id.clone();
        self.send(response(&client, &publish, value));
    }
    fn count(&self, operation: &str, did: &str, siid: u32) -> usize {
        self.state
            .borrow()
            .seen
            .iter()
            .filter(|(op, value)| {
                op == operation
                    && value["did"] == did
                    && (siid == 0
                        || value["siid"] == siid
                        || value["rpc"]["params"][0]["siid"] == siid)
            })
            .count()
    }
}
fn response(client: &str, publish: &Publish, value: Value) -> Packet {
    Packet::Publish(Publish::new(
        format!("{client}/reply"),
        QoS::AtMostOnce,
        encode_mips(decode_mips(&publish.payload).unwrap().id, None, &value),
    ))
}
fn key(value: &Value) -> Binding {
    (
        value["did"].as_str().unwrap().into(),
        value["siid"].as_u64().unwrap() as u32,
        value["piid"].as_u64().unwrap() as u32,
    )
}
fn topic_key(topic: &str) -> Binding {
    let parts = topic.split('/').collect::<Vec<_>>();
    let binding = parts.last().unwrap().split('.').collect::<Vec<_>>();
    (
        parts[parts.len() - 3].into(),
        binding[0].parse().unwrap(),
        binding[1].parse().unwrap(),
    )
}
async fn peer_run(mut broker: Broker, peer: Rc<Peer>, data: Vec<CachedPhysicalDevice>) {
    loop {
        let listener = peer.changed.listen();
        let queued = std::mem::take(&mut peer.state.borrow_mut().send);
        if !queued.is_empty() {
            broker.batch(queued).await;
            continue;
        }
        let packet = future::or(async { Some(broker.next().await) }, async {
            listener.await;
            None
        })
        .await;
        let Some(packet) = packet else {
            continue;
        };
        match packet {
            Packet::Subscribe(sub) => {
                let binding = topic_key(&sub.filters[0].path);
                let (timeout, rejected) = {
                    let mut state = peer.state.borrow_mut();
                    state.seen.push((
                        "subscribe".into(),
                        json!({"did":binding.0,"siid":binding.1,"piid":binding.2}),
                    ));
                    let timeout = state.sub_timeout.contains(&binding);
                    let rejected = state.sub_reject.contains(&binding);
                    if timeout {
                        state.held_subs.push(sub.clone());
                    }
                    (timeout, rejected)
                };
                if !timeout {
                    broker
                        .send(Packet::SubAck(SubAck::new(
                            sub.pkid,
                            vec![if rejected {
                                SubscribeReasonCode::Unspecified
                            } else {
                                SubscribeReasonCode::QoS2
                            }],
                        )))
                        .await;
                }
            }
            Packet::Unsubscribe(unsub) => {
                peer.state
                    .borrow_mut()
                    .unsubscribes
                    .extend(unsub.filters.clone());
                broker
                    .send(Packet::UnsubAck(UnsubAck {
                        pkid: unsub.pkid,
                        reasons: vec![UnsubAckReason::Success; unsub.filters.len()],
                        properties: None,
                    }))
                    .await;
            }
            Packet::Publish(publish) => {
                let payload = decode_mips(&publish.payload).unwrap().payload;
                let operation = publish.topic.rsplit('/').next().unwrap().to_owned();
                peer.state
                    .borrow_mut()
                    .seen
                    .push((operation.clone(), payload.clone()));
                broker.acknowledge(&publish).await;
                let (reply, hold, timeout) = {
                    let state = peer.state.borrow();
                    match operation.as_str() {
                        "getDevList" => {
                            assert!(payload.get("filter").is_none());
                            let mut list = serde_json::Map::new();
                            for record in &data {
                                let did = &record.device.description.key.id;
                                if state.omit.contains(did) {
                                    continue;
                                }
                                let mut info = json!({"name":format!("name {did}"),"model":record.model,"urn":record.instance_type,"online":state.online.get(did).copied().unwrap_or(true),"specV2Access":true,"pushAvailable":true});
                                if state.bad.contains(did) {
                                    info.as_object_mut().unwrap().remove("pushAvailable");
                                }
                                list.insert(did.clone(), info);
                            }
                            (
                                if state.list_error {
                                    json!({"invalid":list})
                                } else {
                                    json!({"devList":list})
                                },
                                false,
                                state.list_timeout,
                            )
                        }
                        "get" => {
                            let binding = key(&payload);
                            (
                                if state.read_fail.contains(&binding) {
                                    json!({"code":-7})
                                } else {
                                    json!({"value":state.values.get(&binding).copied().unwrap_or(false)})
                                },
                                state.hold_reads,
                                state.read_timeout.contains(&binding),
                            )
                        }
                        "rpcReq" => {
                            let property = &payload["rpc"]["params"][0];
                            (
                                json!({"result":[{"did":property["did"],"siid":property["siid"],"piid":property["piid"],"code":0}]}),
                                state.hold_writes,
                                false,
                            )
                        }
                        _ => panic!("unexpected operation {operation}"),
                    }
                };
                if hold || timeout {
                    peer.state.borrow_mut().held.push((publish, reply));
                } else {
                    broker
                        .send(response(&broker.client_id, &publish, reply))
                        .await;
                }
            }
            other => panic!("unexpected source packet {other:?}"),
        }
    }
}
fn function(registry: &DeviceRegistry, did: &str, id: &str) -> crate::device::Function {
    registry
        .get(&DeviceKey::new("xiaomi", did))
        .unwrap()
        .function(id)
        .unwrap()
}
async fn initialized(c: &Control, peer: &Peer, dids: &[&str]) {
    until(|| {
        dids.iter().all(|did| {
            c.context
                .registry
                .get(&DeviceKey::new("xiaomi", *did))
                .is_some_and(|physical| {
                    physical.availability() == Availability::Available
                        && physical
                            .functions()
                            .iter()
                            .all(|function| function.snapshot().power != Power::Unknown)
                        && c.context
                            .live
                            .borrow()
                            .session
                            .as_ref()
                            .and_then(|session| session.devices.get(*did))
                            .is_some_and(|work| {
                                work.subscribed.borrow().len()
                                    == physical.functions().len()
                                        + usize::from(
                                            work.cached.borrow().device.refresh_trigger.is_some(),
                                        )
                            })
                })
        }) && peer
            .state
            .borrow()
            .seen
            .iter()
            .filter(|(op, _)| op == "getDevList")
            .count()
            >= 1
    })
    .await;
}
#[test]
fn prepared_accounts_can_deliver_an_empty_registry() {
    block_on(Rig::new(false, &[]).run(|mut c| async move {
        let registry = c.ready.take().unwrap().wait().await.unwrap();
        assert!(registry.list().is_empty());
    }));
}
#[test]
fn multiple_devices_publish_before_initial_reads_and_unknown_online_commands_work() {
    block_on(
        Rig::new(false, &["one", "multi", "two"]).run(|c| async move {
            c.announce();
            let peer = Rc::new(Peer::default());
            peer.state.borrow_mut().hold_reads = true;
            let control = &c;
            c.peer(peer, |peer| async move {
                until(|| {
                    control.context.registry.list().len() == 3
                        && control
                            .context
                            .registry
                            .list()
                            .iter()
                            .all(|physical| physical.availability() == Availability::Available)
                })
                .await;
                let registry = &control.context.registry;
                assert_eq!(
                    registry
                        .get(&DeviceKey::new("xiaomi", "multi"))
                        .unwrap()
                        .functions()
                        .len(),
                    3
                );
                let one = function(registry, "one", "main");
                assert_eq!(one.snapshot().power, Power::Unknown);
                assert_eq!(
                    one.execute(Command::Toggle).await,
                    Err(crate::device::CommandError::UnknownState)
                );
                assert_eq!(one.execute(Command::On).await.unwrap().power, Power::On);
                peer.notify("two", 2, true);
                peer.notify("multi", 4, true);
                until(|| {
                    function(registry, "two", "main").snapshot().power == Power::On
                        && function(registry, "multi", "outlet_2").snapshot().power == Power::On
                })
                .await;
                assert_eq!(
                    function(registry, "multi", "outlet_1").snapshot().power,
                    Power::Unknown
                );
                assert_eq!(
                    function(registry, "multi", "usb").snapshot().power,
                    Power::Unknown
                );
            })
            .await;
        }),
    );
}

#[test]
fn cached_local_control_survives_temporary_cloud_detail_failures() {
    block_on(Rig::new(true, &["one"]).run(|c| async move {
        c.cloud.lock().unwrap().detail_status = 503;
        c.announce();
        let peer = Rc::new(Peer::default());
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| {
                control.context.live.borrow().loading.is_empty()
                    && matches!(
                        control
                            .context
                            .status
                            .borrow()
                            .authentication
                            .authentication,
                        crate::xiaomi::auth::AuthenticationState::Authenticated
                    )
                    && function(&control.context.registry, "one", "main")
                        .snapshot()
                        .power
                        != Power::Unknown
            })
            .await;
            assert_eq!(
                function(&control.context.registry, "one", "main")
                    .snapshot()
                    .availability,
                Availability::Available
            );
            function(&control.context.registry, "one", "main")
                .execute(Command::On)
                .await
                .unwrap();
            assert!(peer.count("rpcReq", "one", 2) > 0);
        })
        .await;
    }));
}

async fn advance_to(clock: &TestClock, deadline: Duration) {
    until(|| clock.deadlines.lock().unwrap().contains(&deadline)).await;
    clock.advance(deadline - clock.now());
}
fn accepted(did: &str, siid: u32, code: i32) -> Value {
    json!({"result":[{"did":did,"siid":siid,"piid":1,"code":code}]})
}
#[test]
fn a_bad_candidate_remains_pending_without_blocking_good_devices() {
    block_on(Rig::new(false, &["one", "multi"]).run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        peer.state.borrow_mut().bad.insert("multi".into());
        let control = &c;
        c.peer(peer, |peer| async move {
            initialized(control, &peer, &["one"]).await;
            until(|| {
                control
                    .context
                    .status
                    .borrow()
                    .pending
                    .iter()
                    .any(|pending| pending.did == "multi")
            })
            .await;
            assert_eq!(control.context.registry.list().len(), 1);
            assert!(
                control
                    .store
                    .devices()
                    .load_published(&DeviceKey::new("xiaomi", "multi"))
                    .unwrap()
                    .is_none()
            );
            peer.state.borrow_mut().bad.clear();
            peer.list_changed(&["multi"]);
            initialized(control, &peer, &["one", "multi"]).await;
        })
        .await;
    }));
}
#[test]
fn total_control_refresh_reads_mixed_values_without_inference_and_coalesces() {
    block_on(Rig::new(true, &["multi"]).run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        let control = &c;
        c.peer(peer, |peer| async move {
            initialized(control, &peer, &["multi"]).await;
            let registry = &control.context.registry;
            peer.notify("multi", 4, true);
            until(|| function(registry, "multi", "outlet_2").snapshot().power == Power::On).await;
            let before = peer.count("get", "multi", 3);
            peer.state.borrow_mut().hold_reads = true;
            peer.notify("multi", 2, true);
            until(|| {
                peer.count("get", "multi", 3) == before + 1
                    && peer
                        .state
                        .borrow()
                        .held
                        .iter()
                        .any(|(p, _)| decode_mips(&p.payload).unwrap().payload["siid"] == 3)
            })
            .await;
            assert_eq!(
                function(registry, "multi", "outlet_1").snapshot().power,
                Power::Off
            );
            assert_eq!(
                function(registry, "multi", "outlet_2").snapshot().power,
                Power::On
            );
            assert_eq!(
                function(registry, "multi", "usb").snapshot().power,
                Power::Off
            );
            peer.notify("multi", 2, false);
            peer.notify("multi", 2, true);
            peer.notify("multi", 2, false);
            peer.release("multi", 3, json!({"value":true})).await;
            until(|| {
                peer.state
                    .borrow()
                    .held
                    .iter()
                    .any(|(p, _)| decode_mips(&p.payload).unwrap().payload["siid"] == 4)
            })
            .await;
            peer.release("multi", 4, json!({"value":false})).await;
            until(|| {
                peer.state
                    .borrow()
                    .held
                    .iter()
                    .any(|(p, _)| decode_mips(&p.payload).unwrap().payload["siid"] == 9)
            })
            .await;
            peer.release("multi", 9, json!({"value":true})).await;
            until(|| {
                peer.count("get", "multi", 3) == before + 2
                    && peer
                        .state
                        .borrow()
                        .held
                        .iter()
                        .any(|(p, _)| decode_mips(&p.payload).unwrap().payload["siid"] == 3)
            })
            .await;
            peer.state.borrow_mut().hold_reads = false;
            peer.release("multi", 3, json!({"value":true})).await;
            until(|| {
                function(registry, "multi", "usb").snapshot().power == Power::Off
                    && peer.count("get", "multi", 9) == before + 2
            })
            .await;
            assert_eq!(
                function(registry, "multi", "outlet_1").snapshot().power,
                Power::On
            );
            assert_eq!(
                function(registry, "multi", "outlet_2").snapshot().power,
                Power::Off
            );
            assert_eq!(
                registry
                    .get(&DeviceKey::new("xiaomi", "multi"))
                    .unwrap()
                    .functions()
                    .len(),
                3
            );
        })
        .await;
    }));
}
#[test]
fn commands_are_concurrent_across_channels_and_fifo_with_execution_time_toggle() {
    block_on(Rig::new(true, &["one", "multi"]).run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        let control = &c;
        c.peer(peer, |peer| async move {
            initialized(control, &peer, &["one", "multi"]).await;
            peer.state.borrow_mut().hold_writes = true;
            let a = function(&control.context.registry, "one", "main");
            let b = function(&control.context.registry, "multi", "outlet_1");
            let d = function(&control.context.registry, "multi", "outlet_2");
            let mut first = Box::pin(a.execute(Command::On));
            let mut second = Box::pin(a.execute(Command::Toggle));
            let mut third = Box::pin(a.execute(Command::Off));
            let mut other = Box::pin(b.execute(Command::On));
            let mut channel = Box::pin(d.execute(Command::On));
            for command in [
                &mut first,
                &mut second,
                &mut third,
                &mut other,
                &mut channel,
            ] {
                assert!(poll_once(command.as_mut()).await.is_none());
            }
            until(|| {
                peer.count("rpcReq", "one", 2) == 1
                    && peer.count("rpcReq", "multi", 3) == 1
                    && peer.count("rpcReq", "multi", 4) == 1
            })
            .await;
            control.clock.advance(Duration::from_secs(6));
            assert!(poll_once(second.as_mut()).await.is_none());
            peer.release("multi", 4, accepted("multi", 4, 1)).await;
            assert_eq!(channel.await.unwrap().power, Power::On);
            peer.release("multi", 3, accepted("multi", 3, 0)).await;
            other.await.unwrap();
            peer.release("one", 2, accepted("one", 2, 0)).await;
            first.await.unwrap();
            until(|| peer.count("rpcReq", "one", 2) == 2).await;
            assert_eq!(
                peer.state
                    .borrow()
                    .seen
                    .iter()
                    .filter(|(op, value)| op == "rpcReq" && value["did"] == "one")
                    .nth(1)
                    .unwrap()
                    .1["rpc"]["params"][0]["value"],
                false
            );
            peer.release("one", 2, accepted("one", 2, 1)).await;
            second.await.unwrap();
            until(|| peer.count("rpcReq", "one", 2) == 3).await;
            peer.release("one", 2, accepted("one", 2, 0)).await;
            third.await.unwrap();
            assert_eq!(a.snapshot().power, Power::Off);
        })
        .await;
    }));
}
#[test]
fn an_offline_event_before_an_active_success_reply_preserves_acceptance() {
    block_on(Rig::new(true, &["one", "two"]).run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        let control = &c;
        c.peer(peer, |peer| async move {
            initialized(control, &peer, &["one", "two"]).await;
            peer.state.borrow_mut().hold_writes = true;
            let one = function(&control.context.registry, "one", "main");
            let mut command = Box::pin(one.execute(Command::On));
            assert!(poll_once(command.as_mut()).await.is_none());
            until(|| peer.count("rpcReq", "one", 2) == 1).await;
            peer.state.borrow_mut().online.insert("one".into(), false);
            peer.list_changed(&["one"]);
            until(|| matches!(one.snapshot().availability, Availability::Unavailable(_))).await;
            assert!(poll_once(command.as_mut()).await.is_none());
            peer.release("one", 2, accepted("one", 2, 0)).await;
            let accepted = command.await.unwrap();
            assert_eq!(accepted.power, Power::On);
            assert!(matches!(
                accepted.availability,
                Availability::Unavailable(_)
            ));
            assert_eq!(
                function(&control.context.registry, "two", "main")
                    .snapshot()
                    .availability,
                Availability::Available
            );
        })
        .await;
    }));
}
#[test]
fn subscription_and_read_retries_are_bounded_and_later_events_restore_them() {
    for timeout in [false, true] {
        block_on(Rig::new(true, &["one", "multi"]).run(|c| async move {
            c.announce();
            let peer = Rc::new(Peer::default());
            let sub = ("multi".into(), 9, 1);
            let read = ("one".into(), 2, 1);
            if timeout {
                peer.state.borrow_mut().sub_timeout.insert(sub.clone());
                peer.state.borrow_mut().read_timeout.insert(read.clone());
            } else {
                peer.state.borrow_mut().sub_reject.insert(sub.clone());
                peer.state.borrow_mut().read_fail.insert(read.clone());
            }
            let control = &c;
            c.peer(peer, |peer| async move {
                until(|| {
                    peer.count("subscribe", "multi", 9) == 1 && peer.count("get", "one", 2) == 1
                })
                .await;
                for attempt in 2..=4 {
                    let deadline = control.clock.now() + Duration::from_millis(100);
                    advance_to(&control.clock, deadline).await;
                    until(|| peer.count("subscribe", "multi", 9) == attempt).await;
                }
                // Read retries retain their own three-second schedule despite subscription attempts.
                for attempt in 2..=4 {
                    let deadline = Duration::from_secs((attempt as u64 - 1) * 3);
                    advance_to(&control.clock, deadline).await;
                    until(|| peer.count("get", "one", 2) == attempt).await;
                }
                until(|| {
                    control
                        .context
                        .status
                        .borrow()
                        .pending
                        .iter()
                        .any(|pending| pending.did == "one" && pending.reason.contains("read"))
                        && control
                            .context
                            .status
                            .borrow()
                            .pending
                            .iter()
                            .any(|pending| {
                                pending.did == "multi" && pending.reason.contains("subscribe")
                            })
                })
                .await;
                assert!(
                    control
                        .context
                        .status
                        .borrow()
                        .pending
                        .iter()
                        .any(|pending| pending.did == "one" && pending.reason.contains("read"))
                );
                assert!(
                    control
                        .context
                        .status
                        .borrow()
                        .pending
                        .iter()
                        .any(|pending| pending.did == "multi"
                            && pending.reason.contains("subscribe"))
                );
                assert_eq!(
                    function(&control.context.registry, "one", "main")
                        .snapshot()
                        .power,
                    Power::Unknown
                );
                assert_eq!(
                    function(&control.context.registry, "one", "main")
                        .snapshot()
                        .availability,
                    Availability::Available
                );
                control.clock.advance(Duration::from_secs(10));
                for _ in 0..10 {
                    future::yield_now().await;
                }
                assert_eq!(peer.count("get", "one", 2), 4);
                assert_eq!(peer.count("subscribe", "multi", 9), 4);
                assert_eq!(peer.count("subscribe", "multi", 3), 1);
                {
                    let mut state = peer.state.borrow_mut();
                    state.sub_reject.clear();
                    state.sub_timeout.clear();
                    state.read_fail.clear();
                    state.read_timeout.clear();
                    state.values.insert(read.clone(), true);
                }
                peer.list_changed(&["multi"]);
                until(|| {
                    control
                        .context
                        .live
                        .borrow()
                        .session
                        .as_ref()
                        .unwrap()
                        .devices["multi"]
                        .subscribed
                        .borrow()
                        .contains(&(9, 1))
                })
                .await;
                assert_eq!(peer.count("get", "one", 2), 4);
                peer.state.borrow_mut().online.insert("one".into(), false);
                peer.list_changed(&["one"]);
                until(|| {
                    matches!(
                        function(&control.context.registry, "one", "main")
                            .snapshot()
                            .availability,
                        Availability::Unavailable(_)
                    )
                })
                .await;
                peer.state.borrow_mut().online.insert("one".into(), true);
                peer.list_changed(&["one"]);
                until(|| {
                    function(&control.context.registry, "one", "main")
                        .snapshot()
                        .power
                        == Power::On
                })
                .await;
                assert_eq!(peer.count("get", "one", 2), 5);
            })
            .await;
        }));
    }
}
#[test]
fn five_minute_member_sync_does_not_poll_healthy_channel_power() {
    block_on(Rig::new(true, &["one", "multi"]).run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        let control = &c;
        c.peer(peer, |peer| async move {
            initialized(control, &peer, &["one", "multi"]).await;
            until(|| {
                control.context.live.borrow().loading.is_empty()
                    && control
                        .clock
                        .deadlines
                        .lock()
                        .unwrap()
                        .contains(&Duration::from_secs(300))
            })
            .await;
            let reads = peer
                .state
                .borrow()
                .seen
                .iter()
                .filter(|(op, _)| op == "get")
                .count();
            let homes = control.cloud.lock().unwrap().homes;
            let lists = peer
                .state
                .borrow()
                .seen
                .iter()
                .filter(|(op, _)| op == "getDevList")
                .count();
            advance_to(&control.clock, Duration::from_secs(300)).await;
            until(|| {
                control.cloud.lock().unwrap().homes > homes
                    && peer
                        .state
                        .borrow()
                        .seen
                        .iter()
                        .filter(|(op, _)| op == "getDevList")
                        .count()
                        > lists
            })
            .await;
            peer.notify("one", 2, true);
            until(|| {
                function(&control.context.registry, "one", "main")
                    .snapshot()
                    .power
                    == Power::On
                    && matches!(
                        control
                            .context
                            .live
                            .borrow()
                            .session
                            .as_ref()
                            .unwrap()
                            .access,
                        super::state::AccessRefresh::Idle
                    )
            })
            .await;
            for _ in 0..10 {
                future::yield_now().await;
            }
            assert_eq!(
                peer.state
                    .borrow()
                    .seen
                    .iter()
                    .filter(|(op, _)| op == "get")
                    .count(),
                reads
            );
        })
        .await;
    }));
}
#[test]
fn cached_build_dropped_before_run_releases_its_context_and_closes_commands() {
    let rig = Rig::new(true, &["one"]);
    let weak = Rc::downgrade(&rig.source.context);
    let function = function(&rig.source.context.registry, "one", "main");
    drop(rig);
    assert!(weak.upgrade().is_none());
    assert_eq!(
        block_on(function.execute(Command::On)),
        Err(crate::device::CommandError::Cancelled)
    );
}
#[test]
fn an_empty_bridge_adds_removes_and_readds_with_stable_identity_and_name() {
    block_on(Rig::new(false, &[]).run(|mut c| async move {
        let registry = c.ready.take().unwrap().wait().await.unwrap();
        assert!(registry.list().is_empty());
        c.announce();
        let peer = Rc::new(Peer::default());
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| control.context.live.borrow().scope.is_some()).await;
            control.cloud.lock().unwrap().members = vec!["multi".into()];
            peer.list_changed(&["multi"]);
            initialized(control, &peer, &["multi"]).await;
            let key = DeviceKey::new("xiaomi", "multi");
            control
                .store
                .devices()
                .set_device_label(&key, "Kitchen saved")
                .unwrap();
            let before = control
                .store
                .devices()
                .load_published(&key)
                .unwrap()
                .unwrap();
            let old = function(&registry, "multi", "outlet_1");
            control.cloud.lock().unwrap().members.clear();
            peer.list_changed(&["multi"]);
            until(|| registry.list().is_empty() && peer.state.borrow().unsubscribes.len() == 4)
                .await;
            assert!(
                control
                    .store
                    .devices()
                    .load_published(&key)
                    .unwrap()
                    .is_none()
            );
            assert_eq!(
                old.execute(Command::On).await,
                Err(crate::device::CommandError::Cancelled)
            );
            peer.notify("multi", 3, true);
            control.cloud.lock().unwrap().members = vec!["multi".into()];
            peer.list_changed(&["multi"]);
            initialized(control, &peer, &["multi"]).await;
            let after = control
                .store
                .devices()
                .load_published(&key)
                .unwrap()
                .unwrap();
            assert_eq!(after.unique_id, before.unique_id);
            assert_eq!(after.endpoint, before.endpoint);
            assert_eq!(after.functions, before.functions);
            assert_eq!(after.label, "Kitchen saved");
            assert_eq!(
                function(&registry, "multi", "outlet_1").snapshot().power,
                Power::Off
            );
        })
        .await;
    }));
}
#[test]
fn incomplete_foreign_or_failed_cloud_sync_cannot_remove_published_devices() {
    for failure in 0..4 {
        block_on(Rig::new(true, &["one"]).run(|c| async move {
            c.announce();
            let peer = Rc::new(Peer::default());
            let control = &c;
            c.peer(peer, |peer| async move {
                initialized(control, &peer, &["one"]).await;
                {
                    let mut cloud = control.cloud.lock().unwrap();
                    cloud.members.clear();
                    match failure {
                        0 => cloud.partial = true,
                        1 => cloud.foreign = true,
                        2 => cloud.status = 503,
                        _ => cloud.members = vec!["one".into()],
                    };
                }
                if failure == 3 {
                    let mut scope = control.context.live.borrow().scope.clone().unwrap();
                    scope.home_id = "missing".into();
                    control.context.live.borrow_mut().scope = Some(scope);
                }
                peer.list_changed(&["one"]);
                until(|| {
                    control.context.status.borrow().catalog_problem.is_some()
                        || matches!(
                            control
                                .context
                                .status
                                .borrow()
                                .authentication
                                .authentication,
                            crate::xiaomi::auth::AuthenticationState::Unavailable(_)
                        )
                })
                .await;
                assert!(
                    control
                        .context
                        .registry
                        .get(&DeviceKey::new("xiaomi", "one"))
                        .is_some()
                );
                assert!(
                    control
                        .store
                        .devices()
                        .load_published(&DeviceKey::new("xiaomi", "one"))
                        .unwrap()
                        .is_some()
                );
                assert_eq!(
                    function(&control.context.registry, "one", "main")
                        .snapshot()
                        .availability,
                    Availability::Available
                );
                assert!(peer.state.borrow().unsubscribes.is_empty());
            })
            .await;
        }));
    }
}
#[test]
fn cached_local_access_recovers_while_cloud_member_sync_is_stalled() {
    let rig = Rig::new(true, &["one"]);
    let (_release, gate) = flume::bounded(1);
    rig.cloud.lock().unwrap().home_gate = Some(gate);
    block_on(rig.run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        let control = &c;
        c.peer(peer, |peer| async move {
            initialized(control, &peer, &["one"]).await;
            until(|| control.cloud.lock().unwrap().homes == 1).await;
            assert_eq!(control.cloud.lock().unwrap().homes, 1);
            assert_eq!(
                function(&control.context.registry, "one", "main")
                    .snapshot()
                    .availability,
                Availability::Available
            );
            function(&control.context.registry, "one", "main")
                .execute(Command::On)
                .await
                .unwrap();
            assert_eq!(peer.count("rpcReq", "one", 2), 1);
        })
        .await;
    }));
}
#[test]
fn discovery_timeout_keeps_its_reason_and_diagnostic_wakes_do_not_skip_retry() {
    block_on(Rig::new(true,&[]).run(|c|async move {
    advance_to(&c.clock,Duration::from_secs(30)).await;until(||matches!(c.context.status.borrow().gateway,GatewayStatus::Unavailable(_))).await;
    c.context.set_catalog_problem(Some("cloud diagnostic".into()));c.context.changed.notify(usize::MAX);for _ in 0..10 {future::yield_now().await;}
    assert!(matches!(c.context.status.borrow().gateway,GatewayStatus::Unavailable(ref reason) if reason.contains("gateway")));
    let mut changes=c.discovery.subscribe();let mut retry=Box::pin(super::local::wait_retry(&c.context,&c.discovery,&mut changes,None,Duration::from_secs(5)));assert!(poll_once(retry.as_mut()).await.is_none());
    c.context.changed.notify(usize::MAX);assert!(poll_once(retry.as_mut()).await.is_none());c.clock.advance(Duration::from_secs(5));retry.await.unwrap();
}));
}
#[test]
fn removed_pending_reads_subscriptions_and_commands_cannot_revive_a_device() {
    block_on(Rig::new(true, &["one", "multi"]).run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        peer.state
            .borrow_mut()
            .read_timeout
            .insert(("one".into(), 2, 1));
        peer.state
            .borrow_mut()
            .sub_timeout
            .insert(("one".into(), 2, 1));
        let control = &c;
        c.peer(peer, |peer| async move {
            initialized(control, &peer, &["multi"]).await;
            until(|| peer.count("get", "one", 2) == 1 && peer.count("subscribe", "one", 2) == 1)
                .await;
            let one = function(&control.context.registry, "one", "main");
            peer.state.borrow_mut().hold_writes = true;
            let mut command = Box::pin(one.execute(Command::On));
            assert!(poll_once(command.as_mut()).await.is_none());
            until(|| peer.count("rpcReq", "one", 2) == 1).await;
            control.cloud.lock().unwrap().members = vec!["multi".into()];
            peer.list_changed(&["one"]);
            until(|| {
                control
                    .context
                    .registry
                    .get(&DeviceKey::new("xiaomi", "one"))
                    .is_none()
                    && !peer.state.borrow().unsubscribes.is_empty()
            })
            .await;
            assert_eq!(command.await, Err(crate::device::CommandError::Cancelled));
            let subs = std::mem::take(&mut peer.state.borrow_mut().held_subs);
            for sub in subs {
                peer.send(Packet::SubAck(SubAck::new(
                    sub.pkid,
                    vec![SubscribeReasonCode::QoS2],
                )));
            }
            peer.release("one", 2, json!({"value":true})).await;
            peer.release("one", 2, accepted("one", 2, 0)).await;
            peer.notify("one", 2, true);
            peer.notify("multi", 4, true);
            until(|| {
                function(&control.context.registry, "multi", "outlet_2")
                    .snapshot()
                    .power
                    == Power::On
            })
            .await;
            control.clock.advance(Duration::from_secs(10));
            for _ in 0..10 {
                future::yield_now().await;
            }
            assert!(
                control
                    .context
                    .registry
                    .get(&DeviceKey::new("xiaomi", "one"))
                    .is_none()
            );
            assert_eq!(peer.count("get", "one", 2), 1);
            assert_eq!(peer.count("subscribe", "one", 2), 1);
        })
        .await;
    }));
}
#[test]
fn whole_gateway_list_failures_preserve_confirmed_access_and_availability() {
    for timeout in [false, true] {
        block_on(Rig::new(true, &["one", "multi"]).run(|c| async move {
            c.announce();
            let peer = Rc::new(Peer::default());
            let control = &c;
            c.peer(peer, |peer| async move {
                initialized(control, &peer, &["one", "multi"]).await;
                if timeout {
                    peer.state.borrow_mut().list_timeout = true;
                } else {
                    peer.state.borrow_mut().list_error = true;
                }
                peer.list_changed(&["one"]);
                until(|| control.context.status.borrow().catalog_problem.is_some()).await;
                peer.notify("one", 2, true);
                until(|| {
                    function(&control.context.registry, "one", "main")
                        .snapshot()
                        .power
                        == Power::On
                })
                .await;
                for physical in control.context.registry.list() {
                    assert_eq!(physical.availability(), Availability::Available);
                }
                function(&control.context.registry, "multi", "usb")
                    .execute(Command::On)
                    .await
                    .unwrap();
            })
            .await;
        }));
    }
}
#[test]
fn pending_cloud_descriptions_retry_on_a_later_event_without_blocking_cached_control() {
    let rig = Rig::new(true, &["one"]);
    rig.cloud.lock().unwrap().members = vec!["one".into(), "multi".into()];
    rig.cloud.lock().unwrap().detail_status = 503;
    block_on(rig.run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        let control = &c;
        c.peer(peer, |peer| async move {
            initialized(control, &peer, &["one"]).await;
            until(|| {
                control
                    .context
                    .status
                    .borrow()
                    .pending
                    .iter()
                    .any(|pending| pending.did == "multi" && pending.reason.contains("503"))
            })
            .await;
            function(&control.context.registry, "one", "main")
                .execute(Command::On)
                .await
                .unwrap();
            assert_eq!(control.context.registry.list().len(), 1);
            assert_eq!(control.cloud.lock().unwrap().details, 1);
            control.cloud.lock().unwrap().detail_status = 200;
            peer.list_changed(&["multi"]);
            initialized(control, &peer, &["one", "multi"]).await;
            assert!(
                control
                    .context
                    .status
                    .borrow()
                    .pending
                    .iter()
                    .all(|pending| pending.did != "multi")
            );
        })
        .await;
    }));
}
#[test]
fn compatible_binding_updates_keep_identity_ignore_old_reads_and_use_the_new_command_binding() {
    block_on(Rig::new(true, &["one"]).run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        peer.state
            .borrow_mut()
            .read_timeout
            .insert(("one".into(), 2, 1));
        peer.state
            .borrow_mut()
            .sub_timeout
            .insert(("one".into(), 2, 1));
        peer.state
            .borrow_mut()
            .values
            .insert(("one".into(), 7, 1), true);
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| peer.count("get", "one", 2) == 1 && peer.count("subscribe", "one", 2) == 1)
                .await;
            let key = DeviceKey::new("xiaomi", "one");
            let before = control
                .store
                .devices()
                .load_published(&key)
                .unwrap()
                .unwrap();
            let mut changed = control.context.live.borrow().records["one"].cached.clone();
            changed.device.functions[0].power.siid = 7;
            control
                .context
                .live
                .borrow_mut()
                .descriptions
                .insert("one".into(), changed);
            control.context.reconcile("one");
            until(|| {
                peer.count("subscribe", "one", 7) == 1
                    && function(&control.context.registry, "one", "main")
                        .snapshot()
                        .power
                        == Power::On
            })
            .await;
            let old = std::mem::take(&mut peer.state.borrow_mut().held_subs);
            for sub in old {
                peer.send(Packet::SubAck(SubAck::new(
                    sub.pkid,
                    vec![SubscribeReasonCode::QoS2],
                )));
            }
            peer.release("one", 2, json!({"value":false})).await;
            function(&control.context.registry, "one", "main")
                .execute(Command::On)
                .await
                .unwrap();
            assert_eq!(peer.count("rpcReq", "one", 7), 1);
            assert_eq!(
                function(&control.context.registry, "one", "main")
                    .snapshot()
                    .power,
                Power::On
            );
            assert_eq!(
                control
                    .store
                    .devices()
                    .load_published(&key)
                    .unwrap()
                    .unwrap(),
                before
            );
            assert_eq!(
                control
                    .store
                    .devices()
                    .load_parsed(&control.data[0].uid, "local", &key)
                    .unwrap()
                    .unwrap()
                    .device
                    .functions[0]
                    .power
                    .siid,
                7
            );
        })
        .await;
    }));
}
#[test]
fn incompatible_topology_stays_published_and_unavailable_until_a_compatible_description_returns() {
    block_on(Rig::new(true, &["one", "multi"]).run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        let control = &c;
        c.peer(peer, |peer| async move {
            initialized(control, &peer, &["one", "multi"]).await;
            let original = control.context.live.borrow().records["multi"]
                .cached
                .clone();
            let mut incompatible = original.clone();
            incompatible.device.functions.pop();
            control
                .context
                .live
                .borrow_mut()
                .descriptions
                .insert("multi".into(), incompatible);
            control.context.reconcile("multi");
            let physical = control
                .context
                .registry
                .get(&DeviceKey::new("xiaomi", "multi"))
                .unwrap();
            assert_eq!(physical.functions().len(), 3);
            assert!(matches!(
                physical.availability(),
                Availability::Unavailable(_)
            ));
            assert!(
                control
                    .context
                    .status
                    .borrow()
                    .pending
                    .iter()
                    .any(
                        |pending| pending.did == "multi" && pending.reason.contains("incompatible")
                    )
            );
            assert_eq!(
                control
                    .store
                    .devices()
                    .load_parsed(&original.uid, "local", &physical.description().key)
                    .unwrap()
                    .unwrap(),
                original
            );
            function(&control.context.registry, "one", "main")
                .execute(Command::On)
                .await
                .unwrap();
            control
                .context
                .live
                .borrow_mut()
                .records
                .get_mut("multi")
                .unwrap()
                .blocked = None;
            control
                .context
                .live
                .borrow_mut()
                .descriptions
                .insert("multi".into(), original);
            control.context.reconcile("multi");
            initialized(control, &peer, &["one", "multi"]).await;
        })
        .await;
    }));
}
#[test]
fn endpoint_exhaustion_is_pending_and_does_not_stop_cached_devices() {
    let rig = Rig::new(true, &["one"]);
    rig.cloud.lock().unwrap().members = vec!["one".into(), "multi".into()];
    rusqlite::Connection::open(rig.store.path())
        .unwrap()
        .execute(
            "INSERT INTO endpoint_allocation VALUES (65533,'test','history','main')",
            [],
        )
        .unwrap();
    block_on(rig.run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        let control = &c;
        c.peer(peer, |peer| async move {
            initialized(control, &peer, &["one"]).await;
            until(|| {
                control
                    .context
                    .status
                    .borrow()
                    .pending
                    .iter()
                    .any(|pending| pending.did == "multi" && pending.reason.contains("exhausted"))
            })
            .await;
            assert_eq!(control.context.registry.list().len(), 1);
            assert!(
                control
                    .store
                    .devices()
                    .load_published(&DeviceKey::new("xiaomi", "multi"))
                    .unwrap()
                    .is_none()
            );
            function(&control.context.registry, "one", "main")
                .execute(Command::On)
                .await
                .unwrap();
        })
        .await;
    }));
}
#[test]
fn publication_database_failures_are_fatal_and_do_not_publish_partial_topology() {
    let rig = Rig::new(true, &["one"]);
    let store = rig.store.clone();
    rig.cloud.lock().unwrap().members = vec!["one".into(), "multi".into()];
    rusqlite::Connection::open(store.path()).unwrap().execute_batch("CREATE TRIGGER fail_multi BEFORE INSERT ON device_function WHEN NEW.device_id='multi' BEGIN SELECT RAISE(ABORT,'publication failed'); END;").unwrap();
    let error = block_on(rig.run_result(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        c.peer(peer, |_| future::pending()).await;
    }))
    .unwrap_err();
    assert!(matches!(error, SourceError::Storage(_)));
    assert!(
        store
            .devices()
            .load_published(&DeviceKey::new("xiaomi", "multi"))
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .devices()
            .load_parsed(
                &store.xiaomi().load().unwrap().unwrap().uid,
                "local",
                &DeviceKey::new("xiaomi", "multi")
            )
            .unwrap()
            .is_none()
    );
}
#[test]
fn temporary_certificate_renewal_failures_retry_without_interrupting_local_control() {
    let rig = Rig::new(true, &["one"]);
    rig.cloud.lock().unwrap().cert_status = 503;
    block_on(rig.run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        let control = &c;
        c.peer(peer, |peer| async move {
            initialized(control, &peer, &["one"]).await;
            until(|| {
                matches!(
                    control
                        .context
                        .status
                        .borrow()
                        .authentication
                        .certificate_update,
                    crate::xiaomi::auth::CertificateUpdate::Failed(_)
                )
            })
            .await;
            let before = control.cloud.lock().unwrap().certs;
            function(&control.context.registry, "one", "main")
                .execute(Command::On)
                .await
                .unwrap();
            control.cloud.lock().unwrap().cert_status = 200;
            advance_to(&control.clock, Duration::from_secs(1)).await;
            until(|| {
                control.cloud.lock().unwrap().certs > before
                    && matches!(
                        control
                            .context
                            .status
                            .borrow()
                            .authentication
                            .certificate_update,
                        crate::xiaomi::auth::CertificateUpdate::Updated
                    )
            })
            .await;
            assert_eq!(
                function(&control.context.registry, "one", "main")
                    .snapshot()
                    .availability,
                Availability::Available
            );
        })
        .await;
    }));
}
#[test]
fn explicit_signin_failure_pauses_cloud_but_preserves_cached_local_commands() {
    let rig = Rig::new(true, &["one"]);
    rig.cloud.lock().unwrap().status = 401;
    block_on(rig.run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        let control = &c;
        c.peer(peer, |peer| async move {
            initialized(control, &peer, &["one"]).await;
            until(|| control.context.cloud_paused.get()).await;
            let before = control.cloud.lock().unwrap().homes;
            control.cloud.lock().unwrap().status = 200;
            control.clock.advance(Duration::from_secs(600));
            peer.list_changed(&["one"]);
            function(&control.context.registry, "one", "main")
                .execute(Command::On)
                .await
                .unwrap();
            for _ in 0..10 {
                future::yield_now().await;
            }
            assert_eq!(control.cloud.lock().unwrap().homes, before);
            assert!(matches!(
                control
                    .context
                    .status
                    .borrow()
                    .authentication
                    .authentication,
                crate::xiaomi::auth::AuthenticationState::SignInRequired(_)
            ));
        })
        .await;
    }));
}
#[test]
fn candidate_unauthorized_requests_coalesce_into_one_token_refresh_and_resume_preparation() {
    let rig = Rig::new(true, &["one"]);
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.members = vec!["one".into(), "multi".into(), "two".into()];
        cloud.detail_status = 401;
        cloud.token_delay = Duration::from_millis(100);
    }
    block_on(rig.run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        let control = &c;
        c.peer(peer, |peer| async move {
            initialized(control, &peer, &["one"]).await;
            until(|| control.cloud.lock().unwrap().tokens == 1).await;
            control.cloud.lock().unwrap().detail_status = 200;
            initialized(control, &peer, &["one", "multi", "two"]).await;
            assert_eq!(control.cloud.lock().unwrap().tokens, 1);
            assert!(!control.context.cloud_paused.get());
            assert_eq!(
                control
                    .store
                    .xiaomi()
                    .load()
                    .unwrap()
                    .unwrap()
                    .tokens
                    .access_token,
                "fresh-access"
            );
            function(&control.context.registry, "one", "main")
                .execute(Command::On)
                .await
                .unwrap();
        })
        .await;
    }));
}
#[test]
fn a_late_success_for_an_obsolete_write_binding_does_not_override_the_current_value() {
    block_on(Rig::new(true, &["one"]).run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        let control = &c;
        c.peer(peer, |peer| async move {
            initialized(control, &peer, &["one"]).await;
            peer.state.borrow_mut().hold_writes = true;
            let one = function(&control.context.registry, "one", "main");
            let mut command = Box::pin(one.execute(Command::On));
            assert!(poll_once(command.as_mut()).await.is_none());
            until(|| peer.count("rpcReq", "one", 2) == 1).await;
            let mut changed = control.context.live.borrow().records["one"].cached.clone();
            changed.device.functions[0].power.siid = 7;
            control
                .context
                .live
                .borrow_mut()
                .descriptions
                .insert("one".into(), changed);
            control.context.reconcile("one");
            until(|| peer.count("get", "one", 7) == 1).await;
            peer.release("one", 2, accepted("one", 2, 0)).await;
            assert_eq!(command.await, Err(crate::device::CommandError::Cancelled));
            assert_eq!(one.snapshot().power, Power::Off);
            assert_eq!(one.snapshot().availability, Availability::Available);
        })
        .await;
    }));
}
#[test]
fn stale_candidate_401s_before_or_after_a_refresh_cannot_pause_fresh_credentials() {
    for late in [false, true] {
        let rig = Rig::new(true, &["one"]);
        let (detail_release, detail_gate) = flume::bounded(1);
        let (token_release, token_gate) = flume::bounded(1);
        {
            let mut cloud = rig.cloud.lock().unwrap();
            cloud.members = vec!["one".into(), "multi".into()];
            cloud.detail_status = 401;
            cloud.detail_gate = Some(detail_gate);
            if !late {
                cloud.token_gate = Some(token_gate);
            }
        }
        block_on(rig.run(|c| async move {
            c.announce();
            let peer = Rc::new(Peer::default());
            let control = &c;
            c.peer(peer, |peer| async move {
                initialized(control, &peer, &["one"]).await;
                until(|| control.cloud.lock().unwrap().details > 0).await;
                let mut record = control.store.xiaomi().load().unwrap().unwrap();
                record.tokens.refresh_at = time::OffsetDateTime::now_utc().unix_timestamp() - 1;
                control.store.xiaomi().replace(&record).unwrap();
                control.context.request_sync();
                until(|| control.cloud.lock().unwrap().tokens == 1).await;
                {
                    let mut cloud = control.cloud.lock().unwrap();
                    cloud.detail_status = 200;
                    cloud.detail_gate = None;
                }
                if late {
                    until(|| {
                        control
                            .store
                            .xiaomi()
                            .load()
                            .unwrap()
                            .unwrap()
                            .tokens
                            .access_token
                            == "fresh-access"
                    })
                    .await;
                    assert_eq!(control.cloud.lock().unwrap().detail_done, 0);
                    detail_release.send(()).unwrap();
                } else {
                    detail_release.send(()).unwrap();
                    until(|| control.context.rejected_token.borrow().is_some()).await;
                    assert_eq!(control.cloud.lock().unwrap().token_done, 0);
                    assert_eq!(
                        control
                            .store
                            .xiaomi()
                            .load()
                            .unwrap()
                            .unwrap()
                            .tokens
                            .access_token,
                        "unused"
                    );
                    token_release.send(()).unwrap();
                }
                until(|| {
                    control
                        .store
                        .xiaomi()
                        .load()
                        .unwrap()
                        .unwrap()
                        .tokens
                        .access_token
                        == "fresh-access"
                        && control.context.live.borrow().loading.is_empty()
                })
                .await;
                assert_eq!(control.cloud.lock().unwrap().detail_done, 1);
                control.context.request_sync();
                initialized(control, &peer, &["one", "multi"]).await;
                assert_eq!(control.cloud.lock().unwrap().tokens, 1);
                assert!(!control.context.cloud_paused.get());
                assert!(matches!(
                    control
                        .context
                        .status
                        .borrow()
                        .authentication
                        .authentication,
                    crate::xiaomi::auth::AuthenticationState::Authenticated
                ));
            })
            .await;
        }));
    }
}
#[test]
fn a_failed_refresh_or_a_second_unauthorized_token_requires_signin_without_stopping_local_access() {
    for refresh_rejected in 0..3 {
        let rig = Rig::new(true, &["one"]);
        {
            let mut cloud = rig.cloud.lock().unwrap();
            cloud.members = vec!["one".into(), "multi".into()];
            cloud.detail_status = 401;
            if refresh_rejected == 1 {
                cloud.token_status = 401;
            } else if refresh_rejected == 2 {
                cloud.token_access = Some("unused".into());
            }
        }
        block_on(rig.run(|c| async move {
            c.announce();
            let peer = Rc::new(Peer::default());
            let control = &c;
            c.peer(peer, |peer| async move {
                initialized(control, &peer, &["one"]).await;
                until(|| control.context.cloud_paused.get()).await;
                assert_eq!(control.cloud.lock().unwrap().tokens, 1);
                control.clock.advance(Duration::from_secs(600));
                control.context.request_sync();
                function(&control.context.registry, "one", "main")
                    .execute(Command::On)
                    .await
                    .unwrap();
                for _ in 0..10 {
                    future::yield_now().await;
                }
                assert_eq!(control.cloud.lock().unwrap().tokens, 1);
                assert!(matches!(
                    control
                        .context
                        .status
                        .borrow()
                        .authentication
                        .authentication,
                    crate::xiaomi::auth::AuthenticationState::SignInRequired(_)
                ));
            })
            .await;
        }));
    }
}
#[test]
fn reconnect_keeps_command_consumers_cancels_old_writes_and_rebuilds_only_registered_devices() {
    block_on(Rig::new(true, &["one", "multi"]).run(|c| async move {
        c.announce();
        let one = function(&c.context.registry, "one", "main");
        let mut command = Box::pin(one.execute(Command::On));
        let peer = Rc::new(Peer::default());
        let control = &c;
        let pending = &mut command;
        c.peer(peer, |peer| async move {
            initialized(control, &peer, &["one", "multi"]).await;
            peer.state.borrow_mut().hold_writes = true;
            assert!(poll_once(pending.as_mut()).await.is_none());
            until(|| {
                peer.state.borrow().held.iter().any(|(publish, _)| {
                    decode_mips(&publish.payload).unwrap().payload["rpc"].is_object()
                })
            })
            .await;
        })
        .await;
        assert!(command.await.is_err());
        until(|| {
            matches!(
                c.context.status.borrow().gateway,
                GatewayStatus::Unavailable(_)
            )
        })
        .await;
        c.cloud.lock().unwrap().members = vec!["multi".into()];
        c.context.request_sync();
        until(|| {
            c.context
                .registry
                .get(&DeviceKey::new("xiaomi", "one"))
                .is_none()
        })
        .await;
        assert_eq!(
            one.execute(Command::Off).await,
            Err(crate::device::CommandError::Cancelled)
        );
        advance_to(&c.clock, Duration::from_secs(1)).await;
        let next = Rc::new(Peer::default());
        c.peer(next, |peer| async move {
            initialized(control, &peer, &["multi"]).await;
            assert_eq!(peer.count("subscribe", "one", 2), 0);
            assert_eq!(peer.count("rpcReq", "one", 2), 0);
            let usb = function(&control.context.registry, "multi", "usb");
            usb.execute(Command::On).await.unwrap();
            usb.execute(Command::Toggle).await.unwrap();
            assert_eq!(usb.snapshot().power, Power::Off);
            assert_eq!(peer.count("rpcReq", "multi", 9), 2);
        })
        .await;
    }));
}
#[test]
fn corrupt_saved_credentials_or_cached_topology_fail_before_network_requests() {
    for corrupt_credentials in [false, true] {
        let rig = Rig::new(true, &["one"]);
        let database = rusqlite::Connection::open(rig.store.path()).unwrap();
        if corrupt_credentials {
            database
                .execute("UPDATE xiaomi_auth SET certificate_pem='broken'", [])
                .unwrap();
        } else {
            database
                .execute("UPDATE device_cache SET functions='broken'", [])
                .unwrap();
        }
        let result = XiaomiSource::build(
            rig.store.clone(),
            rig.source.cloud.clone(),
            rig.discovery.clone(),
            rig.clock.clone(),
        );
        assert!(matches!(result, Err(SourceError::Storage(_))));
        assert_eq!(rig.cloud.lock().unwrap().homes, 0);
    }
}
#[test]
fn coalesced_candidate_401s_preserve_the_temporary_refresh_backoff() {
    let rig = Rig::new(true, &["one"]);
    let (release, gate) = flume::bounded(1);
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.members = vec!["one".into(), "multi".into(), "two".into()];
        cloud.detail_status = 401;
        cloud.token_status = 503;
        cloud.token_gate = Some(gate);
    }
    block_on(rig.run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        let control = &c;
        c.peer(peer, |peer| async move {
            initialized(control, &peer, &["one"]).await;
            until(|| {
                control.cloud.lock().unwrap().tokens == 1
                    && control.context.live.borrow().loading.is_empty()
            })
            .await;
            assert_eq!(control.cloud.lock().unwrap().details, 2);
            release.send(()).unwrap();
            until(|| {
                matches!(
                    control
                        .context
                        .status
                        .borrow()
                        .authentication
                        .authentication,
                    crate::xiaomi::auth::AuthenticationState::Unavailable(_)
                )
            })
            .await;
            {
                let mut cloud = control.cloud.lock().unwrap();
                cloud.token_status = 200;
                cloud.token_gate = None;
                cloud.detail_status = 200;
            }
            for _ in 0..10 {
                future::yield_now().await;
            }
            assert_eq!(control.cloud.lock().unwrap().tokens, 1);
            assert!(!control.context.cloud_paused.get());
            function(&control.context.registry, "one", "main")
                .execute(Command::On)
                .await
                .unwrap();
            advance_to(&control.clock, Duration::from_secs(1)).await;
            initialized(control, &peer, &["one", "multi", "two"]).await;
            assert_eq!(control.cloud.lock().unwrap().tokens, 2);
        })
        .await;
    }));
}
