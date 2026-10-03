use super::*;
use crate::{
    device::{Availability, DeviceKey, Power},
    storage::{CachedDevice, Store},
    xiaomi::{
        capability::parse_outlet,
        central::test_support::TlsFixture,
        cloud::group_id,
        discovery::Discovery,
        test_support::{MockResponse, mock_server},
    },
};
use futures_lite::future::{block_on, poll_once};
use serde_json::Value;
use std::time::Duration;

fn cached(uid: &str) -> CachedDevice {
    let instance: Value =
        serde_json::from_str(include_str!("../../../tests/fixtures/miot/outlet.json")).unwrap();
    CachedDevice {
        uid: uid.into(),
        home_id: "local".into(),
        group_id: group_id(uid, "local"),
        model: "vendor.plug".into(),
        instance_type: instance["type"].as_str().unwrap().into(),
        outlet: parse_outlet(DeviceKey::new("xiaomi", "target"), &instance).unwrap(),
    }
}

#[test]
fn cached_topology_is_ready_before_network_and_unknown_unreachable() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let tls = TlsFixture::new(false);
    store.xiaomi().replace(&tls.credentials).unwrap();
    let metadata = cached(&tls.credentials.uid);
    store.devices().save(&metadata).unwrap();
    let (base, requests) = mock_server(vec![MockResponse::json(503, "{}")]);
    let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    let (_events, receiver) = flume::unbounded();
    let (discovery, _driver) = Discovery::with_events(receiver);
    let (source, ready) =
        XiaomiSource::new(store, cloud, discovery, DeviceKey::new("xiaomi", "target")).unwrap();
    let prepared = block_on(poll_once(ready.wait()))
        .expect("cache must not wait for networking")
        .unwrap();
    assert_eq!(prepared.metadata, metadata);
    assert_eq!(prepared.device.snapshot().power, Power::Unknown);
    assert!(!matches!(
        prepared.device.snapshot().availability,
        Availability::Available
    ));
    assert_eq!(requests.try_iter().count(), 0);
    drop(source);
}

use crate::{
    device::Command,
    xiaomi::{
        central::{encode_mips, test_support::Broker},
        discovery::SERVICE_TYPE,
        test_support::dynamic_mock_server,
    },
};
use async_io::Async;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures_rustls::TlsAcceptor;
use mqttbytes::{QoS, v5::*};
use serde_json::json;
use std::{
    net::{SocketAddr, TcpListener},
    sync::Arc,
    time::Instant,
};

fn announcement(address: SocketAddr, uid: &str, name: &str, hub: u64) -> mdns_sd::ServiceEvent {
    home_announcement(address, uid, "local", name, hub)
}
fn home_announcement(
    address: SocketAddr,
    uid: &str,
    home: &str,
    name: &str,
    hub: u64,
) -> mdns_sd::ServiceEvent {
    let mut profile = [0; 23];
    profile[1..9].copy_from_slice(&hub.to_be_bytes());
    profile[9..17].copy_from_slice(
        &u64::from_str_radix(&group_id(uid, home), 16)
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

#[derive(Default)]
struct TestClock {
    elapsed: std::sync::Mutex<Duration>,
    deadlines: std::sync::Mutex<Vec<Duration>>,
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

#[derive(Clone, Copy)]
enum CloudCase {
    Normal,
    Failure,
    RefreshRejected,
    Missing,
    Moved,
    Unsupported,
    DetailUnauthorized,
    ForeignAccount,
    Slow,
}
struct Rig {
    _directory: tempfile::TempDir,
    store: Store,
    source: XiaomiSource,
    ready: TopologyReady,
    discovery: Discovery,
    discovery_driver: super::super::discovery::DiscoveryDriver,
    events: flume::Sender<mdns_sd::ServiceEvent>,
    listener: Async<TcpListener>,
    acceptor: TlsAcceptor,
    metadata: CachedDevice,
}
struct Control {
    ready: Option<TopologyReady>,
    store: Store,
    discovery: Discovery,
    events: flume::Sender<mdns_sd::ServiceEvent>,
    listener: Async<TcpListener>,
    acceptor: TlsAcceptor,
    metadata: CachedDevice,
    context: Rc<Context>,
    statuses: Rc<RefCell<Vec<SourceStatus>>>,
    exited: Rc<RefCell<Option<Result<(), SourceError>>>>,
}
impl Control {
    async fn accept(&self) -> Broker {
        let (socket, _) = self.listener.accept().await.unwrap();
        Broker::new(self.acceptor.accept(socket).await.unwrap())
    }
    async fn exited(&self) -> SourceError {
        loop {
            if let Some(Err(error)) = &*self.exited.borrow() {
                return error.clone();
            }
            future::yield_now().await;
        }
    }
}
impl Rig {
    fn new(has_cache: bool, case: CloudCase, clock: Arc<dyn Clock>) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let mut tls = TlsFixture::new(false);
        if matches!(case, CloudCase::RefreshRejected) {
            tls.credentials.tokens.refresh_at = 1;
        }
        store.xiaomi().replace(&tls.credentials).unwrap();
        let metadata = cached(&tls.credentials.uid);
        if has_cache {
            store.devices().save(&metadata).unwrap();
        }
        let data = metadata.clone();
        let (base, _) = dynamic_mock_server(8, move |request| {
            if matches!(case, CloudCase::Failure) {
                return MockResponse::json(503, "{}");
            }
            if matches!(case, CloudCase::RefreshRejected) {
                return MockResponse::json(401, "{}");
            }
            let body = if request.target.ends_with("/gethome") {
                let uid = if matches!(case, CloudCase::ForeignAccount) {
                    "foreign-account"
                } else {
                    &data.uid
                };
                let homes = match case {
                    CloudCase::Missing => json!([{"id":"local","uid":uid,"dids":[]}]),
                    CloudCase::Moved => {
                        json!([{"id":"local","uid":uid,"dids":[]},{"id":"moved","uid":uid,"dids":["target"]}])
                    }
                    _ => json!([{"id":"local","uid":uid,"dids":["target"]}]),
                };
                json!({"code":0,"result":{"homelist":homes}})
            } else if request.target.ends_with("/device_list_page") {
                if matches!(case, CloudCase::DetailUnauthorized) {
                    return MockResponse::json(401, "{}");
                }
                json!({"code":0,"result":{"list":[{"did":"target","model":data.model,"spec_type":data.instance_type}]}})
            } else if request.target.starts_with("/miot-spec-v2/instance?") {
                let mut instance: Value =
                    serde_json::from_str(include_str!("../../../tests/fixtures/miot/outlet.json"))
                        .unwrap();
                if matches!(case, CloudCase::Unsupported) {
                    instance["services"] = json!([]);
                }
                instance
            } else {
                return MockResponse::json(503, "{}");
            };
            let response = MockResponse::json(200, &body.to_string());
            if matches!(case, CloudCase::Slow) && request.target.ends_with("/gethome") {
                response.delayed(Duration::from_millis(80))
            } else {
                response
            }
        });
        let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
        let listener = Async::<TcpListener>::bind(([127, 0, 0, 1], 0)).unwrap();
        let (events, receiver) = flume::unbounded();
        let (discovery, discovery_driver) = Discovery::with_events(receiver);
        let (source, ready) = XiaomiSource::for_test(
            store.clone(),
            cloud,
            discovery.clone(),
            DeviceKey::new("xiaomi", "target"),
            tls.ca_pem.clone(),
            clock,
        )
        .unwrap();
        Self {
            _directory: directory,
            store,
            source,
            ready,
            discovery,
            discovery_driver,
            events,
            listener,
            acceptor: TlsAcceptor::from(tls.server_config()),
            metadata,
        }
    }
    fn announce(&self) {
        self.events
            .send(announcement(
                self.listener.get_ref().local_addr().unwrap(),
                &self.metadata.uid,
                "hub",
                123,
            ))
            .unwrap();
    }
    async fn run<F, Fut>(self, test: F)
    where
        F: FnOnce(Control) -> Fut,
        Fut: Future<Output = ()>,
    {
        let statuses = Rc::new(RefCell::new(Vec::new()));
        let reports = statuses.clone();
        let exited = Rc::new(RefCell::new(None));
        let outcome = exited.clone();
        let context = self.source.context.clone();
        let keep_events = self.events.clone();
        let running = async {
            *outcome.borrow_mut() = Some(
                self.source
                    .run(move |status| reports.borrow_mut().push(status))
                    .await,
            );
            future::pending::<()>().await;
        };
        let discovering = async {
            self.discovery_driver.run().await.unwrap();
            panic!("discovery stopped");
        };
        let testing = test(Control {
            ready: Some(self.ready),
            store: self.store,
            discovery: self.discovery,
            events: self.events,
            listener: self.listener,
            acceptor: self.acceptor,
            metadata: self.metadata,
            context,
            statuses,
            exited,
        });
        future::or(
            future::or(future::or(running, discovering), testing),
            async {
                async_io::Timer::after(Duration::from_secs(3)).await;
                panic!("isolated source case timed out");
            },
        )
        .await;
        drop(keep_events);
    }
}
fn info(metadata: &CachedDevice, online: bool) -> Value {
    json!({"devList":{"target":{"model":metadata.model,"urn":metadata.instance_type,"online":online,"specV2Access":true,"pushAvailable":true}}})
}
async fn available(device: &Device) {
    let mut changes = device.subscribe();
    while !matches!(device.snapshot().availability, Availability::Available) {
        changes.changed().await;
    }
}
async fn initialize(broker: &mut Broker, metadata: &CachedDevice, value: bool) {
    broker.setup().await;
    initialize_application(broker, metadata, value).await;
}
async fn initialize_application(broker: &mut Broker, metadata: &CachedDevice, value: bool) {
    let Packet::Publish(list) = broker.next().await else {
        panic!("getDevList expected")
    };
    assert_eq!(list.topic, "master/proxy/getDevList");
    broker.acknowledge(&list).await;
    broker.reply(&list, info(metadata, true), 10).await;
    broker.subscription().await;
    let Packet::Publish(read) = broker.next().await else {
        panic!("get expected")
    };
    assert_eq!(read.topic, "master/proxy/get");
    broker.acknowledge(&read).await;
    broker.reply(&read, json!({"value":value}), 11).await;
}
fn notification(broker: &Broker, value: bool) -> Packet {
    Packet::Publish(Publish::new(
        format!("{}/appMsg/notify/iot/target/property/2.1", broker.client_id),
        QoS::AtMostOnce,
        encode_mips(
            700,
            None,
            &json!({"did":"target","siid":2,"piid":1,"value":value}),
        ),
    ))
}

async fn scenario<S, W, SF, WF>(has_cache: bool, cloud_failure: bool, server: S, work: W)
where
    S: FnOnce(Broker, CachedDevice) -> SF,
    W: FnOnce(TopologyReady, Store, Rc<RefCell<Vec<SourceStatus>>>) -> WF,
    SF: Future<Output = ()>,
    WF: Future<Output = ()>,
{
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let tls = TlsFixture::new(false);
    store.xiaomi().replace(&tls.credentials).unwrap();
    let metadata = cached(&tls.credentials.uid);
    if has_cache {
        store.devices().save(&metadata).unwrap();
    }
    let instance: Value =
        serde_json::from_str(include_str!("../../../tests/fixtures/miot/outlet.json")).unwrap();
    let uid = tls.credentials.uid.clone();
    let model = metadata.model.clone();
    let urn = metadata.instance_type.clone();
    let (base, _requests) = dynamic_mock_server(8, move |request| {
        if cloud_failure {
            return MockResponse::json(503, "{}").delayed(Duration::from_millis(60));
        }
        if request.target.ends_with("/gethome") {
            MockResponse::json(200,&json!({"code":0,"result":{"homelist":[{"id":"local","uid":uid,"dids":["target"]}]}}).to_string())
        } else if request.target.starts_with("/miot-spec-v2/instance?") {
            MockResponse::json(200, &instance.to_string())
        } else if request.target.ends_with("/device_list_page") {
            MockResponse::json(200,&json!({"code":0,"result":{"list":[{"did":"target","model":model,"spec_type":urn}]}}).to_string())
        } else {
            MockResponse::json(503, "{}")
        }
    });
    let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    let listener = Async::<TcpListener>::bind(([127, 0, 0, 1], 0)).unwrap();
    let address = listener.get_ref().local_addr().unwrap();
    let (events, receiver) = flume::unbounded();
    let (discovery, discovery_driver) = Discovery::with_events(receiver);
    events
        .send(announcement(address, &tls.credentials.uid, "hub", 123))
        .unwrap();
    let (source, ready) = XiaomiSource::for_test(
        store.clone(),
        cloud,
        discovery,
        DeviceKey::new("xiaomi", "target"),
        tls.ca_pem.clone(),
        Arc::new(SystemClock(Instant::now())),
    )
    .unwrap();
    let acceptor = TlsAcceptor::from(tls.server_config());
    let accepting = async {
        let (socket, _) = listener.accept().await.unwrap();
        let stream = acceptor.accept(socket).await.unwrap();
        server(Broker::new(stream), metadata).await;
    };
    let statuses = Rc::new(RefCell::new(Vec::new()));
    let reports = statuses.clone();
    let serving = future::or(
        source.run(move |status| reports.borrow_mut().push(status)),
        async { discovery_driver.run().await.map_err(Into::into) },
    );
    let testing = async {
        future::zip(accepting, work(ready, store, statuses)).await;
    };
    future::or(
        async {
            future::or(
                async {
                    serving.await.unwrap();
                    panic!("source stopped")
                },
                testing,
            )
            .await;
        },
        async {
            async_io::Timer::after(Duration::from_secs(3)).await;
            panic!("isolated source timed out")
        },
    )
    .await;
    drop(events);
}

#[test]
fn uncached_ready_follows_actual_local_preparation_and_static_save() {
    block_on(scenario(
        false,
        false,
        |mut broker, metadata| async move {
            initialize(&mut broker, &metadata, false).await;
            async_io::Timer::after(Duration::from_millis(20)).await;
        },
        |ready, store, _| async move {
            assert!(
                store
                    .devices()
                    .load("isolated-account", &DeviceKey::new("xiaomi", "target"))
                    .unwrap()
                    .is_none()
            );
            let prepared = ready.wait().await.unwrap();
            assert_eq!(prepared.device.snapshot().power, Power::Off);
            assert_eq!(
                prepared.device.snapshot().availability,
                Availability::Available
            );
            assert_eq!(
                store
                    .devices()
                    .load(
                        &prepared.metadata.uid,
                        &prepared.metadata.outlet.description.key
                    )
                    .unwrap(),
                Some(prepared.metadata)
            );
        },
    ));
}

#[test]
fn cached_cloud_failure_keeps_actual_local_control_and_diagnostic() {
    block_on(scenario(
        true,
        true,
        |mut broker, metadata| async move {
            initialize(&mut broker, &metadata, false).await;
            let Packet::Publish(write) = broker.next().await else {
                panic!("write expected")
            };
            broker.acknowledge(&write).await;
            broker
                .reply(&write, json!({"result":[{"did":"target","code":1}]}), 12)
                .await;
            async_io::Timer::after(Duration::from_millis(100)).await;
        },
        |ready, _, statuses| async move {
            let prepared = ready.wait().await.unwrap();
            available(&prepared.device).await;
            assert_eq!(
                prepared.device.execute(Command::On).await.unwrap().power,
                Power::On
            );
            async_io::Timer::after(Duration::from_millis(80)).await;
            assert!(statuses.borrow().iter().any(|status| matches!(
                status.authentication.authentication,
                AuthenticationState::Unavailable(_)
            )));
            assert_eq!(
                prepared.device.snapshot().availability,
                Availability::Available
            );
        },
    ));
}

#[test]
fn offline_list_then_write_success_cannot_accept_or_panic_and_notify_still_applies() {
    block_on(scenario(
        true,
        true,
        |mut broker, metadata| async move {
            initialize(&mut broker, &metadata, false).await;
            let Packet::Publish(write) = broker.next().await else {
                panic!("write expected")
            };
            broker.acknowledge(&write).await;
            broker
                .send(Packet::Publish(Publish::new(
                    format!("{}/appMsg/devListChange", broker.client_id),
                    QoS::AtMostOnce,
                    encode_mips(701, None, &json!({"devList":["target"]})),
                )))
                .await;
            let Packet::Publish(refresh) = broker.next().await else {
                panic!("refresh expected")
            };
            broker.acknowledge(&refresh).await;
            broker
                .batch(vec![
                    broker.business(&refresh, info(&metadata, false)),
                    broker.business(&write, json!({"result":[{"did":"target","code":0}]})),
                    notification(&broker, true),
                ])
                .await;
            let Packet::Publish(recovery) = broker.next().await else {
                panic!("recovery list expected")
            };
            broker.acknowledge(&recovery).await;
            broker.reply(&recovery, info(&metadata, true), 13).await;
            broker.subscription().await;
            let Packet::Publish(read) = broker.next().await else {
                panic!("recovery read expected")
            };
            broker.acknowledge(&read).await;
            broker
                .batch(vec![
                    broker.business(&read, json!({"value":false})),
                    notification(&broker, true),
                ])
                .await;
            async_io::Timer::after(Duration::from_millis(20)).await;
        },
        |ready, _, _| async move {
            let prepared = ready.wait().await.unwrap();
            available(&prepared.device).await;
            assert!(matches!(
                prepared.device.execute(Command::On).await,
                Err(CommandError::Unavailable(_))
            ));
            available(&prepared.device).await;
            assert_eq!(prepared.device.snapshot().power, Power::On);
        },
    ));
}

fn system_clock() -> Arc<dyn Clock> {
    Arc::new(SystemClock(Instant::now()))
}
async fn wait_for(mut predicate: impl FnMut() -> bool) {
    while !predicate() {
        future::yield_now().await;
    }
}
#[test]
fn uncached_cloud_failure_and_missing_target_fail_without_waiting_for_discovery() {
    block_on(async {
        for case in [
            CloudCase::Failure,
            CloudCase::RefreshRejected,
            CloudCase::Missing,
        ] {
            Rig::new(false, case, Arc::new(TestClock::default()))
                .run(|mut c| async move {
                    let error = c.ready.take().unwrap().wait().await.err().unwrap();
                    assert!(!matches!(error, SourceError::NoLocalGateway));
                    assert!(
                        c.store
                            .devices()
                            .load(&c.metadata.uid, &c.metadata.outlet.description.key)
                            .unwrap()
                            .is_none()
                    );
                    assert!(!matches!(c.exited().await, SourceError::NoLocalGateway));
                })
                .await;
        }
    });
}
#[test]
fn uncached_list_subscribe_and_read_failure_never_save_topology() {
    block_on(async {
        for stage in [0, 1, 2, 3] {
            let rig = Rig::new(false, CloudCase::Normal, system_clock());
            rig.announce();
            rig.run(|mut c| async move {
                let ready = c.ready.take().unwrap();
                future::zip(
                    async {
                        let mut broker = c.accept().await;
                        broker.setup().await;
                        let Packet::Publish(list) = broker.next().await else {
                            panic!()
                        };
                        broker.acknowledge(&list).await;
                        if stage == 0 {
                            broker
                                .send(broker.business(&list, info(&c.metadata, false)))
                                .await;
                        } else {
                            broker.reply(&list, info(&c.metadata, true), 80).await;
                            if stage == 1 {
                                let Packet::Subscribe(sub) = broker.next().await else {
                                    panic!()
                                };
                                broker
                                    .send(Packet::SubAck(SubAck::new(
                                        sub.pkid,
                                        vec![SubscribeReasonCode::NotAuthorized],
                                    )))
                                    .await;
                            } else {
                                broker.subscription().await;
                                let Packet::Publish(read) = broker.next().await else {
                                    panic!()
                                };
                                broker.acknowledge(&read).await;
                                if stage == 2 {
                                    broker
                                        .send(broker.business(&read, json!({"value":"false"})))
                                        .await;
                                } else {
                                    async_io::Timer::after(Duration::from_millis(250)).await;
                                }
                            }
                        }
                        async_io::Timer::after(Duration::from_millis(10)).await;
                    },
                    async {
                        assert!(ready.wait().await.is_err());
                        assert!(
                            c.store
                                .devices()
                                .load(&c.metadata.uid, &c.metadata.outlet.description.key)
                                .unwrap()
                                .is_none()
                        );
                        assert!(matches!(
                            c.exited().await,
                            SourceError::Central(_) | SourceError::Unavailable(_)
                        ));
                    },
                )
                .await;
            })
            .await;
        }
    });
}
#[test]
fn cached_removed_or_unsupported_is_unavailable_and_preserves_static_cache() {
    block_on(async {
        for case in [CloudCase::Missing, CloudCase::Unsupported] {
            let rig = Rig::new(true, case, system_clock());
            rig.announce();
            rig.run(|mut c| async move {
                let prepared = c.ready.take().unwrap().wait().await.unwrap();
                wait_for(|| c.context.live.borrow().blocked.is_some()).await;
                assert!(matches!(
                    prepared.device.snapshot().availability,
                    Availability::Unavailable(_)
                ));
                assert_eq!(
                    c.store
                        .devices()
                        .load(&c.metadata.uid, &c.metadata.outlet.description.key)
                        .unwrap(),
                    Some(c.metadata)
                );
                assert!(c.exited.borrow().is_none());
            })
            .await;
        }
    });
}
#[test]
fn cached_refresh_rejection_and_account_mismatch_keep_same_account_local_access() {
    block_on(async {
        for case in [
            CloudCase::RefreshRejected,
            CloudCase::ForeignAccount,
            CloudCase::DetailUnauthorized,
        ] {
            let rig = Rig::new(true, case, system_clock());
            rig.announce();
            rig.run(|mut c| async move {
                let ready = c.ready.take().unwrap();
                future::zip(
                    async {
                        let mut broker = c.accept().await;
                        initialize(&mut broker, &c.metadata, false).await;
                        let Packet::Publish(write) = broker.next().await else {
                            panic!("write expected")
                        };
                        broker.acknowledge(&write).await;
                        broker
                            .reply(&write, json!({"result":[{"did":"target","code":0}]}), 90)
                            .await;
                        async_io::Timer::after(Duration::from_millis(20)).await;
                    },
                    async {
                        let prepared = ready.wait().await.unwrap();
                        available(&prepared.device).await;
                        prepared.device.execute(Command::On).await.unwrap();
                        wait_for(|| {
                            c.statuses.borrow().iter().any(|s| {
                                matches!(
                                    s.authentication.authentication,
                                    AuthenticationState::SignInRequired(_)
                                        | AuthenticationState::Unavailable(
                                            FailureReason::AccountIdentityMismatch
                                        )
                                )
                            })
                        })
                        .await;
                        assert_eq!(
                            prepared.device.snapshot().availability,
                            Availability::Available
                        );
                        assert_eq!(
                            c.store.xiaomi().load().unwrap().unwrap().uid,
                            c.metadata.uid
                        );
                    },
                )
                .await;
            })
            .await;
        }
    });
}
#[test]
fn slow_authentication_does_not_consume_the_initial_discovery_wait() {
    block_on(async {
        let clock = Arc::new(TestClock::default());
        let rig = Rig::new(false, CloudCase::Slow, clock.clone());
        rig.announce();
        rig.run(|mut c| async move {
            let ready = c.ready.take().unwrap();
            future::zip(
                async {
                    let mut broker = c.accept().await;
                    initialize(&mut broker, &c.metadata, true).await;
                    async_io::Timer::after(Duration::from_millis(10)).await;
                },
                async {
                    async_io::Timer::after(Duration::from_millis(20)).await;
                    clock.advance(Duration::from_secs(60));
                    assert_eq!(
                        ready.wait().await.unwrap().device.snapshot().availability,
                        Availability::Available
                    );
                },
            )
            .await;
        })
        .await;
    });
}
#[test]
fn actual_discovery_wait_expires_even_if_a_late_announcement_is_ready() {
    block_on(async {
        let clock = Arc::new(TestClock::default());
        Rig::new(false, CloudCase::Normal, clock.clone())
            .run(|mut c| async move {
                wait_for(|| clock.deadlines.lock().unwrap().contains(&DISCOVERY_WAIT)).await;
                clock.advance(Duration::from_secs(31));
                c.events
                    .send(announcement(
                        c.listener.get_ref().local_addr().unwrap(),
                        &c.metadata.uid,
                        "late",
                        123,
                    ))
                    .unwrap();
                assert!(matches!(
                    c.ready.take().unwrap().wait().await,
                    Err(SourceError::NoLocalGateway)
                ));
                assert!(
                    c.store
                        .devices()
                        .load(&c.metadata.uid, &c.metadata.outlet.description.key)
                        .unwrap()
                        .is_none()
                );
            })
            .await;
    });
}
#[test]
fn moved_target_waits_for_its_new_owned_home_and_updates_cache_without_replacing_device() {
    block_on(async {
        let rig = Rig::new(true, CloudCase::Moved, system_clock());
        rig.announce();
        rig.run(|mut c| async move {
            let prepared = c.ready.take().unwrap().wait().await.unwrap();
            wait_for(|| c.context.live.borrow().blocked.is_some()).await;
            assert!(!matches!(
                prepared.device.snapshot().availability,
                Availability::Available
            ));
            c.events
                .send(home_announcement(
                    c.listener.get_ref().local_addr().unwrap(),
                    &c.metadata.uid,
                    "moved",
                    "new-home",
                    999,
                ))
                .unwrap();
            let mut updated = c.metadata.clone();
            updated.home_id = "moved".into();
            updated.group_id = group_id(&updated.uid, "moved");
            // An old attempted TLS connection can precede the directory result.
            // Complete handshakes until the current route starts MQTT.
            future::zip(
                async {
                    loop {
                        let mut broker = c.accept().await;
                        if broker.setup_if_connected().await {
                            initialize_application(&mut broker, &updated, true).await;
                            break;
                        }
                    }
                    async_io::Timer::after(Duration::from_millis(20)).await;
                },
                async {
                    available(&prepared.device).await;
                    wait_for(|| {
                        c.store
                            .devices()
                            .load(&updated.uid, &updated.outlet.description.key)
                            .unwrap()
                            == Some(updated.clone())
                    })
                    .await;
                    assert_eq!(prepared.device.snapshot().power, Power::On);
                },
            )
            .await;
        })
        .await;
    });
}
#[test]
fn unavailable_clears_stable_history_and_retry_delays_cap_at_thirty_seconds() {
    let clock = Arc::new(TestClock::default());
    let rig = Rig::new(true, CloudCase::Failure, clock.clone());
    let context = rig.source.context.clone();
    context.live.borrow_mut().generation = Some(1);
    context.prepared(1, 0, false);
    clock.advance(Duration::from_secs(30));
    // The deadline passed before the session's stable timer was polled.
    assert!(!context.live.borrow().stable);
    context.unavailable("offline".into());
    assert!(!context.live.borrow().stable);
    assert!(context.live.borrow().reset_backoff);
    context.live.borrow_mut().reset_backoff = false;
    let version = context.live.borrow().preparation;
    context.prepared(1, version, true);
    clock.advance(Duration::from_secs(1));
    assert!(!context.live.borrow().stable);
    context.unavailable("short availability".into());
    assert!(!context.live.borrow().reset_backoff);
    let mut retry = Backoff::new();
    let delays = (0..7)
        .map(|_| retry.failed(false).as_secs())
        .collect::<Vec<_>>();
    assert_eq!(delays, [1, 2, 4, 8, 16, 30, 30]);
    assert_eq!(retry.failed(true), Duration::from_secs(1));
}
#[test]
fn static_cache_save_error_is_fatal_after_local_success_and_before_ready() {
    block_on(async {
        let rig = Rig::new(false, CloudCase::Normal, system_clock());
        rig.announce();
        rusqlite::Connection::open(rig.store.path()).unwrap().execute_batch("CREATE TRIGGER fail_cache BEFORE INSERT ON device_cache BEGIN SELECT RAISE(ABORT, 'isolated disk failure'); END;").unwrap();
        rig.run(|mut c| async move {
            let ready = c.ready.take().unwrap();
            future::zip(
                async {
                    let mut broker = c.accept().await;
                    // A QoS0 final reply allows the runner to terminate at its save boundary.
                    broker.setup().await;
                    let Packet::Publish(list) = broker.next().await else {
                        panic!()
                    };
                    broker.acknowledge(&list).await;
                    broker.reply(&list, info(&c.metadata, true), 95).await;
                    broker.subscription().await;
                    let Packet::Publish(read) = broker.next().await else {
                        panic!()
                    };
                    broker.acknowledge(&read).await;
                    broker
                        .send(broker.business(&read, json!({"value":true})))
                        .await;
                    async_io::Timer::after(Duration::from_millis(20)).await;
                },
                async {
                    assert!(matches!(ready.wait().await, Err(SourceError::Storage(_))));
                    assert!(matches!(c.exited().await, SourceError::Storage(_)));
                    assert!(
                        c.store
                            .devices()
                            .load(&c.metadata.uid, &c.metadata.outlet.description.key)
                            .unwrap()
                            .is_none()
                    );
                },
            )
            .await;
        })
        .await;
    });
}

fn removed(name: &str) -> mdns_sd::ServiceEvent {
    mdns_sd::ServiceEvent::ServiceRemoved(SERVICE_TYPE.into(), format!("{name}.{SERVICE_TYPE}"))
}
#[test]
fn withdrawn_route_during_connect_never_subscribes_or_sends_application_requests() {
    block_on(async {
        let rig = Rig::new(true, CloudCase::Failure, system_clock());
        rig.announce();
        rig.run(|mut c| async move {
            let prepared = c.ready.take().unwrap().wait().await.unwrap();
            let mut broker = c.accept().await;
            assert!(matches!(broker.next().await, Packet::Connect(_)));
            c.events.send(removed("hub")).unwrap();
            wait_for(|| c.discovery.snapshot().gateways.is_empty()).await;
            broker
                .send(Packet::ConnAck(ConnAck::new(
                    ConnectReturnCode::Success,
                    false,
                )))
                .await;
            assert!(broker.next_optional().await.is_none());
            assert!(c.context.live.borrow().generation.is_none());
            assert_eq!(prepared.device.snapshot().power, Power::Unknown);
            assert!(matches!(
                prepared.device.snapshot().availability,
                Availability::Unavailable(_)
            ));
        })
        .await;
    });
}
#[test]
fn route_change_cancels_pending_write_and_recovers_with_the_same_device() {
    block_on(async {
        let clock = Arc::new(TestClock::default());
        let rig = Rig::new(true, CloudCase::Failure, clock.clone());
        rig.announce();
        rig.run(|mut c| async move {
            let prepared = c.ready.take().unwrap().wait().await.unwrap();
            let old_generation = Rc::new(std::cell::Cell::new(None));
            let generation = old_generation.clone();
            future::zip(
                async {
                    let mut broker = c.accept().await;
                    initialize(&mut broker, &c.metadata, false).await;
                    generation.set(c.context.live.borrow().generation);
                    // A backup cannot interrupt the current master.
                    c.events
                        .send(announcement(
                            c.listener.get_ref().local_addr().unwrap(),
                            &c.metadata.uid,
                            "backup",
                            999,
                        ))
                        .unwrap();
                    let Packet::Publish(write) = broker.next().await else {
                        panic!("write expected")
                    };
                    broker.acknowledge(&write).await;
                    assert_eq!(c.context.live.borrow().generation, generation.get());
                    c.events.send(removed("hub")).unwrap();
                    wait_for(|| c.context.live.borrow().generation.is_none()).await;
                    assert!(broker.next_optional().await.is_none());
                    clock.advance(Duration::from_secs(1));
                    let mut replacement = c.accept().await;
                    initialize(&mut replacement, &c.metadata, true).await;
                    async_io::Timer::after(Duration::from_millis(20)).await;
                },
                async {
                    available(&prepared.device).await;
                    assert!(matches!(
                        prepared.device.execute(Command::On).await,
                        Err(CommandError::Unavailable(_))
                    ));
                    available(&prepared.device).await;
                    assert_eq!(prepared.device.snapshot().power, Power::On);
                    assert_ne!(c.context.live.borrow().generation, old_generation.get());
                    assert_eq!(
                        prepared.device.description().key,
                        c.metadata.outlet.description.key
                    );
                },
            )
            .await;
        })
        .await;
    });
}
#[test]
fn preparation_superseded_by_offline_reply_cannot_restore_available_from_old_read() {
    block_on(async {
        let rig = Rig::new(true, CloudCase::Failure, system_clock());
        rig.announce();
        rig.run(|mut c| async move {
            let prepared = c.ready.take().unwrap().wait().await.unwrap();
            let mut broker = c.accept().await;
            broker.setup().await;
            let Packet::Publish(list) = broker.next().await else {
                panic!()
            };
            broker.acknowledge(&list).await;
            broker.reply(&list, info(&c.metadata, true), 100).await;
            broker.subscription().await;
            let Packet::Publish(read) = broker.next().await else {
                panic!()
            };
            broker.acknowledge(&read).await;
            broker
                .send(Packet::Publish(Publish::new(
                    format!("{}/appMsg/devListChange", broker.client_id),
                    QoS::AtMostOnce,
                    encode_mips(701, None, &json!({"devList":["target"]})),
                )))
                .await;
            let Packet::Publish(refresh) = broker.next().await else {
                panic!()
            };
            broker.acknowledge(&refresh).await;
            broker
                .batch(vec![
                    broker.business(&refresh, info(&c.metadata, false)),
                    broker.business(&read, json!({"value":false})),
                    notification(&broker, true),
                ])
                .await;
            wait_for(|| prepared.device.snapshot().power == Power::On).await;
            assert!(matches!(
                prepared.device.snapshot().availability,
                Availability::Unavailable(_)
            ));
            assert!(c.context.live.borrow().needs_prepare);
        })
        .await;
    });
}
#[test]
fn queued_command_budget_is_remaining_time_and_expired_unsent_commands_never_publish() {
    block_on(async {
        for expired in [false, true] {
            let clock = Arc::new(TestClock::default());
            let rig = Rig::new(true, CloudCase::Failure, clock.clone());
            rig.announce();
            rig.run(|mut c| async move {
                let prepared = c.ready.take().unwrap().wait().await.unwrap();
                let sending_clock = clock.clone();
                future::zip(
                    async {
                        let mut broker = c.accept().await;
                        initialize(&mut broker, &c.metadata, false).await;
                        let Packet::Publish(first) = broker.next().await else {
                            panic!()
                        };
                        broker.acknowledge(&first).await;
                        // Give both callers a poll to enter the FIFO at time zero.
                        async_io::Timer::after(Duration::from_millis(10)).await;
                        sending_clock.advance(Duration::from_secs(if expired { 6 } else { 4 }));
                        broker
                            .reply(&first, json!({"result":[{"did":"target","code":0}]}), 110)
                            .await;
                        if expired {
                            assert!(
                                future::or(
                                    async {
                                        broker.next_optional().await;
                                        false
                                    },
                                    async {
                                        async_io::Timer::after(Duration::from_millis(60)).await;
                                        true
                                    }
                                )
                                .await,
                                "expired command was sent"
                            );
                        } else {
                            let Packet::Publish(second) = broker.next().await else {
                                panic!()
                            };
                            assert_eq!(second.topic, "master/proxy/rpcReq");
                            broker.acknowledge(&second).await;
                            // The queue consumed four seconds of the five-second budget.
                            async_io::Timer::after(Duration::from_millis(1150)).await;
                        }
                    },
                    async {
                        available(&prepared.device).await;
                        let started = Instant::now();
                        let (first, second) = future::zip(
                            prepared.device.execute(Command::On),
                            prepared.device.execute(Command::Off),
                        )
                        .await;
                        if expired {
                            assert_eq!(first, Err(CommandError::Timeout));
                            assert_eq!(second, Err(CommandError::Timeout));
                            assert_eq!(prepared.device.snapshot().power, Power::Off);
                        } else {
                            first.unwrap();
                            assert_eq!(second, Err(CommandError::Timeout));
                            assert!(started.elapsed() < Duration::from_secs(2));
                            assert_eq!(prepared.device.snapshot().power, Power::On);
                        }
                    },
                )
                .await;
            })
            .await;
        }
    });
}

#[test]
fn disconnected_sent_write_is_not_replayed_and_new_session_recovers_the_same_receiver() {
    block_on(async {
        let clock = Arc::new(TestClock::default());
        let rig = Rig::new(true, CloudCase::Failure, clock.clone());
        rig.announce();
        rig.run(|mut c| async move {
            let prepared = c.ready.take().unwrap().wait().await.unwrap();
            future::zip(
                async {
                    let mut old = c.accept().await;
                    initialize(&mut old, &c.metadata, false).await;
                    let Packet::Publish(write) = old.next().await else {
                        panic!("old write expected")
                    };
                    old.acknowledge(&write).await;
                    drop(old);
                    wait_for(|| c.context.live.borrow().generation.is_none()).await;
                    clock.advance(Duration::from_secs(1));
                    let mut new = c.accept().await;
                    initialize(&mut new, &c.metadata, true).await;
                    let Packet::Publish(write) = new.next().await else {
                        panic!("new write expected")
                    };
                    let payload = crate::xiaomi::central::decode_mips(&write.payload)
                        .unwrap()
                        .payload;
                    assert_eq!(
                        payload["rpc"]["params"][0]["value"], false,
                        "old On write was replayed"
                    );
                    new.acknowledge(&write).await;
                    new.reply(&write, json!({"result":[{"did":"target","code":0}]}), 125)
                        .await;
                    assert!(
                        future::or(
                            async {
                                new.next_optional().await;
                                false
                            },
                            async {
                                async_io::Timer::after(Duration::from_millis(70)).await;
                                true
                            }
                        )
                        .await,
                        "unexpected property polling or replay"
                    );
                },
                async {
                    available(&prepared.device).await;
                    assert!(matches!(
                        prepared.device.execute(Command::On).await,
                        Err(CommandError::SendFailed(_) | CommandError::Unavailable(_))
                    ));
                    available(&prepared.device).await;
                    assert_eq!(prepared.device.snapshot().power, Power::On);
                    assert_eq!(
                        prepared.device.execute(Command::Off).await.unwrap().power,
                        Power::Off
                    );
                },
            )
            .await;
        })
        .await;
    });
}

#[test]
fn device_list_hints_during_refresh_preserve_a_followup_and_cancel_pending_commands() {
    block_on(async {
        let rig = Rig::new(true, CloudCase::Failure, system_clock());
        rig.announce();
        rig.run(|mut c| async move {
            let prepared = c.ready.take().unwrap().wait().await.unwrap();
            future::zip(
                async {
                    let mut broker = c.accept().await;
                    initialize(&mut broker, &c.metadata, false).await;
                    let Packet::Publish(write) = broker.next().await else {
                        panic!()
                    };
                    broker.acknowledge(&write).await;
                    let hint = |b: &Broker, id| {
                        Packet::Publish(Publish::new(
                            format!("{}/appMsg/devListChange", b.client_id),
                            QoS::AtMostOnce,
                            encode_mips(id, None, &json!({"devList":["target"]})),
                        ))
                    };
                    broker.send(hint(&broker, 801)).await;
                    let Packet::Publish(refresh) = broker.next().await else {
                        panic!()
                    };
                    broker.acknowledge(&refresh).await;
                    broker.send(hint(&broker, 802)).await;
                    broker.reply(&refresh, info(&c.metadata, true), 140).await;
                    let Packet::Publish(followup) = broker.next().await else {
                        panic!("followup refresh expected")
                    };
                    assert_eq!(followup.topic, "master/proxy/getDevList");
                    broker.acknowledge(&followup).await;
                    broker
                        .batch(vec![
                            broker.business(&followup, info(&c.metadata, false)),
                            broker.business(&write, json!({"result":[{"did":"target","code":0}]})),
                        ])
                        .await;
                    async_io::Timer::after(Duration::from_millis(20)).await;
                },
                async {
                    available(&prepared.device).await;
                    assert!(matches!(
                        prepared.device.execute(Command::On).await,
                        Err(CommandError::Unavailable(_))
                    ));
                    assert_eq!(prepared.device.snapshot().power, Power::Off);
                    assert!(matches!(
                        prepared.device.snapshot().availability,
                        Availability::Unavailable(_)
                    ));
                },
            )
            .await;
        })
        .await;
    });
}
#[test]
fn local_model_and_instance_type_must_match_the_static_description() {
    block_on(async {
        for field in ["model", "urn"] {
            let rig = Rig::new(true, CloudCase::Failure, system_clock());
            rig.announce();
            rig.run(|mut c|async move {
                let prepared=c.ready.take().unwrap().wait().await.unwrap();
                future::zip(async {
                    let mut broker=c.accept().await;broker.setup().await;
                    let Packet::Publish(list)=broker.next().await else {panic!()};broker.acknowledge(&list).await;
                    let mut value=info(&c.metadata,true);value["devList"]["target"][field]=json!("wrong-static-description");
                    broker.send(broker.business(&list,value)).await;
                    async_io::Timer::after(Duration::from_millis(30)).await;
                },async {
                    wait_for(||matches!(prepared.device.snapshot().availability,Availability::Unavailable(ref why) if why.contains("differs"))).await;
                    assert_eq!(prepared.device.snapshot().power,Power::Unknown);
                    assert!(c.exited.borrow().is_none());
                    assert_eq!(c.store.devices().load(&c.metadata.uid,&c.metadata.outlet.description.key).unwrap(),Some(c.metadata.clone()));
                }).await;
            }).await;
        }
    });
}
#[test]
fn corrupt_static_cache_is_fatal_at_the_system_boundary_without_overwriting_data() {
    let rig = Rig::new(true, CloudCase::Failure, system_clock());
    rusqlite::Connection::open(rig.store.path())
        .unwrap()
        .execute("UPDATE device_cache SET instance_type='invalid'", [])
        .unwrap();
    let (base, _) = mock_server(vec![MockResponse::json(503, "{}")]);
    let result = XiaomiSource::new(
        rig.store.clone(),
        CloudClient::for_test(&base, Duration::from_secs(1)).unwrap(),
        rig.discovery.clone(),
        DeviceKey::new("xiaomi", "target"),
    );
    assert!(matches!(result, Err(SourceError::Storage(_))));
    let value: String = rusqlite::Connection::open(rig.store.path())
        .unwrap()
        .query_row("SELECT instance_type FROM device_cache", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(value, "invalid");
}

#[test]
fn thirty_seconds_available_resets_connection_backoff_before_later_offline() {
    block_on(async {
        let clock = Arc::new(TestClock::default());
        let rig = Rig::new(true, CloudCase::Failure, clock.clone());
        rig.announce();
        rig.run(|mut c| async move {
            let prepared = c.ready.take().unwrap().wait().await.unwrap();
            for delay in [1, 2] {
                let mut broker = c.accept().await;
                assert!(matches!(broker.next().await, Packet::Connect(_)));
                broker
                    .send(Packet::ConnAck(ConnAck::new(
                        ConnectReturnCode::NotAuthorized,
                        false,
                    )))
                    .await;
                assert!(broker.next_optional().await.is_none());
                clock.advance(Duration::from_secs(delay));
            }
            let mut stable = c.accept().await;
            initialize(&mut stable, &c.metadata, false).await;
            clock.advance(Duration::from_secs(30));
            wait_for(|| c.context.live.borrow().stable).await;
            stable
                .send(Packet::Publish(Publish::new(
                    format!("{}/appMsg/devListChange", stable.client_id),
                    QoS::AtMostOnce,
                    encode_mips(820, None, &json!({"devList":["target"]})),
                )))
                .await;
            let Packet::Publish(refresh) = stable.next().await else {
                panic!()
            };
            stable.acknowledge(&refresh).await;
            stable
                .send(stable.business(&refresh, info(&c.metadata, false)))
                .await;
            wait_for(|| !c.context.live.borrow().stable).await;
            drop(stable);
            wait_for(|| c.context.live.borrow().generation.is_none()).await;
            clock.advance(Duration::from_secs(1));
            let mut recovered = c.accept().await;
            initialize(&mut recovered, &c.metadata, true).await;
            available(&prepared.device).await;
            assert_eq!(prepared.device.snapshot().power, Power::On);
            drop(recovered);
            wait_for(|| c.context.live.borrow().generation.is_none()).await;
            clock.advance(Duration::from_secs(1));
            async_io::Timer::after(Duration::from_millis(10)).await;
            assert!(
                poll_once(c.listener.accept()).await.is_none(),
                "short recovery reused the previous stable reset"
            );
            clock.advance(Duration::from_secs(1));
            let mut final_session = c.accept().await;
            initialize(&mut final_session, &c.metadata, false).await;
            available(&prepared.device).await;
            assert_eq!(prepared.device.snapshot().power, Power::Off);
        })
        .await;
    });
}
