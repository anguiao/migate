mod review_regressions;

use super::*;
use crate::{
    device::{AccountId, DeviceDid, FeatureRole, HomeId, PropertyState},
    storage::{Store, TokenSet, XiaomiRecord},
    xiaomi::catalog::compile_spec,
};
use flume::{Receiver, Sender};
use tempfile::TempDir;

use super::super::{
    AdmissionFeature, CommandRuntime, CommandTransport, GatewayPathEvidence, TransportCommand,
    TransportFailure,
};

type ReadReply = Sender<Result<Vec<StateReadResult>, StateReadFailure>>;
type ReadCall = (StateReadRequest, ReadReply);
type ActorHarness = (
    TempDir,
    DeviceService,
    StateRuntime,
    CommandRuntime,
    Receiver<ReadCall>,
    FeatureIdentity,
    FeatureDescriptor,
);

thread_local! {
    static COMMAND_SENDS: Cell<usize> = const { Cell::new(0) };
}

struct NoopCommandTransport;

impl CommandTransport for NoopCommandTransport {
    fn send(
        &self,
        _path: ControlPath,
        _command: TransportCommand,
        _timeout: Duration,
        guard: super::super::SendGuard,
    ) -> LocalBoxFuture<'static, Result<(), TransportFailure>> {
        async move {
            if !guard.permitted() {
                return Err(TransportFailure::Unavailable);
            }
            COMMAND_SENDS.with(|sends| sends.set(sends.get() + 1));
            guard.shared_state().mark_sent();
            Ok(())
        }
        .boxed_local()
    }
}

struct BlockingReadTransport {
    calls: Sender<ReadCall>,
}

impl StateReadTransport for BlockingReadTransport {
    fn read(
        &self,
        request: StateReadRequest,
        _timeout: Duration,
        guard: StateReadGuard,
    ) -> LocalBoxFuture<'static, Result<Vec<StateReadResult>, StateReadFailure>> {
        let (reply, receiver) = flume::bounded(1);
        self.calls.send((request, reply)).unwrap();
        async move {
            if !guard.permitted() {
                return Err(StateReadFailure::Unavailable);
            }
            receiver.recv_async().await.unwrap()
        }
        .boxed_local()
    }
}

fn credentials() -> XiaomiRecord {
    XiaomiRecord {
        uid: "10001".into(),
        region: "cn".into(),
        oauth_client_uuid: "123e4567-e89b-12d3-a456-426614174000".into(),
        redirect_uri: "http://127.0.0.1/callback".into(),
        tokens: TokenSet {
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            expires_at: 10_000,
            refresh_at: 5_000,
        },
        virtual_did: "123456789012345".into(),
        private_key_pem: "key".into(),
        certificate_pem: "cert".into(),
    }
}

fn actor() -> ActorHarness {
    actor_with_limits(StateLimits::default())
}

fn actor_with_limits(limits: StateLimits) -> ActorHarness {
    actor_with_descriptor(limits, descriptor())
}

fn actor_with_descriptor(mut limits: StateLimits, descriptor: FeatureDescriptor) -> ActorHarness {
    if limits.minimum_read_interval == StateLimits::default().minimum_read_interval {
        limits.minimum_read_interval = Duration::ZERO;
    }
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    store.xiaomi().replace(&credentials()).unwrap();
    let service = DeviceService::new();
    let feature = identity();
    store.devices().allocate_feature(&feature).unwrap();
    service.publish(
        feature.clone(),
        "Light",
        descriptor.definition.capabilities.clone(),
    );
    let command = CommandRuntime::new(service.clone(), Rc::new(NoopCommandTransport));
    let (calls, receiver) = flume::bounded(8);
    let runtime = StateRuntime::with_limits(
        service.clone(),
        store.devices(),
        Rc::new(BlockingReadTransport { calls }),
        command.execution_gate(),
        command.subscribe_completions(),
        limits,
    );
    let auth_generation = store.xiaomi().snapshot().unwrap().session_generation;
    let registered = RuntimeFeature {
        identity: feature.clone(),
        descriptor: descriptor.clone(),
        authority_generation: 1,
        auth_session_generation: auth_generation,
    };
    command.register(registered.clone());
    runtime.reconcile(&AdmissionSnapshot {
        binding: None,
        status: AdmissionStatus::Active,
        epoch: crate::xiaomi::discovery::NetworkEpoch::new(7),
        features: vec![AdmissionFeature {
            identity: feature.clone(),
            runtime: registered,
            paths: OperationPaths {
                gateway: true,
                ..OperationPaths::default()
            },
            gateways: vec![GatewayPathEvidence {
                gateway_did: 99,
                access: true,
                push: true,
                online: Some(true),
            }],
            lan_evidence: None,
        }],
    });
    (
        directory, service, runtime, command, receiver, feature, descriptor,
    )
}

#[test]
fn admitted_write_only_power_can_be_controlled_without_inventing_a_read() {
    COMMAND_SENDS.with(|sends| sends.set(0));
    let mut specification: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot_specs/yeelink.light.ml9.json"
    ))
    .unwrap();
    let power = specification["services"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|service| service["iid"] == 2)
        .unwrap()["properties"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|property| property["iid"] == 1)
        .unwrap();
    power["access"] = serde_json::json!(["write"]);
    let descriptor = compile_spec("yeelink.light.ml9", &specification.to_string())
        .unwrap()
        .features
        .into_iter()
        .next()
        .unwrap();
    let power = descriptor
        .binding
        .properties
        .iter()
        .find(|mapping| mapping.property == Property::Power)
        .unwrap();
    assert!(!power.readable);
    assert!(!power.notify);
    let (_directory, service, _runtime, command, _calls, feature, _descriptor) =
        actor_with_descriptor(StateLimits::default(), descriptor);

    assert!(
        service
            .snapshot(&feature)
            .unwrap()
            .property(Property::Power)
            .is_none()
    );
    assert!(service.is_available(&feature));
    future::block_on(async {
        let ticket = service.command(&feature, DeviceCommand::SetPower(true));
        command.run_until_idle().await;
        assert_eq!(ticket.await, CommandOutcome::Accepted);
    });
    assert_eq!(COMMAND_SENDS.with(Cell::get), 1);
    assert!(
        service
            .snapshot(&feature)
            .unwrap()
            .property(Property::Power)
            .is_none()
    );

    service.revoke(&feature);
    future::block_on(async {
        let ticket = service.command(&feature, DeviceCommand::SetPower(false));
        command.run_until_idle().await;
        assert_eq!(ticket.await, CommandOutcome::Unavailable);
    });
    assert_eq!(COMMAND_SENDS.with(Cell::get), 1);
}

#[test]
fn failed_transport_read_records_feature_path_and_safe_failure() {
    let (_directory, _service, runtime, _command, calls, feature, _descriptor) = actor();
    future::block_on(async {
        let run = runtime.run_until_idle();
        futures_util::pin_mut!(run);
        assert!(future::poll_once(&mut run).await.is_none());
        let (request, reply) = calls.recv_async().await.unwrap();
        assert_eq!(request.path, ControlPath::Gateway);
        reply.send(Err(StateReadFailure::Unavailable)).unwrap();
        run.await;
    });
    assert!(runtime.drain_diagnostics().iter().any(|diagnostic| {
        matches!(
            diagnostic,
            StateDiagnostic::ReadFailure {
                feature: actual,
                path: ControlPath::Gateway,
                failure: StateReadFailure::Unavailable,
            } if actual == &feature
        )
    }));
}

fn identity() -> FeatureIdentity {
    FeatureIdentity {
        physical: PhysicalDeviceId {
            account: AccountId::new("10001").unwrap(),
            home: HomeId::new("home-a").unwrap(),
            parent_did: DeviceDid::new("device-a").unwrap(),
        },
        service_instance: 2,
        role: FeatureRole::Light,
    }
}

fn descriptor() -> FeatureDescriptor {
    compile_spec(
        "yeelink.light.ml9",
        include_str!("../../../../tests/fixtures/miot_specs/yeelink.light.ml9.json"),
    )
    .unwrap()
    .features
    .into_iter()
    .next()
    .unwrap()
}

#[test]
fn healthy_subscription_still_reads_unconfirmed_notify_properties() {
    let (_directory, service, runtime, _command, calls, feature, descriptor) = actor();
    assert!(!service.is_available(&feature));
    let token = runtime
        .select_push_source(&feature.physical, PushSource::Gateway(99), 41, 1)
        .unwrap();
    assert!(runtime.acknowledge(&token, 41, 1));
    future::block_on(async {
        let run = runtime.run_until_idle();
        futures_util::pin_mut!(run);
        assert!(future::poll_once(&mut run).await.is_none());
        let (request, reply) = calls.recv_async().await.unwrap();
        assert!(request.targets.iter().any(|target| {
            descriptor.binding.properties.iter().any(|mapping| {
                mapping.property == Property::Power
                    && mapping.siid == target.siid
                    && mapping.piid == target.piid
            })
        }));
        let power = descriptor
            .binding
            .properties
            .iter()
            .find(|mapping| mapping.property == Property::Power)
            .unwrap();
        reply
            .send(Ok(vec![StateReadResult {
                siid: power.siid,
                piid: power.piid,
                value: Some(WireValue::Boolean(true)),
            }]))
            .unwrap();
        run.await;
    });
    assert!(matches!(
        service
            .snapshot(&feature)
            .unwrap()
            .property(Property::Power),
        Some(PropertyState::Current { .. })
    ));
    assert!(service.is_available(&feature));
}

#[test]
fn queued_new_query_blocks_older_result_before_second_read_starts() {
    let (_directory, service, runtime, _command, calls, feature, descriptor) = actor();
    future::block_on(async {
        let run = runtime.run_until_idle();
        futures_util::pin_mut!(run);
        assert!(future::poll_once(&mut run).await.is_none());
        let (_first, first_reply) = calls.recv_async().await.unwrap();
        runtime.request_refresh(&feature.physical);
        let power = descriptor
            .binding
            .properties
            .iter()
            .find(|mapping| mapping.property == Property::Power)
            .unwrap();
        first_reply
            .send(Ok(vec![StateReadResult {
                siid: power.siid,
                piid: power.piid,
                value: Some(WireValue::Boolean(false)),
            }]))
            .unwrap();
        assert!(future::poll_once(&mut run).await.is_none());
        assert!(
            service
                .snapshot(&feature)
                .unwrap()
                .property(Property::Power)
                .is_none()
        );
        let (_second, second_reply) = calls.recv_async().await.unwrap();
        second_reply
            .send(Ok(vec![StateReadResult {
                siid: power.siid,
                piid: power.piid,
                value: Some(WireValue::Boolean(true)),
            }]))
            .unwrap();
        run.await;
    });
    assert!(matches!(
        service
            .snapshot(&feature)
            .unwrap()
            .property(Property::Power),
        Some(PropertyState::Current {
            value: crate::device::PropertyValue::Power(true),
            ..
        })
    ));
}

#[test]
fn newer_push_wins_over_a_blocked_read_and_old_value_is_not_cached() {
    let (directory, service, runtime, _command, calls, feature, descriptor) = actor();
    let token = runtime
        .select_push_source(&feature.physical, PushSource::Gateway(99), 51, 1)
        .unwrap();
    assert!(runtime.acknowledge(&token, 51, 1));
    future::block_on(async {
        let run = runtime.run_until_idle();
        futures_util::pin_mut!(run);
        assert!(future::poll_once(&mut run).await.is_none());
        let (_request, reply) = calls.recv_async().await.unwrap();
        let power = descriptor
            .binding
            .properties
            .iter()
            .find(|mapping| mapping.property == Property::Power)
            .unwrap();
        assert!(runtime.apply_property(
            &token,
            51,
            1,
            power.siid,
            power.piid,
            Some(&WireValue::Boolean(true)),
            2,
            false,
        ));
        reply
            .send(Ok(vec![StateReadResult {
                siid: power.siid,
                piid: power.piid,
                value: Some(WireValue::Boolean(false)),
            }]))
            .unwrap();
        run.await;
    });
    assert!(matches!(
        service
            .snapshot(&feature)
            .unwrap()
            .property(Property::Power),
        Some(PropertyState::Current {
            value: crate::device::PropertyValue::Power(true),
            ..
        })
    ));
    let cached = runtime
        .inner
        .borrow()
        .dirty_states
        .values()
        .next()
        .unwrap()
        .clone();
    assert_eq!(cached.value, crate::device::PropertyValue::Power(true));
    runtime.flush_cache();
    let reopened = Store::open(directory.path()).unwrap();
    let states = reopened.devices().load_states().unwrap();
    assert_eq!(states.len(), 1);
    let restored = DeviceService::new();
    restored.restore(
        feature.clone(),
        "Light",
        descriptor.definition.capabilities.clone(),
    );
    restored.restore_last_known(StateReport::new(
        feature.clone(),
        states[0].report_version,
        StateSource::Cache,
        states[0].observed_at,
        [(states[0].property, states[0].value.clone())],
    ));
    assert!(matches!(
        restored
            .snapshot(&feature)
            .unwrap()
            .property(Property::Power),
        Some(PropertyState::LastKnown { .. })
    ));
}

#[test]
fn one_flush_persists_more_than_the_old_cache_limit_and_the_latest_value() {
    let (directory, service, runtime, _command, _calls, feature, descriptor) = actor();
    let mut expected = Vec::new();
    for index in 0..300 {
        let mut identity = feature.clone();
        identity.physical.parent_did = DeviceDid::new(format!("device-{index}")).unwrap();
        runtime.store.allocate_feature(&identity).unwrap();
        service.publish(
            identity.clone(),
            format!("Light {index}"),
            descriptor.definition.capabilities.clone(),
        );
        runtime.apply_confirmed(
            identity.clone(),
            Property::Power,
            crate::device::PropertyValue::Power(true),
            runtime.service.next_report_version(),
            StateSource::Gateway,
            10,
        );
        expected.push(identity);
    }
    runtime.apply_confirmed(
        expected[0].clone(),
        Property::Power,
        crate::device::PropertyValue::Power(false),
        runtime.service.next_report_version(),
        StateSource::Gateway,
        11,
    );

    runtime.flush_cache();
    let stored = Store::open(directory.path())
        .unwrap()
        .devices()
        .load_states()
        .unwrap();
    assert_eq!(stored.len(), 300);
    let latest = stored
        .iter()
        .find(|state| state.feature == expected[0])
        .unwrap();
    assert_eq!(latest.value, crate::device::PropertyValue::Power(false));
    assert_eq!(latest.observed_at, 11);
}

#[test]
fn logout_before_flush_preserves_the_latest_value_for_last_known_restore() {
    let (directory, service, runtime, _command, _calls, feature, descriptor) = actor();
    runtime.apply_confirmed(
        feature.clone(),
        Property::Power,
        crate::device::PropertyValue::Power(true),
        runtime.service.next_report_version(),
        StateSource::Cloud,
        42,
    );
    service.logout();
    runtime.reconcile(&AdmissionSnapshot {
        binding: None,
        status: AdmissionStatus::Unbound,
        epoch: crate::xiaomi::discovery::NetworkEpoch::new(7),
        features: vec![],
    });
    runtime.flush_cache();

    let states = Store::open(directory.path())
        .unwrap()
        .devices()
        .load_states()
        .unwrap();
    assert_eq!(states.len(), 1);
    assert_eq!(states[0].value, crate::device::PropertyValue::Power(true));
    let restored = DeviceService::new();
    restored.restore(feature.clone(), "Light", descriptor.definition.capabilities);
    restored.restore_last_known(StateReport::new(
        feature.clone(),
        states[0].report_version,
        StateSource::Cache,
        states[0].observed_at,
        [(states[0].property, states[0].value.clone())],
    ));
    assert!(matches!(
        restored
            .snapshot(&feature)
            .unwrap()
            .property(Property::Power),
        Some(PropertyState::LastKnown {
            value: crate::device::PropertyValue::Power(true),
            ..
        })
    ));
}

#[test]
fn removing_a_feature_discards_its_unflushed_state() {
    let (directory, service, runtime, _command, _calls, feature, _descriptor) = actor();
    runtime.apply_confirmed(
        feature.clone(),
        Property::Power,
        crate::device::PropertyValue::Power(true),
        runtime.service.next_report_version(),
        StateSource::Gateway,
        42,
    );
    service.remove(&feature);
    runtime.reconcile(&AdmissionSnapshot {
        binding: None,
        status: AdmissionStatus::Active,
        epoch: crate::xiaomi::discovery::NetworkEpoch::new(7),
        features: vec![],
    });
    runtime.flush_cache();

    assert!(
        Store::open(directory.path())
            .unwrap()
            .devices()
            .load_states()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn stopped_runtime_and_retained_reports_cannot_update_state() {
    let (_directory, service, runtime, _command, _calls, feature, descriptor) = actor();
    let old = runtime
        .select_push_source(&feature.physical, PushSource::Gateway(99), 61, 1)
        .unwrap();
    assert!(runtime.acknowledge(&old, 61, 1));
    let current = runtime
        .select_push_source(&feature.physical, PushSource::Cloud, 62, 1)
        .unwrap();
    assert!(runtime.acknowledge(&current, 62, 1));
    let power = descriptor
        .binding
        .properties
        .iter()
        .find(|mapping| mapping.property == Property::Power)
        .unwrap();
    assert!(!runtime.apply_property(
        &old,
        61,
        1,
        power.siid,
        power.piid,
        Some(&WireValue::Boolean(false)),
        2,
        false,
    ));
    assert!(!runtime.apply_property(
        &current,
        62,
        1,
        power.siid,
        power.piid,
        Some(&WireValue::Boolean(false)),
        2,
        true,
    ));
    runtime.stop();
    assert!(!runtime.apply_property(
        &current,
        62,
        1,
        power.siid,
        power.piid,
        Some(&WireValue::Boolean(true)),
        3,
        false,
    ));
    assert!(
        service
            .snapshot(&feature)
            .unwrap()
            .property(Property::Power)
            .is_none()
    );
}

#[test]
fn accepted_encoded_operation_schedules_only_its_actual_readback() {
    let (_directory, service, runtime, command, calls, feature, descriptor) = actor();
    let power = descriptor
        .binding
        .properties
        .iter()
        .find(|mapping| mapping.property == Property::Power)
        .unwrap();
    future::block_on(async {
        let initial = runtime.run_until_idle();
        futures_util::pin_mut!(initial);
        assert!(future::poll_once(&mut initial).await.is_none());
        let (_request, reply) = calls.recv_async().await.unwrap();
        reply
            .send(Ok(vec![StateReadResult {
                siid: power.siid,
                piid: power.piid,
                value: Some(WireValue::Boolean(true)),
            }]))
            .unwrap();
        initial.await;

        let ticket = service.command(
            &feature,
            DeviceCommand::SetBrightness(crate::device::Percent::new(0.0).unwrap()),
        );
        command.run_until_idle().await;
        assert_eq!(ticket.await, CommandOutcome::Accepted);

        let readback = runtime.run_until_idle();
        futures_util::pin_mut!(readback);
        assert!(future::poll_once(&mut readback).await.is_none());
        let (request, reply) = calls.recv_async().await.unwrap();
        assert_eq!(
            request.targets,
            vec![ReadTarget {
                siid: power.siid,
                piid: power.piid,
            }]
        );
        reply.send(Ok(Vec::new())).unwrap();
        readback.await;
    });
}

#[test]
fn read_and_control_share_the_physical_device_execution_gate() {
    let (_directory, service, runtime, command, calls, feature, _descriptor) = actor();
    service.set_state_availability(&feature, true);
    future::block_on(async {
        let read_run = runtime.run_until_idle();
        futures_util::pin_mut!(read_run);
        assert!(future::poll_once(&mut read_run).await.is_none());
        let (_request, read_reply) = calls.recv_async().await.unwrap();

        let ticket = service.command(&feature, DeviceCommand::SetPower(true));
        futures_util::pin_mut!(ticket);
        let command_run = command.run_until_idle();
        futures_util::pin_mut!(command_run);
        assert!(future::poll_once(&mut command_run).await.is_none());
        assert!(future::poll_once(&mut ticket).await.is_none());

        read_reply.send(Ok(Vec::new())).unwrap();
        read_run.await;
        command_run.await;
        assert_eq!(ticket.await, CommandOutcome::Accepted);
    });
}

#[test]
fn stop_revokes_a_blocked_read_and_retires_the_request() {
    let (_directory, _service, runtime, _command, calls, _feature, _descriptor) = actor();
    future::block_on(async {
        let run = runtime.run();
        futures_util::pin_mut!(run);
        assert!(future::poll_once(&mut run).await.is_none());
        let (_request, reply) = calls.recv_async().await.unwrap();
        runtime.stop();
        run.await;
        assert!(reply.send(Ok(Vec::new())).is_err());
        assert!(runtime.inner.borrow().active.is_empty());
        assert!(runtime.inner.borrow().pending.is_empty());
    });
}

#[test]
fn dropping_a_persistent_runner_finally_revokes_state_work() {
    let (_directory, _service, runtime, _command, calls, feature, _descriptor) = actor();
    future::block_on(async {
        let mut run = Box::pin(runtime.run());
        assert!(future::poll_once(&mut run).await.is_none());
        let (_request, reply) = calls.recv_async().await.unwrap();
        drop(run);
        assert!(reply.send(Ok(Vec::new())).is_err());
        assert!(
            runtime
                .select_push_source(&feature.physical, PushSource::Gateway(99), 81, 1)
                .is_none()
        );
        assert!(runtime.inner.borrow().active.is_empty());
    });
}

#[test]
fn gate_blocked_readback_expires_without_starting_transport() {
    let (_directory, _service, runtime, command, calls, feature, descriptor) = actor();
    let power = descriptor
        .binding
        .properties
        .iter()
        .find(|mapping| mapping.property == Property::Power)
        .unwrap();
    let targets = BTreeSet::from([(power.siid, power.piid)]);
    future::block_on(async {
        let lease = command
            .execution_gate()
            .acquire(
                feature.physical.clone(),
                Instant::now() + Duration::from_secs(1),
            )
            .await
            .unwrap();
        {
            let mut data = runtime.inner.borrow_mut();
            data.pending.clear();
            data.pending_order.clear();
        }
        runtime.schedule_selected(
            &feature.physical,
            false,
            true,
            Some(&targets),
            Some(Instant::now() + Duration::from_millis(2)),
        );
        runtime.run_until_idle().await;
        assert!(calls.try_recv().is_err());
        drop(lease);
    });
}

#[test]
fn gate_wait_drops_only_expired_targets_from_a_mixed_readback() {
    let (_directory, _service, runtime, command, calls, feature, descriptor) = actor();
    let power = descriptor
        .binding
        .properties
        .iter()
        .find(|mapping| mapping.property == Property::Power)
        .unwrap();
    let brightness = descriptor
        .binding
        .properties
        .iter()
        .find(|mapping| mapping.property == Property::Brightness)
        .unwrap();
    future::block_on(async {
        let lease = command
            .execution_gate()
            .acquire(
                feature.physical.clone(),
                Instant::now() + Duration::from_secs(1),
            )
            .await
            .unwrap();
        {
            let mut data = runtime.inner.borrow_mut();
            data.pending.clear();
            data.pending_order.clear();
        }
        runtime.schedule_selected(
            &feature.physical,
            false,
            true,
            Some(&BTreeSet::from([(power.siid, power.piid)])),
            Some(Instant::now() + Duration::from_millis(2)),
        );
        runtime.schedule_selected(
            &feature.physical,
            false,
            true,
            Some(&BTreeSet::from([(brightness.siid, brightness.piid)])),
            Some(Instant::now() + Duration::from_millis(100)),
        );
        let run = runtime.run_until_idle();
        futures_util::pin_mut!(run);
        assert!(future::poll_once(&mut run).await.is_none());
        Timer::after(Duration::from_millis(5)).await;
        drop(lease);
        assert!(future::poll_once(&mut run).await.is_none());
        let (request, reply) = calls.recv_async().await.unwrap();
        assert_eq!(
            request.targets,
            vec![ReadTarget {
                siid: brightness.siid,
                piid: brightness.piid,
            }]
        );
        reply.send(Ok(Vec::new())).unwrap();
        run.await;
    });
}

#[test]
fn core_success_does_not_postpone_battery_cadence_or_gate_availability() {
    let (_directory, service, runtime, _command, calls, feature, descriptor) = actor();
    let mut battery = descriptor
        .binding
        .properties
        .iter()
        .find(|mapping| mapping.property == Property::Brightness)
        .unwrap()
        .clone();
    battery.property = Property::Battery;
    battery.piid += 100;
    battery.class = PropertyClass::Ancillary;
    battery.notify = false;
    {
        let mut data = runtime.inner.borrow_mut();
        data.features
            .get_mut(&feature)
            .unwrap()
            .descriptor
            .binding
            .properties
            .push(battery.clone());
        data.pending.clear();
        data.pending_order.clear();
    }
    runtime.schedule(&feature.physical, true);
    future::block_on(async {
        let first = runtime.run_until_idle();
        futures_util::pin_mut!(first);
        assert!(future::poll_once(&mut first).await.is_none());
        let (request, reply) = calls.recv_async().await.unwrap();
        assert!(request.targets.contains(&ReadTarget {
            siid: battery.siid,
            piid: battery.piid,
        }));
        let power = descriptor
            .binding
            .properties
            .iter()
            .find(|mapping| mapping.property == Property::Power)
            .unwrap();
        reply
            .send(Ok(vec![
                StateReadResult {
                    siid: power.siid,
                    piid: power.piid,
                    value: Some(WireValue::Boolean(true)),
                },
                StateReadResult {
                    siid: battery.siid,
                    piid: battery.piid,
                    value: None,
                },
            ]))
            .unwrap();
        first.await;
        assert!(service.is_available(&feature));
        let battery_deadline = runtime
            .inner
            .borrow()
            .next_battery
            .get(&feature.physical)
            .copied()
            .unwrap();

        runtime.schedule(&feature.physical, false);
        let core = runtime.run_until_idle();
        futures_util::pin_mut!(core);
        assert!(future::poll_once(&mut core).await.is_none());
        let (request, reply) = calls.recv_async().await.unwrap();
        assert!(!request.targets.contains(&ReadTarget {
            siid: battery.siid,
            piid: battery.piid,
        }));
        reply.send(Ok(Vec::new())).unwrap();
        core.await;
        assert_eq!(
            runtime.inner.borrow().next_battery.get(&feature.physical),
            Some(&battery_deadline)
        );
    });
}

#[test]
fn healthy_covered_property_is_suppressed_while_uncovered_property_polls() {
    let (_directory, _service, runtime, _command, calls, feature, descriptor) = actor();
    let power = descriptor
        .binding
        .properties
        .iter()
        .find(|mapping| mapping.property == Property::Power)
        .unwrap()
        .clone();
    let mut brightness = descriptor
        .binding
        .properties
        .iter()
        .find(|mapping| mapping.property == Property::Brightness)
        .unwrap()
        .clone();
    brightness.notify = false;
    {
        let mut data = runtime.inner.borrow_mut();
        data.features
            .get_mut(&feature)
            .unwrap()
            .descriptor
            .binding
            .properties = vec![power.clone(), brightness.clone()];
        data.pending.clear();
        data.pending_order.clear();
    }
    let token = runtime
        .select_push_source(&feature.physical, PushSource::Gateway(99), 91, 1)
        .unwrap();
    assert!(runtime.acknowledge(&token, 91, 1));
    {
        let mut data = runtime.inner.borrow_mut();
        data.pending.clear();
        data.pending_order.clear();
    }
    assert!(runtime.apply_property(
        &token,
        91,
        1,
        power.siid,
        power.piid,
        Some(&WireValue::Boolean(true)),
        10,
        false,
    ));
    assert!(runtime.apply_property(
        &token,
        91,
        1,
        brightness.siid,
        brightness.piid,
        Some(&WireValue::Integer(50)),
        10,
        false,
    ));
    let poll_deadline = runtime
        .inner
        .borrow()
        .next_due
        .get(&feature.physical)
        .copied()
        .unwrap();
    assert!(runtime.apply_property(
        &token,
        91,
        1,
        power.siid,
        power.piid,
        Some(&WireValue::Boolean(false)),
        11,
        false,
    ));
    assert_eq!(
        runtime.inner.borrow().next_due.get(&feature.physical),
        Some(&poll_deadline)
    );
    runtime.schedule(&feature.physical, false);
    future::block_on(async {
        let run = runtime.run_until_idle();
        futures_util::pin_mut!(run);
        assert!(future::poll_once(&mut run).await.is_none());
        let (request, reply) = calls.recv_async().await.unwrap();
        assert_eq!(
            request.targets,
            vec![ReadTarget {
                siid: brightness.siid,
                piid: brightness.piid,
            }]
        );
        reply.send(Ok(Vec::new())).unwrap();
        run.await;
    });
}

#[test]
fn partial_success_with_a_healthy_source_retries_unconfirmed_notify_values() {
    let limits = StateLimits {
        retry_initial: Duration::from_millis(2),
        retry_max: Duration::from_millis(2),
        ..StateLimits::default()
    };
    let (_directory, _service, runtime, _command, calls, feature, descriptor) =
        actor_with_limits(limits);
    let token = runtime
        .select_push_source(&feature.physical, PushSource::Gateway(99), 101, 1)
        .unwrap();
    assert!(runtime.acknowledge(&token, 101, 1));
    let power = descriptor
        .binding
        .properties
        .iter()
        .find(|mapping| mapping.property == Property::Power)
        .unwrap();
    future::block_on(async {
        let run = runtime.run();
        futures_util::pin_mut!(run);
        assert!(future::poll_once(&mut run).await.is_none());
        let (_request, reply) = calls.recv_async().await.unwrap();
        reply
            .send(Ok(vec![StateReadResult {
                siid: power.siid,
                piid: power.piid,
                value: Some(WireValue::Boolean(true)),
            }]))
            .unwrap();
        assert!(future::poll_once(&mut run).await.is_none());
        Timer::after(Duration::from_millis(4)).await;
        assert!(future::poll_once(&mut run).await.is_none());
        let (_retry, reply) = calls.recv_async().await.unwrap();
        reply.send(Ok(Vec::new())).unwrap();
        runtime.stop();
        run.await;
    });
}

#[test]
fn batches_smaller_than_the_property_set_eventually_read_every_target() {
    let limits = StateLimits {
        batch_size: 1,
        ..StateLimits::default()
    };
    let (_directory, _service, runtime, _command, calls, _feature, descriptor) =
        actor_with_limits(limits);
    let expected = descriptor
        .binding
        .properties
        .iter()
        .filter(|mapping| mapping.readable)
        .map(|mapping| (mapping.siid, mapping.piid))
        .collect::<BTreeSet<_>>();
    future::block_on(async {
        let run = runtime.run_until_idle();
        futures_util::pin_mut!(run);
        let mut observed = BTreeSet::new();
        while observed.len() < expected.len() {
            assert!(future::poll_once(&mut run).await.is_none());
            let (request, reply) = calls.recv_async().await.unwrap();
            assert_eq!(request.targets.len(), 1);
            observed.insert((request.targets[0].siid, request.targets[0].piid));
            reply.send(Ok(Vec::new())).unwrap();
        }
        run.await;
        assert_eq!(observed, expected);
    });
}

#[test]
fn failed_read_uses_retry_timer_and_success_resets_backoff() {
    let limits = StateLimits {
        retry_initial: Duration::from_millis(2),
        retry_max: Duration::from_millis(8),
        poll_interval: Duration::from_secs(60),
        ..StateLimits::default()
    };
    let (_directory, _service, runtime, _command, calls, feature, descriptor) =
        actor_with_limits(limits);
    future::block_on(async {
        let run = runtime.run();
        futures_util::pin_mut!(run);
        assert!(future::poll_once(&mut run).await.is_none());
        let (_first, reply) = calls.recv_async().await.unwrap();
        reply.send(Err(StateReadFailure::Unavailable)).unwrap();
        assert!(future::poll_once(&mut run).await.is_none());
        Timer::after(Duration::from_millis(4)).await;
        assert!(future::poll_once(&mut run).await.is_none());
        let (retry, reply) = calls.recv_async().await.unwrap();
        let results = retry
            .targets
            .iter()
            .map(|target| {
                let property = descriptor
                    .binding
                    .properties
                    .iter()
                    .find(|mapping| mapping.siid == target.siid && mapping.piid == target.piid)
                    .unwrap()
                    .property;
                let value = match property {
                    Property::Power => WireValue::Boolean(true),
                    Property::Brightness => WireValue::Integer(50),
                    Property::ColorTemperature => WireValue::Integer(4_000),
                    _ => panic!("unexpected light property {property:?}"),
                };
                StateReadResult {
                    siid: target.siid,
                    piid: target.piid,
                    value: Some(value),
                }
            })
            .collect();
        reply.send(Ok(results)).unwrap();
        assert!(future::poll_once(&mut run).await.is_none());
        assert!(
            !runtime
                .inner
                .borrow()
                .retry_attempts
                .contains_key(&feature.physical)
        );
        runtime.stop();
        run.await;
    });
}

#[test]
fn repeated_refreshes_coalesce_into_one_pending_round() {
    let (_directory, _service, runtime, _command, calls, feature, _descriptor) = actor();
    future::block_on(async {
        let run = runtime.run_until_idle();
        futures_util::pin_mut!(run);
        assert!(future::poll_once(&mut run).await.is_none());
        let (_first, first_reply) = calls.recv_async().await.unwrap();
        for _ in 0..100 {
            runtime.request_refresh(&feature.physical);
        }
        assert_eq!(runtime.inner.borrow().pending.len(), 1);
        first_reply.send(Ok(Vec::new())).unwrap();
        assert!(future::poll_once(&mut run).await.is_none());
        let (_second, second_reply) = calls.recv_async().await.unwrap();
        second_reply.send(Ok(Vec::new())).unwrap();
        run.await;
        assert!(calls.try_recv().is_err());
    });
}

#[test]
fn positional_event_is_normalized_and_retained_action_is_discarded() {
    let (directory, service, runtime, _command, _calls, light, _descriptor) = actor();
    let mut descriptor = compile_spec(
        "xiaomi.motion.pir1",
        include_str!("../../../../tests/fixtures/miot_specs/xiaomi.motion.pir1.json"),
    )
    .unwrap()
    .features
    .into_iter()
    .find(|feature| feature.definition.role == FeatureRole::MotionSensor)
    .unwrap();
    descriptor
        .binding
        .properties
        .retain(|mapping| mapping.property != Property::Motion);
    let feature = FeatureIdentity {
        physical: light.physical.clone(),
        service_instance: descriptor.definition.service_instance,
        role: descriptor.definition.role,
    };
    runtime.store.allocate_feature(&feature).unwrap();
    service.publish(
        feature.clone(),
        "Motion",
        descriptor.definition.capabilities.clone(),
    );
    let template = runtime.inner.borrow().features[&light].clone();
    let registered = RuntimeFeature {
        identity: feature.clone(),
        descriptor: descriptor.clone(),
        ..template
    };
    runtime
        .inner
        .borrow_mut()
        .features
        .insert(feature.clone(), registered.clone());
    let token = runtime
        .select_push_source(&feature.physical, PushSource::Gateway(99), 81, 1)
        .unwrap();
    assert!(runtime.acknowledge(&token, 81, 1));
    let event = descriptor
        .binding
        .events
        .iter()
        .find(|event| event.argument_count == 0)
        .unwrap();
    assert!(!runtime.apply_positional_event(&token, 81, 1, event.siid, event.eiid, &[], 1, true,));
    assert!(runtime.apply_positional_event(&token, 81, 1, event.siid, event.eiid, &[], 2, false,));
    assert!(matches!(
        service
            .snapshot(&feature)
            .unwrap()
            .property(Property::Motion),
        Some(PropertyState::Current {
            value: crate::device::PropertyValue::Motion(false),
            ..
        })
    ));
    runtime.reconcile(&AdmissionSnapshot {
        binding: None,
        status: AdmissionStatus::Active,
        epoch: crate::xiaomi::discovery::NetworkEpoch::new(7),
        features: vec![AdmissionFeature {
            identity: feature.clone(),
            runtime: registered,
            paths: OperationPaths {
                gateway: true,
                ..OperationPaths::default()
            },
            gateways: vec![GatewayPathEvidence {
                gateway_did: 99,
                access: true,
                push: true,
                online: Some(true),
            }],
            lan_evidence: None,
        }],
    });
    runtime.flush_cache();
    let stored = Store::open(directory.path())
        .unwrap()
        .devices()
        .load_states()
        .unwrap();
    assert!(stored.iter().any(|state| {
        state.feature == feature
            && state.property == Property::Motion
            && state.value == crate::device::PropertyValue::Motion(false)
    }));
}
