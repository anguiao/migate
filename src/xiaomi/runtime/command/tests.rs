use super::*;
use crate::{
    device::{AccountId, DeviceDid, FeatureRole, HomeId, Percent},
    storage::{Store, TokenSet, XiaomiRecord},
    xiaomi::catalog::compile_spec,
};
use futures_util::FutureExt;
use tempfile::TempDir;

#[derive(Clone, Copy)]
enum Behavior {
    Accept,
    Reject,
    WaitBeforeWrite,
    WaitAfterWrite,
}

struct FakeTransport {
    behavior: Cell<Behavior>,
    calls: Rc<RefCell<Vec<(ControlPath, TransportCommand)>>>,
}

type TestSetup = (
    TempDir,
    Store,
    DeviceService,
    CommandRuntime,
    Rc<RefCell<Vec<(ControlPath, TransportCommand)>>>,
);

struct BlockingDeviceTransport {
    blocked_did: String,
    calls: Rc<RefCell<Vec<TransportCommand>>>,
}

struct TwoStepDeadlineTransport {
    calls: Rc<Cell<usize>>,
}

struct HeldReplyTransport {
    calls: Rc<RefCell<Vec<TransportCommand>>>,
    delivered: flume::Sender<()>,
    release: flume::Receiver<()>,
}

impl CommandTransport for HeldReplyTransport {
    fn send(
        &self,
        _path: ControlPath,
        command: TransportCommand,
        _timeout: Duration,
        guard: SendGuard,
    ) -> LocalBoxFuture<'static, Result<(), TransportFailure>> {
        let calls = self.calls.clone();
        let delivered = self.delivered.clone();
        let release = self.release.clone();
        async move {
            guard.shared_state().mark_sent();
            calls.borrow_mut().push(command);
            delivered.send_async(()).await.unwrap();
            release.recv_async().await.unwrap();
            Ok(())
        }
        .boxed_local()
    }
}

impl CommandTransport for TwoStepDeadlineTransport {
    fn send(
        &self,
        _path: ControlPath,
        _command: TransportCommand,
        timeout: Duration,
        guard: SendGuard,
    ) -> LocalBoxFuture<'static, Result<(), TransportFailure>> {
        let step = self.calls.get();
        self.calls.set(step + 1);
        async move {
            if !guard.permitted() {
                return Err(TransportFailure::Unavailable);
            }
            if step == 0 {
                guard.shared_state().mark_sent();
                Ok(())
            } else {
                std::thread::sleep(timeout + Duration::from_millis(2));
                if guard.permitted() {
                    guard.shared_state().mark_sent();
                }
                Err(TransportFailure::Unavailable)
            }
        }
        .boxed_local()
    }
}

impl CommandTransport for BlockingDeviceTransport {
    fn send(
        &self,
        _path: ControlPath,
        command: TransportCommand,
        _timeout: Duration,
        guard: SendGuard,
    ) -> LocalBoxFuture<'static, Result<(), TransportFailure>> {
        let blocked = command.device.parent_did.as_str() == self.blocked_did
            && command.typed == DeviceCommand::SetPower(true);
        let calls = self.calls.clone();
        async move {
            if !guard.permitted() {
                return Err(TransportFailure::Unavailable);
            }
            guard.shared_state().mark_sent();
            calls.borrow_mut().push(command);
            if blocked {
                guard.cancelled().await;
                Err(TransportFailure::Ambiguous)
            } else {
                Ok(())
            }
        }
        .boxed_local()
    }
}

impl CommandTransport for FakeTransport {
    fn send(
        &self,
        path: ControlPath,
        command: TransportCommand,
        _timeout: Duration,
        guard: SendGuard,
    ) -> LocalBoxFuture<'static, Result<(), TransportFailure>> {
        let behavior = self.behavior.get();
        let calls = self.calls.clone();
        async move {
            if !guard.permitted() {
                return Err(TransportFailure::Unavailable);
            }
            match behavior {
                Behavior::WaitBeforeWrite => future::pending().await,
                Behavior::Accept | Behavior::Reject | Behavior::WaitAfterWrite => {
                    guard.shared_state().mark_sent();
                }
            }
            calls.borrow_mut().push((path, command));
            match behavior {
                Behavior::Accept => Ok(()),
                Behavior::Reject => Err(TransportFailure::Rejected(401)),
                Behavior::WaitAfterWrite => future::pending().await,
                Behavior::WaitBeforeWrite => unreachable!(),
            }
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
        private_key_pem: "-----BEGIN PRIVATE KEY-----\nkey\n-----END PRIVATE KEY-----".into(),
        certificate_pem: "-----BEGIN CERTIFICATE-----\ncert\n-----END CERTIFICATE-----".into(),
    }
}

fn identity(did: &str, siid: u32) -> FeatureIdentity {
    FeatureIdentity {
        physical: PhysicalDeviceId {
            account: AccountId::new("10001").unwrap(),
            home: HomeId::new("home-a").unwrap(),
            parent_did: DeviceDid::new(did).unwrap(),
        },
        service_instance: siid,
        role: FeatureRole::Light,
    }
}

fn light_descriptor() -> FeatureDescriptor {
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

fn setup(behavior: Behavior, limits: CommandLimits) -> TestSetup {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    store.xiaomi().replace(&credentials()).unwrap();
    let service = DeviceService::new();
    let calls = Rc::new(RefCell::new(Vec::new()));
    let transport = Rc::new(FakeTransport {
        behavior: Cell::new(behavior),
        calls: calls.clone(),
    });
    let runtime = CommandRuntime::with_limits(service.clone(), transport, limits);
    (directory, store, service, runtime, calls)
}

fn register_light(
    service: &DeviceService,
    runtime: &CommandRuntime,
    feature: FeatureIdentity,
    _paths: OperationPaths,
) {
    let descriptor = light_descriptor();
    service.publish(
        feature.clone(),
        descriptor.definition.name.clone(),
        descriptor.definition.capabilities.clone(),
    );
    runtime.register(RuntimeFeature {
        identity: feature,
        descriptor,
        authority_generation: 3,
        auth_session_generation: {
            let directory = tempfile::tempdir().unwrap();
            Store::open(directory.path())
                .unwrap()
                .xiaomi()
                .snapshot()
                .unwrap()
                .session_generation
        },
    });
}

#[test]
fn exact_path_is_selected_once_and_rejection_is_not_replayed() {
    let limits = CommandLimits::default();
    let (_directory, _store, service, runtime, calls) = setup(Behavior::Reject, limits);
    let feature = identity("device-a", 2);
    register_light(
        &service,
        &runtime,
        feature.clone(),
        OperationPaths {
            gateway: true,
            lan: true,
            cloud: true,
        },
    );
    let ticket = service.command(&feature, DeviceCommand::SetPower(true));
    future::block_on(runtime.run_until_idle());
    assert_eq!(future::block_on(ticket), CommandOutcome::Rejected(401));
    assert_eq!(calls.borrow().len(), 1);
    assert_eq!(calls.borrow()[0].0, ControlPath::Gateway);
    assert_eq!(calls.borrow()[0].1.typed, DeviceCommand::SetPower(true));
}

#[test]
fn generations_are_scoped_to_feature_and_do_not_cross_action_boundaries() {
    let (_directory, _store, service, runtime, calls) =
        setup(Behavior::Accept, CommandLimits::default());
    let first = identity("device-a", 2);
    let second = identity("device-a", 3);
    register_light(
        &service,
        &runtime,
        first.clone(),
        OperationPaths {
            gateway: true,
            ..OperationPaths::default()
        },
    );
    register_light(
        &service,
        &runtime,
        second.clone(),
        OperationPaths {
            gateway: true,
            ..OperationPaths::default()
        },
    );
    let first_ticket = service.command(
        &first,
        DeviceCommand::SetBrightness(Percent::new(25.0).unwrap()),
    );
    let second_ticket = service.command(&second, DeviceCommand::SetPower(false));
    future::block_on(runtime.run_until_idle());
    assert_eq!(future::block_on(first_ticket), CommandOutcome::Accepted);
    assert_eq!(future::block_on(second_ticket), CommandOutcome::Accepted);
    assert_eq!(calls.borrow().len(), 2);
}

#[test]
fn newer_off_supersedes_unsent_followup_and_dropped_ticket_cancels_queue_entry() {
    let (_directory, _store, service, runtime, calls) =
        setup(Behavior::Accept, CommandLimits::default());
    let feature = identity("device-a", 2);
    register_light(
        &service,
        &runtime,
        feature.clone(),
        OperationPaths {
            gateway: true,
            ..OperationPaths::default()
        },
    );
    let obsolete = service.command(
        &feature,
        DeviceCommand::SetBrightness(Percent::new(25.0).unwrap()),
    );
    let off = service.command(&feature, DeviceCommand::SetPower(false));
    let dropped = service.command(&feature, DeviceCommand::SetPower(true));
    drop(dropped);
    future::block_on(runtime.run_until_idle());
    assert_eq!(future::block_on(obsolete), CommandOutcome::Superseded);
    assert_eq!(future::block_on(off), CommandOutcome::Accepted);
    assert_eq!(calls.borrow().len(), 1);
    assert_eq!(calls.borrow()[0].1.typed, DeviceCommand::SetPower(false));
}

#[test]
fn retained_lighting_intent_is_superseded_by_a_newer_off_before_transport() {
    let (_directory, _store, service, runtime, calls) =
        setup(Behavior::Accept, CommandLimits::default());
    let feature = identity("device-a", 2);
    register_light(
        &service,
        &runtime,
        feature.clone(),
        OperationPaths {
            gateway: true,
            ..OperationPaths::default()
        },
    );
    let intent = service.begin_command_intent(&feature, [Property::Brightness]);
    let old_step = intent.command_batch(vec![DeviceCommand::SetBrightness(
        Percent::new(25.0).unwrap(),
    )]);
    let off = service.command(&feature, DeviceCommand::SetPower(false));
    assert!(!intent.is_current());
    future::block_on(runtime.run_until_idle());
    assert_eq!(future::block_on(old_step), CommandOutcome::Superseded);
    assert_eq!(future::block_on(off), CommandOutcome::Accepted);
    assert_eq!(calls.borrow().len(), 1);
    assert_eq!(calls.borrow()[0].1.typed, DeviceCommand::SetPower(false));
}

#[test]
fn timeout_before_write_is_expired_and_after_write_is_ambiguous() {
    let limits = CommandLimits {
        total: Duration::from_millis(30),
        local_attempt: Duration::from_millis(30),
        ..CommandLimits::default()
    };
    for (behavior, expected) in [
        (Behavior::WaitBeforeWrite, CommandOutcome::Expired),
        (Behavior::WaitAfterWrite, CommandOutcome::Ambiguous),
    ] {
        let (_directory, _store, service, runtime, _calls) = setup(behavior, limits);
        let feature = identity("device-a", 2);
        register_light(
            &service,
            &runtime,
            feature.clone(),
            OperationPaths {
                gateway: true,
                ..OperationPaths::default()
            },
        );
        let ticket = service.command(&feature, DeviceCommand::SetPower(true));
        future::block_on(runtime.run_until_idle());
        assert_eq!(future::block_on(ticket), expected);
    }
}

#[test]
fn direct_storage_change_does_not_replace_the_runtime_dispatch_owner() {
    let (_directory, store, service, runtime, calls) =
        setup(Behavior::Accept, CommandLimits::default());
    let feature = identity("device-a", 2);
    register_light(
        &service,
        &runtime,
        feature.clone(),
        OperationPaths {
            gateway: true,
            ..OperationPaths::default()
        },
    );
    let ticket = service.command(&feature, DeviceCommand::SetPower(true));
    store.xiaomi().logout().unwrap();
    store.xiaomi().replace(&credentials()).unwrap();
    future::block_on(runtime.run_until_idle());
    assert_eq!(future::block_on(ticket), CommandOutcome::Accepted);
    assert_eq!(calls.borrow().len(), 1);
}

#[test]
fn persistent_runner_wakes_for_new_work_and_stops_cleanly() {
    let (_directory, _store, service, runtime, calls) =
        setup(Behavior::Accept, CommandLimits::default());
    let feature = identity("device-a", 2);
    register_light(
        &service,
        &runtime,
        feature.clone(),
        OperationPaths {
            gateway: true,
            ..OperationPaths::default()
        },
    );
    future::block_on(async {
        future::zip(runtime.run(), async {
            Timer::after(Duration::from_millis(1)).await;
            runtime.run().await;
            let ticket = service.command(&feature, DeviceCommand::SetPower(true));
            assert_eq!(ticket.await, CommandOutcome::Accepted);
            runtime.stop();
        })
        .await;
    });
    assert_eq!(calls.borrow().len(), 1);
}

#[test]
fn queue_deadline_expires_without_a_running_actor_and_capacity_is_released() {
    let limits = CommandLimits {
        queue_capacity: 1,
        total: Duration::from_millis(30),
        ..CommandLimits::default()
    };
    let (_directory, _store, service, runtime, _calls) = setup(Behavior::Accept, limits);
    let feature = identity("device-a", 2);
    register_light(
        &service,
        &runtime,
        feature.clone(),
        OperationPaths {
            gateway: true,
            ..OperationPaths::default()
        },
    );
    let first = service.command(&feature, DeviceCommand::SetPower(true));
    let full = service.command(
        &feature,
        DeviceCommand::SetBrightness(Percent::new(20.0).unwrap()),
    );
    assert_eq!(future::block_on(full), CommandOutcome::Unavailable);
    future::block_on(Timer::after(Duration::from_millis(40)));
    assert_eq!(future::block_on(first), CommandOutcome::Expired);
    let replacement = service.command(&feature, DeviceCommand::SetPower(false));
    future::block_on(runtime.run_until_idle());
    assert_eq!(future::block_on(replacement), CommandOutcome::Accepted);
}

#[test]
fn a_slow_device_does_not_block_later_work_for_another_device() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    store.xiaomi().replace(&credentials()).unwrap();
    let service = DeviceService::new();
    let calls = Rc::new(RefCell::new(Vec::new()));
    let transport = Rc::new(BlockingDeviceTransport {
        blocked_did: "device-a".into(),
        calls: calls.clone(),
    });
    let runtime = CommandRuntime::new(service.clone(), transport);
    let first = identity("device-a", 2);
    let second = identity("device-b", 2);
    for feature in [&first, &second] {
        register_light(
            &service,
            &runtime,
            feature.clone(),
            OperationPaths {
                gateway: true,
                ..OperationPaths::default()
            },
        );
    }
    let slow = service.command(&first, DeviceCommand::SetPower(true));
    let fast_one = service.command(&second, DeviceCommand::SetPower(true));
    let fast_two = service.command(&second, DeviceCommand::SetColorTemperature(3000));
    future::block_on(async {
        future::zip(runtime.run(), async {
            Timer::after(Duration::from_millis(10)).await;
            assert_eq!(fast_one.await, CommandOutcome::Accepted);
            assert_eq!(fast_two.await, CommandOutcome::Accepted);
            assert_eq!(
                calls
                    .borrow()
                    .iter()
                    .filter(|call| call.device.parent_did.as_str() == "device-b")
                    .count(),
                2
            );
            runtime.stop();
            assert_eq!(slow.await, CommandOutcome::Ambiguous);
        })
        .await;
    });
}

#[test]
fn newer_off_cancels_remaining_steps_of_a_power_on_batch() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    store.xiaomi().replace(&credentials()).unwrap();
    let service = DeviceService::new();
    let calls = Rc::new(RefCell::new(Vec::new()));
    let transport = Rc::new(BlockingDeviceTransport {
        blocked_did: "device-a".into(),
        calls: calls.clone(),
    });
    let runtime = CommandRuntime::new(service.clone(), transport);
    let feature = identity("device-a", 2);
    register_light(
        &service,
        &runtime,
        feature.clone(),
        OperationPaths {
            gateway: true,
            ..OperationPaths::default()
        },
    );
    let composite = service.command_batch(
        &feature,
        vec![
            DeviceCommand::SetPower(true),
            DeviceCommand::SetColorTemperature(3000),
        ],
    );
    future::block_on(async {
        future::zip(runtime.run(), async {
            while calls.borrow().is_empty() {
                Timer::after(Duration::from_millis(1)).await;
            }
            let off = service.command(&feature, DeviceCommand::SetPower(false));
            assert_eq!(composite.await, CommandOutcome::Ambiguous);
            assert_eq!(off.await, CommandOutcome::Accepted);
            runtime.stop();
        })
        .await;
    });
    let commands = calls
        .borrow()
        .iter()
        .map(|call| call.typed.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        commands,
        vec![
            DeviceCommand::SetPower(true),
            DeviceCommand::SetPower(false)
        ]
    );
}

#[test]
fn dropping_runner_retracts_blocked_transport_and_reports_sent_ambiguity() {
    let (_directory, _store, service, runtime, _calls) = setup(
        Behavior::WaitAfterWrite,
        CommandLimits {
            total: Duration::from_secs(1),
            ..CommandLimits::default()
        },
    );
    let feature = identity("device-a", 2);
    register_light(
        &service,
        &runtime,
        feature.clone(),
        OperationPaths {
            gateway: true,
            ..OperationPaths::default()
        },
    );
    let ticket = service.command(&feature, DeviceCommand::SetPower(true));
    future::block_on(async {
        assert!(future::poll_once(runtime.run()).await.is_none());
    });
    assert_eq!(future::block_on(ticket), CommandOutcome::Ambiguous);
    assert_eq!(
        future::block_on(service.command(&feature, DeviceCommand::SetPower(false))),
        CommandOutcome::Unavailable
    );
}

#[test]
fn dropping_runtime_does_not_leave_a_service_transport_reference_cycle() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    store.xiaomi().replace(&credentials()).unwrap();
    let service = DeviceService::new();
    let transport = Rc::new(FakeTransport {
        behavior: Cell::new(Behavior::Accept),
        calls: Rc::new(RefCell::new(Vec::new())),
    });
    let weak = Rc::downgrade(&transport);
    let runtime = CommandRuntime::new(service, transport.clone());
    drop(transport);
    drop(runtime);
    assert!(weak.upgrade().is_none());
}

#[test]
fn business_cancellation_keeps_a_delivered_reply_and_cancels_followup_steps() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    store.xiaomi().replace(&credentials()).unwrap();
    let service = DeviceService::new();
    let calls = Rc::new(RefCell::new(Vec::new()));
    let (delivered, delivered_rx) = flume::bounded(1);
    let (release, release_rx) = flume::bounded(1);
    let transport = Rc::new(HeldReplyTransport {
        calls: calls.clone(),
        delivered,
        release: release_rx,
    });
    let runtime = CommandRuntime::new(service.clone(), transport);
    let feature = identity("device-a", 2);
    register_light(
        &service,
        &runtime,
        feature.clone(),
        OperationPaths {
            gateway: true,
            ..OperationPaths::default()
        },
    );
    let ticket = service.command_batch(
        &feature,
        vec![
            DeviceCommand::SetPower(true),
            DeviceCommand::SetColorTemperature(3000),
        ],
    );
    future::block_on(async {
        future::zip(runtime.run(), async {
            delivered_rx.recv_async().await.unwrap();
            runtime.cancel_all();
            release.send_async(()).await.unwrap();
            assert_eq!(ticket.await, CommandOutcome::Accepted);
            runtime.stop();
        })
        .await;
    });
    assert_eq!(calls.borrow().len(), 1);
}

#[test]
fn device_service_rejects_an_unbounded_command_batch() {
    let (_directory, _store, service, runtime, calls) =
        setup(Behavior::Accept, CommandLimits::default());
    let feature = identity("device-a", 2);
    register_light(
        &service,
        &runtime,
        feature.clone(),
        OperationPaths {
            gateway: true,
            ..OperationPaths::default()
        },
    );
    let commands = vec![DeviceCommand::SetPower(true); crate::device::MAX_COMMAND_BATCH + 1];
    assert_eq!(
        future::block_on(service.command_batch(&feature, commands)),
        CommandOutcome::Unsupported
    );
    assert!(calls.borrow().is_empty());
}

#[test]
fn accepted_first_step_does_not_make_an_unsent_expired_second_step_ambiguous() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    store.xiaomi().replace(&credentials()).unwrap();
    let service = DeviceService::new();
    let calls = Rc::new(Cell::new(0));
    let runtime = CommandRuntime::with_limits(
        service.clone(),
        Rc::new(TwoStepDeadlineTransport {
            calls: calls.clone(),
        }),
        CommandLimits {
            local_attempt: Duration::from_millis(20),
            total: Duration::from_millis(100),
            ..CommandLimits::default()
        },
    );
    let feature = identity("device-a", 2);
    register_light(
        &service,
        &runtime,
        feature.clone(),
        OperationPaths {
            gateway: true,
            ..OperationPaths::default()
        },
    );
    let mut completions = runtime.subscribe_completions();
    let ticket = service.command_batch(
        &feature,
        vec![
            DeviceCommand::SetPower(true),
            DeviceCommand::SetColorTemperature(3000),
        ],
    );
    future::block_on(runtime.run_until_idle());
    assert_eq!(future::block_on(ticket), CommandOutcome::Expired);
    assert_eq!(calls.get(), 2);
    let completed = completions.drain();
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].command, DeviceCommand::SetPower(true));
    assert_eq!(completed[0].outcome, CommandOutcome::Accepted);
}

#[test]
fn completed_and_cancelled_work_retires_scheduler_bookkeeping() {
    let (_directory, _store, service, runtime, _calls) =
        setup(Behavior::Accept, CommandLimits::default());
    let feature = identity("device-a", 2);
    register_light(
        &service,
        &runtime,
        feature.clone(),
        OperationPaths {
            gateway: true,
            ..OperationPaths::default()
        },
    );
    for value in 1..=32 {
        let ticket = service.command(
            &feature,
            DeviceCommand::SetBrightness(Percent::new(f64::from(value)).unwrap()),
        );
        future::block_on(runtime.run_until_idle());
        assert_eq!(future::block_on(ticket), CommandOutcome::Accepted);
        service.stop_adjustment(&feature, Property::Brightness);
    }
    let state = runtime.inner.borrow();
    assert!(state.queues.is_empty());
    assert!(state.action_boundaries.is_empty());
    assert!(state.property_generations.is_empty());
    assert!(state.off_generations.is_empty());
}

#[test]
fn completing_one_device_keeps_another_reserved_devices_targets_live() {
    let (_directory, _store, service, runtime, calls) =
        setup(Behavior::Accept, CommandLimits::default());
    let first = identity("device-a", 2);
    let second = identity("device-b", 2);
    for feature in [&first, &second] {
        register_light(
            &service,
            &runtime,
            feature.clone(),
            OperationPaths {
                gateway: true,
                ..OperationPaths::default()
            },
        );
    }
    let first_ticket = service.command(&first, DeviceCommand::SetPower(true));
    let second_ticket = service.command(&second, DeviceCommand::SetPower(true));
    future::block_on(runtime.run_until_idle());
    assert_eq!(future::block_on(first_ticket), CommandOutcome::Accepted);
    assert_eq!(future::block_on(second_ticket), CommandOutcome::Accepted);
    assert_eq!(calls.borrow().len(), 2);
}

#[test]
fn actual_send_evidence_is_retained_when_observed_after_the_deadline() {
    let shared = SharedSendState {
        state: Arc::new(AtomicU8::new(0)),
        deadline: Instant::now() - Duration::from_millis(1),
    };
    assert!(shared.is_cancelled());
    // The OS may accept bytes just before expiry, with evidence recorded just after.
    shared.mark_sent();
    assert!(shared.may_have_been_sent());
}

#[test]
fn stopping_an_adjustment_retires_it_without_reviving_the_old_target() {
    let (_directory, _store, service, runtime, calls) =
        setup(Behavior::Accept, CommandLimits::default());
    let feature = identity("device-a", 2);
    register_light(
        &service,
        &runtime,
        feature.clone(),
        OperationPaths {
            gateway: true,
            ..OperationPaths::default()
        },
    );
    let old = service.command(
        &feature,
        DeviceCommand::SetBrightness(Percent::new(20.0).unwrap()),
    );
    service.stop_adjustment(&feature, Property::Brightness);
    assert!(runtime.inner.borrow().property_generations.is_empty());
    let next = service.command(
        &feature,
        DeviceCommand::SetBrightness(Percent::new(30.0).unwrap()),
    );
    future::block_on(runtime.run_until_idle());
    assert_eq!(future::block_on(old), CommandOutcome::Superseded);
    assert_eq!(future::block_on(next), CommandOutcome::Accepted);
    assert_eq!(calls.borrow().len(), 1);
    assert_eq!(
        calls.borrow()[0].1.typed,
        DeviceCommand::SetBrightness(Percent::new(30.0).unwrap())
    );
}

#[test]
fn stop_cancels_reserved_batches_before_their_first_poll() {
    let (_directory, _store, service, runtime, calls) =
        setup(Behavior::Accept, CommandLimits::default());
    let first = identity("device-a", 2);
    let second = identity("device-b", 2);
    for feature in [&first, &second] {
        register_light(
            &service,
            &runtime,
            feature.clone(),
            OperationPaths {
                gateway: true,
                ..OperationPaths::default()
            },
        );
    }
    let first_ticket = service.command(&first, DeviceCommand::SetPower(true));
    let second_ticket = service.command(&second, DeviceCommand::SetPower(true));
    let jobs = take_ready_jobs(&mut runtime.inner.borrow_mut(), &HashSet::new());
    assert_eq!(jobs.len(), 2);
    runtime.stop();
    for (_, batch) in jobs {
        future::block_on(runtime.execute(batch));
    }
    assert_eq!(future::block_on(first_ticket), CommandOutcome::Cancelled);
    assert_eq!(future::block_on(second_ticket), CommandOutcome::Cancelled);
    assert!(calls.borrow().is_empty());
}

#[test]
fn dropping_a_ticket_cancels_a_gate_wait_without_consuming_the_attempt_timeout() {
    let limits = CommandLimits {
        total: Duration::from_secs(1),
        local_attempt: Duration::from_millis(200),
        ..CommandLimits::default()
    };
    let (_directory, _store, service, runtime, calls) = setup(Behavior::Accept, limits);
    let feature = identity("device-a", 2);
    register_light(
        &service,
        &runtime,
        feature.clone(),
        OperationPaths {
            gateway: true,
            ..OperationPaths::default()
        },
    );
    future::block_on(async {
        let lease = runtime
            .execution_gate()
            .acquire(
                feature.physical.clone(),
                Instant::now() + Duration::from_secs(1),
            )
            .await
            .unwrap();
        let ticket = service.command(&feature, DeviceCommand::SetPower(true));
        let mut run = Box::pin(runtime.run_until_idle());
        assert!(future::poll_once(&mut run).await.is_none());
        drop(ticket);
        future::race(
            async {
                run.await;
            },
            async {
                Timer::after(Duration::from_millis(20)).await;
                panic!("cancelled gate wait retained the obsolete attempt");
            },
        )
        .await;
        drop(lease);

        let next = service.command(&feature, DeviceCommand::SetPower(false));
        runtime.run_until_idle().await;
        assert_eq!(next.await, CommandOutcome::Accepted);
    });
    assert_eq!(calls.borrow().len(), 1);
}

#[test]
fn completed_reply_survives_awaiting_the_ticket_after_its_deadline() {
    let limits = CommandLimits {
        total: Duration::from_millis(100),
        ..CommandLimits::default()
    };
    let (_directory, _store, service, runtime, calls) = setup(Behavior::Accept, limits);
    let feature = identity("device-a", 2);
    register_light(
        &service,
        &runtime,
        feature.clone(),
        OperationPaths {
            gateway: true,
            ..OperationPaths::default()
        },
    );
    let mut tickets = Vec::new();
    for _ in 0..32 {
        tickets.push(service.command(&feature, DeviceCommand::SetPower(true)));
        future::block_on(runtime.run_until_idle());
    }
    assert_eq!(calls.borrow().len(), tickets.len());
    future::block_on(Timer::after(limits.total + Duration::from_millis(20)));
    for ticket in tickets {
        assert_eq!(future::block_on(ticket), CommandOutcome::Accepted);
    }
}
