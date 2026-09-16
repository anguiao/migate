use std::{rc::Rc, time::Duration};

use futures_util::{FutureExt, future::LocalBoxFuture};
use migate::{
    device::{
        AccountId, CommandOutcome, DeviceCommand, DeviceDid, DeviceService, FeatureIdentity,
        HomeId, PhysicalDeviceId, Property,
    },
    storage::{Store, TokenSet, XiaomiRecord},
    xiaomi::{
        catalog::{FeatureDescriptor, WireValue, compile_spec},
        discovery::NetworkEpoch,
        runtime::{
            AdmissionFeature, AdmissionSnapshot, AdmissionStatus, CommandLimits, CommandRuntime,
            CommandTransport, ControlPath, GatewayPathEvidence, OperationPaths, PushSource,
            RuntimeFeature, SendGuard, StateLimits, StateReadFailure, StateReadGuard,
            StateReadRequest, StateReadResult, StateReadTransport, StateRuntime, TransportCommand,
            TransportFailure,
        },
    },
};

struct UnusedTransport;

impl CommandTransport for UnusedTransport {
    fn send(
        &self,
        _: ControlPath,
        _: TransportCommand,
        _: Duration,
        _: SendGuard,
    ) -> LocalBoxFuture<'static, Result<(), TransportFailure>> {
        async { panic!("this boundary test must not send a command") }.boxed_local()
    }
}

impl StateReadTransport for UnusedTransport {
    fn read(
        &self,
        _: StateReadRequest,
        _: Duration,
        _: StateReadGuard,
    ) -> LocalBoxFuture<'static, Result<Vec<StateReadResult>, StateReadFailure>> {
        async { panic!("this boundary test must not start a read") }.boxed_local()
    }
}

struct Harness {
    directory: tempfile::TempDir,
    service: DeviceService,
    state: StateRuntime,
    snapshot: AdmissionSnapshot,
    feature: FeatureIdentity,
    descriptor: FeatureDescriptor,
}

impl Harness {
    fn new() -> Self {
        Self::with_options(StateLimits::default(), false)
    }

    fn with_options(limits: StateLimits, notify_only: bool) -> Self {
        Self::with_read_transport(limits, notify_only, Rc::new(UnusedTransport))
    }

    fn with_read_transport(
        limits: StateLimits,
        notify_only: bool,
        read_transport: Rc<dyn StateReadTransport>,
    ) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        store
            .xiaomi()
            .replace(&XiaomiRecord {
                uid: "10001".into(),
                region: "cn".into(),
                oauth_client_uuid: "123e4567-e89b-12d3-a456-426614174000".into(),
                redirect_uri: "http://127.0.0.1/callback".into(),
                tokens: TokenSet {
                    access_token: "synthetic-access".into(),
                    refresh_token: "synthetic-refresh".into(),
                    expires_at: 2_000_000_000,
                    refresh_at: 1_900_000_000,
                },
                virtual_did: "123456789012345".into(),
                private_key_pem: "synthetic-key".into(),
                certificate_pem: "synthetic-certificate".into(),
            })
            .unwrap();
        let document = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/miot_specs/yeelink.light.ml9.json"
        ))
        .unwrap();
        let mut descriptor = compile_spec("yeelink.light.ml9", &document)
            .unwrap()
            .features
            .remove(0);
        if notify_only {
            for mapping in &mut descriptor.properties {
                mapping.readable = false;
            }
        }
        let feature = FeatureIdentity {
            physical: PhysicalDeviceId {
                account: AccountId::new("10001").unwrap(),
                home: HomeId::new("home-a").unwrap(),
                parent_did: DeviceDid::new("1234").unwrap(),
            },
            service_instance: descriptor.service_instance,
            role: descriptor.role,
        };
        let service = DeviceService::new();
        store.devices().allocate_feature(&feature).unwrap();
        service.publish(
            feature.clone(),
            "Review light",
            descriptor.capabilities.clone(),
        );
        let commands = CommandRuntime::new(service.clone(), Rc::new(UnusedTransport));
        let state = StateRuntime::with_limits(
            service.clone(),
            store.devices(),
            read_transport,
            commands.execution_gate(),
            commands.subscribe_completions(),
            limits,
        );
        let paths = OperationPaths {
            gateway: true,
            lan: false,
            cloud: false,
        };
        let snapshot = AdmissionSnapshot {
            binding: None,
            status: AdmissionStatus::Active,
            epoch: NetworkEpoch::new(7),
            features: vec![AdmissionFeature {
                identity: feature.clone(),
                runtime: RuntimeFeature {
                    identity: feature.clone(),
                    descriptor: descriptor.clone(),
                    authority_generation: 1,
                    auth_session_generation: store.xiaomi().snapshot().unwrap().session_generation,
                },
                paths,
                gateways: vec![GatewayPathEvidence {
                    gateway_did: 99,
                    access: true,
                    push: true,
                    online: Some(true),
                }],
                lan_evidence: None,
            }],
        };
        state.reconcile(&snapshot);
        Self {
            directory,
            service,
            state,
            snapshot,
            feature,
            descriptor,
        }
    }
}

#[test]
fn readmission_keeps_unknown_state_unavailable_in_events_and_send_checks() {
    let harness = Harness::new();
    assert!(!harness.service.is_available(&harness.feature));
    assert!(!harness.service.can_control(&harness.feature));
    harness.service.revoke(&harness.feature);
    let mut changes = harness.service.subscribe();
    harness.service.admit(&harness.feature);
    assert!(!harness.service.is_available(&harness.feature));
    assert!(changes.drain().iter().all(|change| !matches!(
        change,
        migate::device::DeviceChange::AvailabilityChanged {
            available: true,
            ..
        }
    )));
}

#[test]
fn same_epoch_cloud_fallback_keeps_confirmed_state_available() {
    let mut harness = Harness::new();
    let token = harness
        .state
        .select_push_source(&harness.feature.physical, PushSource::Gateway(99), 41, 1)
        .unwrap();
    assert!(harness.state.acknowledge(&token, 41, 1));
    let power = harness
        .descriptor
        .properties
        .iter()
        .find(|mapping| mapping.property == Property::Power)
        .unwrap();
    assert!(harness.state.apply_property(
        &token,
        41,
        1,
        power.siid,
        power.piid,
        Some(&WireValue::Boolean(true)),
        10,
        false
    ));
    let changed = &mut harness.snapshot.features[0];
    changed.paths = OperationPaths {
        gateway: false,
        lan: false,
        cloud: true,
    };
    changed.gateways.clear();
    harness.state.reconcile(&harness.snapshot);
    assert!(matches!(
        harness
            .service
            .snapshot(&harness.feature)
            .unwrap()
            .property(Property::Power),
        Some(migate::device::PropertyState::Current { .. })
    ));
    assert!(harness.service.is_available(&harness.feature));
}

#[test]
fn stopped_state_runtime_rejects_previously_valid_pushes() {
    let harness = Harness::new();
    let token = harness
        .state
        .select_push_source(&harness.feature.physical, PushSource::Gateway(99), 41, 1)
        .unwrap();
    assert!(harness.state.acknowledge(&token, 41, 1));
    let power = harness
        .descriptor
        .properties
        .iter()
        .find(|mapping| mapping.property == Property::Power)
        .unwrap();
    harness.state.stop();
    assert!(!harness.state.apply_property(
        &token,
        41,
        1,
        power.siid,
        power.piid,
        Some(&WireValue::Boolean(true)),
        10,
        false
    ));
    assert!(
        harness
            .state
            .select_push_source(&harness.feature.physical, PushSource::Gateway(99), 42, 1)
            .is_none()
    );
}

fn cache_limits() -> StateLimits {
    StateLimits {
        cache_flush_interval: Duration::from_millis(10),
        ..StateLimits::default()
    }
}

async fn expect_persisted_count(directory: &std::path::Path, count: usize) {
    let reader = Store::open(directory).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_millis(300);
    loop {
        let states = reader.devices().load_states().unwrap();
        if states.len() == count {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "expected {count} cached properties, got {}",
            states.len()
        );
        async_io::Timer::after(Duration::from_millis(5)).await;
    }
}

#[test]
fn idle_actor_flushes_a_pure_push_without_explicit_flush() {
    let harness = Harness::with_options(cache_limits(), true);
    let token = harness
        .state
        .select_push_source(&harness.feature.physical, PushSource::Gateway(99), 41, 1)
        .unwrap();
    assert!(harness.state.acknowledge(&token, 41, 1));
    let power = harness
        .descriptor
        .properties
        .iter()
        .find(|mapping| mapping.property == Property::Power)
        .unwrap();
    futures_lite::future::block_on(futures_lite::future::or(
        async {
            async_io::Timer::after(Duration::from_millis(1)).await;
            assert!(harness.state.apply_property(
                &token,
                41,
                1,
                power.siid,
                power.piid,
                Some(&WireValue::Boolean(true)),
                10,
                false
            ));
            expect_persisted_count(harness.directory.path(), 1).await;
            harness.state.stop();
        },
        async {
            harness.state.run().await;
            panic!("state actor returned before the cache assertion");
        },
    ));
}

#[test]
fn failed_full_cache_eventually_saves_all_latest_confirmed_values() {
    let harness = Harness::with_options(cache_limits(), true);
    let token = harness
        .state
        .select_push_source(&harness.feature.physical, PushSource::Gateway(99), 41, 1)
        .unwrap();
    assert!(harness.state.acknowledge(&token, 41, 1));
    let connection = rusqlite::Connection::open(harness.directory.path().join("state.db")).unwrap();
    connection.execute_batch("CREATE TRIGGER reject_cache BEFORE INSERT ON feature_states BEGIN SELECT RAISE(ABORT, 'test write failure'); END;").unwrap();
    futures_lite::future::block_on(futures_lite::future::or(
        async {
            async_io::Timer::after(Duration::from_millis(1)).await;
            for (property, value) in [
                (Property::Power, WireValue::Boolean(true)),
                (Property::Brightness, WireValue::Integer(50)),
                (Property::ColorTemperature, WireValue::Integer(4000)),
            ] {
                let mapping = harness
                    .descriptor
                    .properties
                    .iter()
                    .find(|mapping| mapping.property == property)
                    .unwrap();
                assert!(harness.state.apply_property(
                    &token,
                    41,
                    1,
                    mapping.siid,
                    mapping.piid,
                    Some(&value),
                    10,
                    false
                ));
            }
            let power = harness
                .descriptor
                .properties
                .iter()
                .find(|mapping| mapping.property == Property::Power)
                .unwrap();
            assert!(harness.state.apply_property(
                &token,
                41,
                1,
                power.siid,
                power.piid,
                Some(&WireValue::Boolean(false)),
                11,
                false
            ));
            assert!(
                harness
                    .state
                    .apply_property(&token, 41, 1, power.siid, power.piid, None, 12, false)
            );
            async_io::Timer::after(Duration::from_millis(25)).await;
            connection
                .execute_batch("DROP TRIGGER reject_cache")
                .unwrap();
            expect_persisted_count(harness.directory.path(), 3).await;
            let stored = Store::open(harness.directory.path())
                .unwrap()
                .devices()
                .load_states()
                .unwrap();
            let power = stored
                .iter()
                .find(|state| state.property == Property::Power)
                .unwrap();
            assert_eq!(power.value, migate::device::PropertyValue::Power(false));
            assert_eq!(power.observed_at, 11);
            harness.state.stop();
        },
        async {
            harness.state.run().await;
            panic!("state actor returned before the cache recovery assertion");
        },
    ));
}

struct FailingReadTransport {
    seen: Rc<std::cell::RefCell<std::collections::BTreeSet<PhysicalDeviceId>>>,
}

impl StateReadTransport for FailingReadTransport {
    fn read(
        &self,
        request: StateReadRequest,
        _: Duration,
        guard: StateReadGuard,
    ) -> LocalBoxFuture<'static, Result<Vec<StateReadResult>, StateReadFailure>> {
        let seen = self.seen.clone();
        async move {
            assert!(guard.permitted());
            seen.borrow_mut().insert(request.device);
            async_io::Timer::after(Duration::from_millis(5)).await;
            Err(StateReadFailure::Unavailable)
        }
        .boxed_local()
    }
}

#[test]
fn retrying_early_devices_cannot_starve_later_initial_reads() {
    let seen = Rc::new(std::cell::RefCell::new(std::collections::BTreeSet::new()));
    let mut harness = Harness::with_read_transport(
        StateLimits {
            queue_capacity: 2,
            global_concurrency: 1,
            retry_initial: Duration::from_millis(1),
            retry_max: Duration::from_millis(1),
            minimum_read_interval: Duration::from_millis(1),
            ..StateLimits::default()
        },
        false,
        Rc::new(FailingReadTransport { seen: seen.clone() }),
    );
    for index in 1..4 {
        let mut added = harness.snapshot.features[0].clone();
        added.identity.physical.parent_did = DeviceDid::new(format!("{}", 1234 + index)).unwrap();
        added.runtime.identity = added.identity.clone();
        harness.service.publish(
            added.identity.clone(),
            format!("Light {index}"),
            harness.descriptor.capabilities.clone(),
        );
        harness.snapshot.features.push(added);
    }
    harness.state.reconcile(&harness.snapshot);
    futures_lite::future::block_on(futures_lite::future::or(
        async {
            let deadline = std::time::Instant::now() + Duration::from_millis(200);
            while seen.borrow().len() != 4 {
                assert!(
                    std::time::Instant::now() < deadline,
                    "only {} of4 devices were read",
                    seen.borrow().len()
                );
                async_io::Timer::after(Duration::from_millis(5)).await;
            }
            harness.state.stop();
        },
        async {
            harness.state.run().await;
            panic!("state actor returned before every device was attempted");
        },
    ));
}

type PausedReadCall = (StateReadRequest, flume::Sender<Vec<StateReadResult>>);

struct PausedReadTransport {
    calls: flume::Sender<PausedReadCall>,
}

struct ReportingReadTransport {
    calls: Rc<std::cell::RefCell<Vec<StateReadRequest>>>,
    values: Vec<(Property, u32, u32, WireValue)>,
    omit_initial: Option<Property>,
}

impl StateReadTransport for ReportingReadTransport {
    fn read(
        &self,
        request: StateReadRequest,
        _: Duration,
        guard: StateReadGuard,
    ) -> LocalBoxFuture<'static, Result<Vec<StateReadResult>, StateReadFailure>> {
        let calls = self.calls.clone();
        let values = self.values.clone();
        let omit_initial = self.omit_initial;
        async move {
            assert!(guard.permitted());
            let first = calls.borrow().is_empty();
            let results = request
                .targets
                .iter()
                .filter_map(|target| {
                    values.iter().find_map(|(property, siid, piid, value)| {
                        (*siid == target.siid
                            && *piid == target.piid
                            && !(first && omit_initial == Some(*property)))
                        .then(|| StateReadResult {
                            siid: *siid,
                            piid: *piid,
                            value: Some(value.clone()),
                        })
                    })
                })
                .collect();
            calls.borrow_mut().push(request);
            Ok(results)
        }
        .boxed_local()
    }
}

fn review_wire_values() -> Vec<(Property, u32, u32, WireValue)> {
    let descriptor = Harness::new().descriptor;
    [
        (Property::Power, WireValue::Boolean(true)),
        (Property::Brightness, WireValue::Integer(50)),
        (Property::ColorTemperature, WireValue::Integer(4000)),
    ]
    .into_iter()
    .map(|(property, value)| {
        let mapping = descriptor
            .properties
            .iter()
            .find(|mapping| mapping.property == property)
            .unwrap();
        (property, mapping.siid, mapping.piid, value)
    })
    .collect()
}

#[test]
fn frequent_covered_pushes_do_not_postpone_uncovered_property_polls() {
    let calls = Rc::new(std::cell::RefCell::new(Vec::new()));
    let mut harness = Harness::with_read_transport(
        StateLimits {
            poll_interval: Duration::from_millis(15),
            minimum_read_interval: Duration::from_millis(1),
            ..StateLimits::default()
        },
        false,
        Rc::new(ReportingReadTransport {
            calls: calls.clone(),
            values: review_wire_values(),
            omit_initial: None,
        }),
    );
    let feature = &mut harness.snapshot.features[0].runtime;
    let brightness = feature
        .descriptor
        .properties
        .iter_mut()
        .find(|mapping| mapping.property == Property::Brightness)
        .unwrap();
    brightness.notify = false;
    let brightness_target = (brightness.siid, brightness.piid);
    feature.authority_generation += 1;
    harness.state.reconcile(&harness.snapshot);
    let token = harness
        .state
        .select_push_source(&harness.feature.physical, PushSource::Gateway(99), 41, 1)
        .unwrap();
    assert!(harness.state.acknowledge(&token, 41, 1));
    let power = harness
        .descriptor
        .properties
        .iter()
        .find(|mapping| mapping.property == Property::Power)
        .unwrap();
    futures_lite::future::block_on(futures_lite::future::or(
        async {
            let deadline = std::time::Instant::now() + Duration::from_millis(100);
            while std::time::Instant::now() < deadline {
                assert!(harness.state.apply_property(
                    &token,
                    41,
                    1,
                    power.siid,
                    power.piid,
                    Some(&WireValue::Boolean(true)),
                    10,
                    false,
                ));
                async_io::Timer::after(Duration::from_millis(2)).await;
            }
            let polls = calls
                .borrow()
                .iter()
                .filter(|request| {
                    request
                        .targets
                        .iter()
                        .any(|target| (target.siid, target.piid) == brightness_target)
                })
                .count();
            assert!(
                polls >= 2,
                "the uncovered value was read only {polls} times"
            );
            harness.state.stop();
        },
        async {
            harness.state.run().await;
            panic!("state actor returned before polling was checked");
        },
    ));
}

#[test]
fn healthy_subscription_retries_a_missing_initial_property() {
    let calls = Rc::new(std::cell::RefCell::new(Vec::new()));
    let harness = Harness::with_read_transport(
        StateLimits {
            retry_initial: Duration::from_millis(5),
            retry_max: Duration::from_millis(10),
            minimum_read_interval: Duration::from_millis(1),
            ..StateLimits::default()
        },
        false,
        Rc::new(ReportingReadTransport {
            calls: calls.clone(),
            values: review_wire_values(),
            omit_initial: Some(Property::Brightness),
        }),
    );
    let token = harness
        .state
        .select_push_source(&harness.feature.physical, PushSource::Gateway(99), 41, 1)
        .unwrap();
    assert!(harness.state.acknowledge(&token, 41, 1));
    futures_lite::future::block_on(futures_lite::future::or(
        async {
            let deadline = std::time::Instant::now() + Duration::from_millis(150);
            loop {
                if matches!(
                    harness
                        .service
                        .snapshot(&harness.feature)
                        .unwrap()
                        .property(Property::Brightness),
                    Some(migate::device::PropertyState::Current { .. })
                ) {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "the missing initial value was never confirmed"
                );
                async_io::Timer::after(Duration::from_millis(2)).await;
            }
            assert!(calls.borrow().len() >= 2);
            harness.state.stop();
        },
        async {
            harness.state.run().await;
            panic!("state actor returned before the missing value was confirmed");
        },
    ));
}

async fn next_paused_read(calls: &flume::Receiver<PausedReadCall>) -> PausedReadCall {
    futures_lite::future::or(async { calls.recv_async().await.unwrap() }, async {
        async_io::Timer::after(Duration::from_millis(500)).await;
        panic!("the expected read was not dispatched");
    })
    .await
}

struct AcceptedCommandTransport {
    calls: Rc<std::cell::Cell<usize>>,
}

impl CommandTransport for AcceptedCommandTransport {
    fn send(
        &self,
        _: ControlPath,
        _: TransportCommand,
        _: Duration,
        guard: SendGuard,
    ) -> LocalBoxFuture<'static, Result<(), TransportFailure>> {
        let calls = self.calls.clone();
        async move {
            assert!(guard.permitted());
            calls.set(calls.get() + 1);
            guard.shared_state().mark_sent();
            Ok(())
        }
        .boxed_local()
    }
}

#[test]
fn gate_queue_time_uses_total_budget_before_local_attempt() {
    let harness = Harness::new();
    let token = harness
        .state
        .select_push_source(&harness.feature.physical, PushSource::Gateway(99), 41, 1)
        .unwrap();
    assert!(harness.state.acknowledge(&token, 41, 1));
    let power = harness
        .descriptor
        .properties
        .iter()
        .find(|mapping| mapping.property == Property::Power)
        .unwrap();
    assert!(harness.state.apply_property(
        &token,
        41,
        1,
        power.siid,
        power.piid,
        Some(&WireValue::Boolean(true)),
        10,
        false
    ));
    let calls = Rc::new(std::cell::Cell::new(0));
    let commands = CommandRuntime::with_limits(
        harness.service.clone(),
        Rc::new(AcceptedCommandTransport {
            calls: calls.clone(),
        }),
        CommandLimits {
            total: Duration::from_millis(300),
            local_attempt: Duration::from_millis(25),
            ..CommandLimits::default()
        },
    );
    commands.register(harness.snapshot.features[0].runtime.clone());
    let gate = commands.execution_gate();
    let held = futures_lite::future::block_on(gate.acquire(
        harness.feature.physical.clone(),
        std::time::Instant::now() + Duration::from_millis(500),
    ))
    .unwrap();
    let ticket = harness
        .service
        .command(&harness.feature, DeviceCommand::SetPower(false));
    futures_lite::future::block_on(futures_lite::future::or(
        async {
            async_io::Timer::after(Duration::from_millis(70)).await;
            drop(held);
            assert_eq!(ticket.await, CommandOutcome::Accepted);
            assert_eq!(calls.get(), 1);
            commands.stop();
        },
        async {
            commands.run().await;
            panic!("command runtime returned before its queued command completed");
        },
    ));
}

#[test]
fn removing_one_route_keeps_the_shared_session_authority_live() {
    use migate::xiaomi::{
        cloud::CloudClient,
        runtime::{CurrentSessionRegistry, SessionAuthority},
    };
    let harness = Harness::new();
    let authority = SessionAuthority::new();
    let registry = CurrentSessionRegistry::default();
    let client = Rc::new(CloudClient::new().unwrap());
    let first = harness.feature.physical.clone();
    let mut second = first.clone();
    second.parent_did = DeviceDid::new("1235").unwrap();
    registry.install_cloud(
        first.clone(),
        client.clone(),
        "synthetic-access",
        i64::MAX,
        authority.clone(),
    );
    registry.install_cloud(
        second.clone(),
        client.clone(),
        "synthetic-access",
        i64::MAX,
        authority.clone(),
    );
    registry.revoke_device(&first);
    assert!(
        authority.check(),
        "removing one device revoked the session shared by another device"
    );
    registry.install_cloud(
        second,
        client,
        "synthetic-access",
        i64::MAX,
        authority.clone(),
    );
    assert!(
        authority.check(),
        "reinstalling a route revoked its replacement's parent session"
    );
}

impl StateReadTransport for PausedReadTransport {
    fn read(
        &self,
        request: StateReadRequest,
        _: Duration,
        guard: StateReadGuard,
    ) -> LocalBoxFuture<'static, Result<Vec<StateReadResult>, StateReadFailure>> {
        let calls = self.calls.clone();
        async move {
            assert!(guard.permitted());
            let (reply, receive) = flume::bounded(1);
            calls.try_send((request, reply)).unwrap();
            receive
                .recv_async()
                .await
                .map_err(|_| StateReadFailure::Unavailable)
        }
        .boxed_local()
    }
}

#[test]
fn deferred_new_query_blocks_an_old_reply_while_capacity_is_full() {
    let (send, calls) = flume::bounded(8);
    let harness = Harness::with_read_transport(
        StateLimits {
            queue_capacity: 1,
            global_concurrency: 1,
            minimum_read_interval: Duration::from_millis(1),
            ..StateLimits::default()
        },
        false,
        Rc::new(PausedReadTransport { calls: send }),
    );
    let power = harness
        .descriptor
        .properties
        .iter()
        .find(|mapping| mapping.property == Property::Power)
        .unwrap();
    futures_lite::future::block_on(futures_lite::future::or(
        async {
            let (_, first) = next_paused_read(&calls).await;
            harness.state.request_refresh(&harness.feature.physical);
            first
                .send(vec![StateReadResult {
                    siid: power.siid,
                    piid: power.piid,
                    value: Some(WireValue::Boolean(true)),
                }])
                .unwrap();
            let (_, second) = next_paused_read(&calls).await;
            assert!(
                !matches!(
                    harness
                        .service
                        .snapshot(&harness.feature)
                        .unwrap()
                        .property(Property::Power),
                    Some(migate::device::PropertyState::Current { .. })
                ),
                "the older query was applied while its replacement was pending"
            );
            second
                .send(vec![StateReadResult {
                    siid: power.siid,
                    piid: power.piid,
                    value: Some(WireValue::Boolean(false)),
                }])
                .unwrap();
            let deadline = std::time::Instant::now() + Duration::from_millis(200);
            loop {
                if matches!(
                    harness
                        .service
                        .snapshot(&harness.feature)
                        .unwrap()
                        .property(Property::Power),
                    Some(migate::device::PropertyState::Current {
                        value: migate::device::PropertyValue::Power(false),
                        ..
                    })
                ) {
                    break;
                }
                assert!(std::time::Instant::now() < deadline);
                async_io::Timer::after(Duration::from_millis(1)).await;
            }
            harness.state.stop();
        },
        async {
            harness.state.run().await;
            panic!("state actor returned before the replacement query completed");
        },
    ));
}

#[test]
fn unrelated_control_route_change_keeps_healthy_gateway_notifications() {
    let mut harness = Harness::new();
    let token = harness
        .state
        .select_push_source(&harness.feature.physical, PushSource::Gateway(99), 41, 1)
        .unwrap();
    assert!(harness.state.acknowledge(&token, 41, 1));
    let power = harness
        .descriptor
        .properties
        .iter()
        .find(|mapping| mapping.property == Property::Power)
        .unwrap();
    assert!(harness.state.apply_property(
        &token,
        41,
        1,
        power.siid,
        power.piid,
        Some(&WireValue::Boolean(true)),
        10,
        false
    ));
    harness.snapshot.features[0].paths.cloud = true;
    harness.state.reconcile(&harness.snapshot);
    assert!(
        harness.state.apply_property(
            &token,
            41,
            1,
            power.siid,
            power.piid,
            Some(&WireValue::Boolean(false)),
            11,
            false
        ),
        "a healthy gateway push source must survive an unrelated cloud control route update"
    );
}

#[test]
fn cloud_offline_does_not_discard_authority_for_later_online_on_the_same_session() {
    let mut harness = Harness::new();
    harness.service.remove(&harness.feature);
    let document = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/miot_specs/miaomiaoce.sensor_ht.t2.json"
    ))
    .unwrap();
    let descriptor = compile_spec("miaomiaoce.sensor_ht.t2", &document)
        .unwrap()
        .features
        .into_iter()
        .find(|feature| feature.role == migate::device::FeatureRole::TemperatureSensor)
        .unwrap();
    harness.feature.service_instance = descriptor.service_instance;
    harness.feature.role = descriptor.role;
    harness.descriptor = descriptor.clone();
    Store::open(harness.directory.path())
        .unwrap()
        .devices()
        .allocate_feature(&harness.feature)
        .unwrap();
    harness.service.publish(
        harness.feature.clone(),
        "Temperature",
        descriptor.capabilities.clone(),
    );
    let feature = &mut harness.snapshot.features[0];
    feature.identity = harness.feature.clone();
    feature.runtime.identity = harness.feature.clone();
    feature.runtime.descriptor = descriptor;
    feature.paths = OperationPaths {
        cloud: true,
        ..OperationPaths::default()
    };
    feature.gateways.clear();
    harness.state.reconcile(&harness.snapshot);
    let token = harness
        .state
        .select_push_source(&harness.feature.physical, PushSource::Cloud, 71, 1)
        .unwrap();
    assert!(harness.state.acknowledge(&token, 71, 1));
    let temperature = harness
        .descriptor
        .properties
        .iter()
        .find(|mapping| mapping.property == Property::Temperature)
        .unwrap();
    assert!(harness.state.apply_property(
        &token,
        71,
        1,
        temperature.siid,
        temperature.piid,
        Some(&WireValue::Number(20.0)),
        10,
        false
    ));
    assert!(harness.service.is_available(&harness.feature));
    assert!(
        harness
            .state
            .apply_cloud_online(&token, 71, 1, false)
            .is_some()
    );
    harness.snapshot.features[0].paths.cloud = false;
    harness.state.reconcile(&harness.snapshot);
    assert!(
        !harness.service.is_available(&harness.feature),
        "an explicitly offline sensor must be unavailable even while its MQTT connection is healthy"
    );
    assert!(
        harness
            .state
            .apply_cloud_online(&token, 71, 1, true)
            .is_some(),
        "offline removes device reachability, not authority to receive its next online notification"
    );
    harness.snapshot.features[0].paths.cloud = true;
    harness.state.reconcile(&harness.snapshot);
    assert!(harness.state.apply_property(
        &token,
        71,
        1,
        temperature.siid,
        temperature.piid,
        Some(&WireValue::Number(21.0)),
        11,
        false
    ));
    assert!(harness.service.is_available(&harness.feature));
}

#[test]
fn cloud_push_can_confirm_sensor_without_a_control_route() {
    let mut harness = Harness::new();
    harness.service.remove(&harness.feature);
    let document = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/miot_specs/miaomiaoce.sensor_ht.t2.json"
    ))
    .unwrap();
    let descriptor = compile_spec("miaomiaoce.sensor_ht.t2", &document)
        .unwrap()
        .features
        .into_iter()
        .find(|feature| feature.role == migate::device::FeatureRole::TemperatureSensor)
        .unwrap();
    harness.feature.service_instance = descriptor.service_instance;
    harness.feature.role = descriptor.role;
    harness.descriptor = descriptor.clone();
    Store::open(harness.directory.path())
        .unwrap()
        .devices()
        .allocate_feature(&harness.feature)
        .unwrap();
    harness.service.publish(
        harness.feature.clone(),
        "Temperature",
        descriptor.capabilities.clone(),
    );
    let feature = &mut harness.snapshot.features[0];
    feature.identity = harness.feature.clone();
    feature.runtime.identity = harness.feature.clone();
    feature.runtime.descriptor = descriptor;
    feature.paths = OperationPaths::default();
    feature.gateways.clear();
    harness.state.reconcile(&harness.snapshot);
    let token = harness
        .state
        .select_push_source(&harness.feature.physical, PushSource::Cloud, 71, 1)
        .unwrap();
    assert!(harness.state.acknowledge(&token, 71, 1));
    let temperature = harness
        .descriptor
        .properties
        .iter()
        .find(|mapping| mapping.property == Property::Temperature)
        .unwrap();
    assert!(harness.state.apply_property(
        &token,
        71,
        1,
        temperature.siid,
        temperature.piid,
        Some(&WireValue::Number(20.0)),
        10,
        false
    ));
    assert!(
        harness.service.is_available(&harness.feature),
        "a confirmed sensor push does not require an independently available control/read route"
    );
}

#[test]
fn cloud_motion_events_confirm_reachability_without_a_control_route() {
    for keyed in [false, true] {
        let mut harness = Harness::new();
        harness.service.remove(&harness.feature);
        let document = include_str!("fixtures/miot_specs/xiaomi.motion.pir1.json");
        let descriptor = compile_spec("xiaomi.motion.pir1", document)
            .unwrap()
            .features
            .remove(0);
        harness.feature.service_instance = descriptor.service_instance;
        harness.feature.role = descriptor.role;
        Store::open(harness.directory.path())
            .unwrap()
            .devices()
            .allocate_feature(&harness.feature)
            .unwrap();
        harness.service.publish(
            harness.feature.clone(),
            "Motion",
            descriptor.capabilities.clone(),
        );
        let feature = &mut harness.snapshot.features[0];
        feature.identity = harness.feature.clone();
        feature.runtime.identity = harness.feature.clone();
        feature.runtime.descriptor = descriptor;
        feature.paths = OperationPaths::default();
        feature.gateways.clear();
        harness.state.reconcile(&harness.snapshot);
        let token = harness
            .state
            .select_push_source(&harness.feature.physical, PushSource::Cloud, 71, 1)
            .unwrap();
        assert!(harness.state.acknowledge(&token, 71, 1));
        assert!(!harness.service.is_available(&harness.feature));
        let applied = if keyed {
            harness.state.apply_keyed_event(
                &token,
                71,
                1,
                2,
                1008,
                &[(1005, WireValue::Number(100.0))],
                10,
                false,
            )
        } else {
            harness.state.apply_positional_event(
                &token,
                71,
                1,
                2,
                1008,
                &[WireValue::Number(100.0)],
                10,
                false,
            )
        };
        assert!(applied);
        assert!(matches!(
            harness
                .service
                .snapshot(&harness.feature)
                .unwrap()
                .property(Property::Motion),
            Some(migate::device::PropertyState::Current {
                value: migate::device::PropertyValue::Motion(true),
                ..
            })
        ));
        assert!(harness.service.is_available(&harness.feature));
    }
}
