use super::*;
use crate::xiaomi::{
    adapter::{routing::PathPriority, state::Session},
    central::{
        CentralError, RequestOutcome, RequestStage, RequestTimeout, SessionDriver, SessionHandle,
    },
};
use futures_lite::io::{AsyncRead, AsyncWrite};
use std::{
    cell::Cell,
    collections::VecDeque,
    task::{Context as TaskContext, Poll, Waker},
};

#[derive(Default)]
struct Wire {
    budget: Cell<usize>,
    polls: Cell<usize>,
    written: RefCell<Vec<u8>>,
    incoming: RefCell<VecDeque<u8>>,
    waker: RefCell<Option<Waker>>,
}
impl Wire {
    fn wake(&self) {
        if let Some(waker) = self.waker.borrow_mut().take() {
            waker.wake();
        }
    }
    fn allow(&self, bytes: usize) {
        self.budget.set(bytes);
        self.wake();
    }
    fn accept(&self) {
        self.reply(accepted("one", 2, 0));
    }
    fn reply(&self, value: Value) {
        let mut frames = crate::xiaomi::mqtt::Frames::default();
        frames.push(&self.written.borrow()).unwrap();
        let Packet::Publish(publish) = frames.next_packet().unwrap().unwrap() else {
            panic!("business publication expected")
        };
        assert!(frames.next_packet().unwrap().is_none());
        let mid = decode_mips(&publish.payload).unwrap().id;
        let reply = Publish::new(
            "controlled/reply",
            QoS::AtMostOnce,
            encode_mips(mid, None, &value),
        );
        let mut bytes = bytes::BytesMut::new();
        reply.write(&mut bytes).unwrap();
        self.incoming.borrow_mut().extend(bytes);
        self.wake();
    }
}
struct Transport(Rc<Wire>);
impl AsyncRead for Transport {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        bytes: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        let mut incoming = self.0.incoming.borrow_mut();
        let count = incoming.len().min(bytes.len());
        if count == 0 {
            *self.0.waker.borrow_mut() = Some(cx.waker().clone());
            return Poll::Pending;
        }
        for byte in &mut bytes[..count] {
            *byte = incoming.pop_front().unwrap();
        }
        Poll::Ready(Ok(count))
    }
}
impl AsyncWrite for Transport {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        self.0.polls.set(self.0.polls.get() + 1);
        let count = bytes.len().min(self.0.budget.get());
        if count == 0 {
            *self.0.waker.borrow_mut() = Some(cx.waker().clone());
            return Poll::Pending;
        }
        self.0.budget.set(self.0.budget.get() - count);
        self.0
            .written
            .borrow_mut()
            .extend_from_slice(&bytes[..count]);
        Poll::Ready(Ok(count))
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_close(self: Pin<&mut Self>, _: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}
fn local(c: &Control, wire: Rc<Wire>) -> (SessionHandle, SessionDriver) {
    let (handle, driver) = SessionDriver::ready_for_test(Transport(wire), Duration::from_secs(1));
    c.context.live.borrow_mut().session = Some(Session::new(handle.clone()));
    c.context.local_list_success(&handle);
    c.context
        .observe_access(vec![super::list_scheduling::observation(&c.data[0], true)]);
    c.context.reconcile_all().unwrap();
    (handle, driver)
}
fn response_timeout() -> RequestOutcome<()> {
    RequestOutcome {
        result: Err(CentralError::Timeout),
        stage: RequestStage::MayHaveBeenSent,
        timeout: Some(RequestTimeout::Response),
    }
}

#[test]
fn stale_local_device_errors_cannot_revoke_current_access() {
    for stale in 0..2 {
        block_on(Rig::new(true, &["one"]).run(|c| async move {
            let (handle, _) = local(&c, Rc::new(Wire::default()));
            let work = c.context.live.borrow().devices["one"]
                .admitted
                .clone()
                .unwrap();
            match stale {
                0 => {
                    // A different current connection makes the captured one stale.
                    local(&c, Rc::new(Wire::default()));
                }
                _ => work.life.close(),
            }
            c.context.local_device_outcome(
                &handle,
                &work,
                &RequestOutcome::<()> {
                    result: Err(CentralError::Rejected(-704_042_011)),
                    stage: RequestStage::BusinessResult,
                    timeout: None,
                },
            );
            assert!(
                c.context.live.borrow().devices["one"]
                    .access
                    .as_ref()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .online
            );
            assert_eq!(c.context.priority(), PathPriority::LocalFirst);
        }));
    }
}

#[test]
fn an_unsent_pending_write_reselects_cloud_and_submits_the_absolute_target_once() {
    let rig = Rig::new(true, &["one"]);
    rig.cloud.lock().unwrap().online.insert("one".into());
    block_on(rig.run(|c| async move {
        until(|| c.context.cloud_scope_valid.get()).await;
        let wire = Rc::new(Wire::default());
        let (handle, driver) = local(&c, wire.clone());
        let one = function(&c.context.registry, "one", "main");
        future::or(
            async {
                driver.run(|_| {}).await.unwrap();
                panic!("driver unexpectedly stopped");
            },
            async {
                let mut command = Box::pin(one.execute(Command::On));
                assert!(poll_once(command.as_mut()).await.is_none());
                until(|| wire.polls.get() > 0).await;
                c.context.local_outcome(&handle, &response_timeout());
                wire.allow(usize::MAX);
                assert_eq!(command.await.unwrap().power(), Some(Power::On));
                assert!(wire.written.borrow().is_empty());
                let cloud = c.cloud.lock().unwrap();
                assert_eq!(cloud.property_writes.len(), 1);
                assert!(cloud.property_writes[0].3);
            },
        )
        .await;
    }));
}

#[test]
fn command_diagnostics_record_no_local_submission_for_a_withdrawn_zero_byte_write() {
    let rig = Rig::new(true, &["one"]);
    rig.cloud.lock().unwrap().online.insert("one".into());
    block_on(rig.run(|c| async move {
        until(|| c.context.cloud_scope_valid.get()).await;
        let wire = Rc::new(Wire::default());
        let (handle, driver) = local(&c, wire.clone());
        let one = function(&c.context.registry, "one", "main");
        future::or(
            async {
                driver.run(|_| {}).await.unwrap();
                panic!("driver unexpectedly stopped")
            },
            async {
                let mut command = Box::pin(one.execute(Command::On));
                assert!(poll_once(command.as_mut()).await.is_none());
                until(|| wire.polls.get() > 0).await;
                let status = c.context.diagnostics();
                let inflight = status.devices[0].commands[0].inflight.as_ref().unwrap();
                let id = inflight.id;
                assert_eq!(inflight.actual_path, None);
                c.context.local_outcome(&handle, &response_timeout());
                wire.allow(usize::MAX);
                command.await.unwrap();
                let status = c.context.diagnostics();
                let last = status.devices[0].commands[0].last.as_ref().unwrap();
                assert_eq!(last.id, id);
                assert_eq!(
                    last.actual_path,
                    Some(crate::xiaomi::adapter::PathKind::Cloud)
                );
                assert_eq!(last.result, Some(Ok(())));
                assert!(wire.written.borrow().is_empty());
                assert_eq!(c.cloud.lock().unwrap().property_writes.len(), 1);
            },
        )
        .await;
    }));
}

#[test]
fn write_observation_capture_occurs_at_first_socket_bytes() {
    for after_send in [false, true] {
        block_on(Rig::new(true, &["one"]).run(|c| async move {
            let wire = Rc::new(Wire::default());
            let (_, driver) = local(&c, wire.clone());
            let one = function(&c.context.registry, "one", "main");
            let work = c.context.live.borrow().devices["one"]
                .admitted
                .clone()
                .unwrap();
            future::or(
                async {
                    driver.run(|_| {}).await.unwrap();
                    panic!("driver unexpectedly stopped");
                },
                async {
                    let mut command = Box::pin(one.execute(Command::On));
                    assert!(poll_once(command.as_mut()).await.is_none());
                    until(|| wire.polls.get() > 0).await;
                    work.test_notify(
                        crate::xiaomi::capability::PropertyBinding { siid: 2, piid: 1 },
                        false,
                    );
                    wire.allow(1);
                    until(|| wire.written.borrow().len() == 1).await;
                    if after_send {
                        // The same value still counts as a notification after the write.
                        work.test_notify(
                            crate::xiaomi::capability::PropertyBinding { siid: 2, piid: 1 },
                            false,
                        );
                    }
                    wire.allow(usize::MAX);
                    until(|| {
                        let mut frames = crate::xiaomi::mqtt::Frames::default();
                        frames.push(&wire.written.borrow()).unwrap();
                        frames.next_packet().unwrap().is_some()
                    })
                    .await;
                    wire.accept();
                    assert_eq!(
                        command.await.unwrap().power(),
                        Some(if after_send { Power::Off } else { Power::On })
                    );
                    assert_eq!(c.cloud.lock().unwrap().property_writes.len(), 0);
                },
            )
            .await;
        }));
    }
}

#[test]
fn only_current_sent_response_timeouts_degrade_and_only_current_lists_restore() {
    block_on(Rig::new(true, &["one"]).run(|c| async move {
        let (old, _) = local(&c, Rc::new(Wire::default()));
        for (stage, timeout, error) in [
            (
                RequestStage::NotSent,
                Some(RequestTimeout::Queue),
                CentralError::Timeout,
            ),
            (
                RequestStage::MayHaveBeenSent,
                Some(RequestTimeout::Subscription),
                CentralError::Timeout,
            ),
            (
                RequestStage::MayHaveBeenSent,
                Some(RequestTimeout::Transport),
                CentralError::Timeout,
            ),
            (
                RequestStage::BusinessResult,
                None,
                CentralError::Rejected(-1),
            ),
            (RequestStage::NotSent, None, CentralError::Cancelled),
        ] {
            c.context.local_outcome(
                &old,
                &RequestOutcome::<()> {
                    result: Err(error),
                    stage,
                    timeout,
                },
            );
            assert_eq!(c.context.priority(), PathPriority::LocalFirst);
        }
        c.context.local_outcome(&old, &response_timeout());
        assert_eq!(c.context.priority(), PathPriority::CloudFirst);
        c.context.disconnect("test reconnect".into());
        let (new, _) = SessionDriver::ready_for_test(
            Transport(Rc::new(Wire::default())),
            Duration::from_secs(1),
        );
        c.context.live.borrow_mut().session = Some(Session::new(new.clone()));
        assert_eq!(c.context.priority(), PathPriority::CloudFirst);
        c.context.local_list_success(&old);
        assert_eq!(c.context.priority(), PathPriority::CloudFirst);
        c.context.local_list_success(&new);
        assert_eq!(c.context.priority(), PathPriority::LocalFirst);
        c.context.local_outcome(&old, &response_timeout());
        assert_eq!(c.context.priority(), PathPriority::LocalFirst);
    }));
}

#[test]
fn a_cancelled_zero_byte_refresh_keeps_one_demand_until_shared_recovery() {
    block_on(Rig::new(true, &["one"]).run(|c| async move {
        let wire = Rc::new(Wire::default());
        let (handle, driver) = local(&c, wire.clone());
        let one = function(&c.context.registry, "one", "main");
        let work = c.context.live.borrow().devices["one"]
            .admitted
            .clone()
            .unwrap();
        let reads = crate::xiaomi::adapter::refresh::local(c.context.clone(), handle.clone());
        future::or(
            async {
                driver.run(|_| {}).await.unwrap();
                panic!("driver unexpectedly stopped");
            },
            future::or(
                async {
                    reads.await.unwrap();
                    panic!("refresh task unexpectedly stopped");
                },
                async {
                    until(|| wire.polls.get() > 0).await;
                    c.clock
                        .automatic
                        .store(false, std::sync::atomic::Ordering::Relaxed);
                    c.context.local_outcome(&handle, &response_timeout());
                    wire.allow(usize::MAX);
                    for _ in 0..20 {
                        future::yield_now().await;
                    }
                    for _ in 0..3 {
                        work.refresh();
                    }
                    c.clock.advance(Duration::from_secs(3));
                    for _ in 0..20 {
                        future::yield_now().await;
                    }
                    assert!(wire.written.borrow().is_empty());
                    c.context.local_list_success(&handle);
                    for _ in 0..20 {
                        future::yield_now().await;
                    }
                    c.clock.advance(Duration::from_millis(200));
                    until(|| {
                        let mut frames = crate::xiaomi::mqtt::Frames::default();
                        frames.push(&wire.written.borrow()).unwrap();
                        frames.next_packet().unwrap().is_some()
                    })
                    .await;
                    wire.reply(json!({"value":true}));
                    until(|| one.snapshot().power() == Some(Power::On)).await;
                    c.clock.advance(Duration::from_secs(3));
                    for _ in 0..20 {
                        future::yield_now().await;
                    }
                    let mut frames = crate::xiaomi::mqtt::Frames::default();
                    frames.push(&wire.written.borrow()).unwrap();
                    let Packet::Publish(publish) = frames.next_packet().unwrap().unwrap() else {
                        panic!("read publication expected")
                    };
                    assert_eq!(publish.topic, "master/proxy/get");
                    assert!(frames.next_packet().unwrap().is_none());
                },
            ),
        )
        .await;
    }));
}

#[test]
fn dropping_a_zero_byte_refresh_task_retires_its_operation_and_returns_the_demand() {
    block_on(Rig::new(true, &["one"]).run(|c| async move {
        let wire = Rc::new(Wire::default());
        let (handle, driver) = local(&c, wire.clone());
        let work = c.context.live.borrow().devices["one"]
            .admitted
            .clone()
            .unwrap();
        let mut reads = Box::pin(crate::xiaomi::adapter::refresh::local(
            c.context.clone(),
            handle,
        ));
        future::or(
            async {
                driver.run(|_| {}).await.unwrap();
                panic!("driver unexpectedly stopped");
            },
            async {
                while wire.polls.get() == 0 {
                    assert!(poll_once(reads.as_mut()).await.is_none());
                    future::yield_now().await;
                }
                c.clock
                    .automatic
                    .store(false, std::sync::atomic::Ordering::Relaxed);
                assert_eq!(work.refresh.borrow().snapshot().inflight.len(), 1);
                drop(reads);
                assert!(work.refresh.borrow().snapshot().inflight.is_empty());
                assert_eq!(work.refresh.borrow().snapshot().pending.len(), 1);
                wire.allow(usize::MAX);
                for _ in 0..20 {
                    future::yield_now().await;
                }
                assert!(wire.written.borrow().is_empty());
                assert_eq!(c.context.priority(), PathPriority::LocalFirst);
                assert_eq!(
                    function(&c.context.registry, "one", "main")
                        .snapshot()
                        .power(),
                    Some(Power::Unknown)
                );
            },
        )
        .await;
    }));
}

#[test]
fn unsent_light_batch_recomputes_mode_and_power_after_retained_observation() {
    use crate::device::{LightMode, LightPatch, LightTarget, Rgb};
    use crate::xiaomi::capability::PropertyBinding;
    block_on(Rig::light("devcea.light.ls2307").run(|c| async move {
        let wire = Rc::new(Wire::default());
        let (_, driver) = local(&c, wire.clone());
        let light = function(&c.context.registry, "one", "main");
        light
            .observe_light(LightPatch {
                power: Some(true),
                brightness_percent: Some(10.0),
                temperature_kelvin: Some(2700),
                color: Rgb::from_packed(0x123456),
                mode: Some(LightMode::Rgb),
            })
            .unwrap();
        future::or(
            async {
                driver.run(|_| {}).await.unwrap();
                panic!("driver stopped")
            },
            async {
                let action = light
                    .begin_light_action(
                        LightTarget::rgb(Some(true), Rgb::from_packed(0x456789).unwrap()),
                        true,
                    )
                    .unwrap();
                until(|| wire.polls.get() > 0).await;
                let work = c.context.live.borrow().devices["one"]
                    .admitted
                    .clone()
                    .unwrap();
                work.notify_value(PropertyBinding { siid: 4, piid: 2 }, &json!(0), true, true);
                work.notify_value(
                    PropertyBinding { siid: 2, piid: 1 },
                    &json!(false),
                    true,
                    true,
                );
                assert!(light.light_action_current(action.token()));
                wire.allow(usize::MAX);
                until(|| !wire.written.borrow().is_empty()).await;
                let mut frames = crate::xiaomi::mqtt::Frames::default();
                frames.push(&wire.written.borrow()).unwrap();
                let Packet::Publish(publish) = frames.next_packet().unwrap().unwrap() else {
                    panic!()
                };
                assert!(frames.next_packet().unwrap().is_none());
                let payload = decode_mips(&publish.payload).unwrap().payload;
                assert_eq!(
                    payload["rpc"]["params"],
                    json!([
                        {"did":"one","siid":2,"piid":1,"value":true},
                        {"did":"one","siid":4,"piid":1,"value":0x456789},
                        {"did":"one","siid":4,"piid":2,"value":1}
                    ])
                );
                wire.reply(json!({
                    "result": [
                        {"did":"one","siid":2,"piid":1,"code":0},
                        {"did":"one","siid":4,"piid":1,"code":0},
                        {"did":"one","siid":4,"piid":2,"code":0}
                    ]
                }));
                action.accepted().await.unwrap();
                let snapshot = light.snapshot();
                assert_eq!(snapshot.power(), Some(Power::Off));
                assert_eq!(
                    snapshot.lighting().unwrap().mode,
                    Some(LightMode::Temperature)
                );
            },
        )
        .await;
    }));
}

#[test]
fn latest_tick_withdraws_unsent_transport_frame_and_preserves_linked_power() {
    use crate::device::{LightPatch, LightTarget};
    for power in [None, Some(false)] {
        block_on(Rig::light("xiaomi.light.ceil04").run(|c| async move {
            let wire = Rc::new(Wire::default());
            let (_, driver) = local(&c, wire.clone());
            let light = function(&c.context.registry, "one", "main");
            light
                .observe_light(LightPatch {
                    power: Some(false),
                    brightness_percent: Some(10.0),
                    ..Default::default()
                })
                .unwrap();
            let driving = async {
                driver.run(|_| {}).await.unwrap();
                panic!("driver stopped")
            };
            let commanding = async {
                let action = light
                    .begin_light_action(LightTarget::brightness(Some(true), 20.0).expect("valid test target"),
                        false,
                    )
                    .unwrap();
                until(|| wire.polls.get() > 0).await;
                light
                    .submit_light_frame(
                        action.token(),
                        LightTarget::brightness(power, 70.0)
                            .expect("valid test target"),
                        true,
                    )
                    .unwrap();
                assert!(light.light_action_current(action.token()));
                assert!(futures_lite::future::poll_once(action.accepted()).await.is_none());
                wire.allow(usize::MAX);
                until(|| !wire.written.borrow().is_empty()).await;
                let mut frames = crate::xiaomi::mqtt::Frames::default();
                frames.push(&wire.written.borrow()).unwrap();
                let Packet::Publish(publish) = frames.next_packet().unwrap().unwrap() else {
                    panic!()
                };
                assert!(frames.next_packet().unwrap().is_none());
                let payload = decode_mips(&publish.payload).unwrap().payload;
                let expected = if power == Some(false) {
                    json!([{"did":"one","siid":2,"piid":2,"value":70}])
                } else {
                    json!([{"did":"one","siid":2,"piid":1,"value":true},{"did":"one","siid":2,"piid":2,"value":70}])
                };
                assert_eq!(payload["rpc"]["params"], expected);
                let result = expected
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|item| json!({"did":"one","siid":item["siid"],"piid":item["piid"],"code":0}))
                    .collect::<Vec<_>>();
                wire.reply(json!({"result":result}));
                action.accepted().await.unwrap();
                action.ended().await.unwrap();
                assert_eq!(light.snapshot().power(), Some(Power::Off));
            };
            future::or(driving, commanding).await;
        }));
    }
}

#[test]
fn unsent_light_target_reselects_cloud_once_without_local_bytes() {
    use crate::device::LightTarget;
    let rig = Rig::light("xiaomi.light.ceil04");
    rig.cloud.lock().unwrap().online.insert("one".into());
    block_on(rig.run(|c| async move {
        until(|| c.context.cloud_scope_valid.get()).await;
        let wire = Rc::new(Wire::default());
        let (handle, driver) = local(&c, wire.clone());
        let light = function(&c.context.registry, "one", "main");
        future::or(
            async {
                driver.run(|_| {}).await.unwrap();
                panic!("driver stopped")
            },
            async {
                let action = light
                    .begin_light_action(
                        LightTarget::brightness(None, 60.0).expect("valid test target"),
                        true,
                    )
                    .unwrap();
                until(|| wire.polls.get() > 0).await;
                c.context.local_outcome(&handle, &response_timeout());
                wire.allow(usize::MAX);
                action.accepted().await.unwrap();
                assert!(wire.written.borrow().is_empty());
                let cloud = c.cloud.lock().unwrap();
                assert_eq!(cloud.numeric_writes.len(), 1);
                assert_eq!(cloud.numeric_writes[0].0[0]["value"], 60);
            },
        )
        .await;
    }));
}

#[test]
fn an_unsent_light_target_is_not_own_feedback() {
    use crate::device::{CommandError, LightPatch, LightTarget};
    use crate::xiaomi::capability::PropertyBinding;
    block_on(Rig::light("xiaomi.light.ceil04").run(|c| async move {
        let wire = Rc::new(Wire::default());
        let (_, driver) = local(&c, wire.clone());
        let light = function(&c.context.registry, "one", "main");
        light
            .observe_light(LightPatch {
                brightness_percent: Some(10.0),
                ..Default::default()
            })
            .unwrap();
        future::or(
            async {
                driver.run(|_| {}).await.unwrap();
                panic!("driver stopped")
            },
            async {
                let action = light
                    .begin_light_action(
                        LightTarget::brightness(None, 60.0).expect("valid test target"),
                        false,
                    )
                    .unwrap();
                until(|| wire.polls.get() > 0).await;
                let work = c.context.live.borrow().devices["one"]
                    .admitted
                    .clone()
                    .unwrap();
                work.notify_value(
                    PropertyBinding { siid: 2, piid: 2 },
                    &json!(60),
                    false,
                    true,
                );
                assert!(!light.light_action_current(action.token()));
                assert_eq!(action.ended().await.unwrap_err(), CommandError::Superseded);
                assert_eq!(
                    action.accepted().await.unwrap_err(),
                    CommandError::Superseded
                );
                assert_eq!(
                    light.snapshot().lighting().unwrap().brightness_percent,
                    Some(60.0)
                );
                wire.allow(usize::MAX);
                until(|| {
                    c.context.diagnostics().devices[0].commands[0]
                        .last
                        .is_some()
                })
                .await;
                assert!(wire.written.borrow().is_empty());
            },
        )
        .await;
    }));
}

#[test]
fn unsent_stop_keeps_its_actual_result_in_command_diagnostics() {
    use crate::device::{CommandError, LightDimension, LightTarget};
    block_on(Rig::light("xiaomi.light.ceil04").run(|c| async move {
        let wire = Rc::new(Wire::default());
        let (_, driver) = local(&c, wire.clone());
        let light = function(&c.context.registry, "one", "main");
        future::or(
            async {
                driver.run(|_| {}).await.unwrap();
                panic!("driver stopped")
            },
            async {
                let action = light
                    .begin_light_action(
                        LightTarget::brightness(None, 60.0).expect("valid test target"),
                        false,
                    )
                    .unwrap();
                until(|| wire.polls.get() > 0).await;
                light.stop_light(LightDimension::Brightness).unwrap();
                assert_eq!(action.ended().await.unwrap_err(), CommandError::Stopped);
                wire.allow(usize::MAX);
                until(|| {
                    c.context.diagnostics().devices[0].commands[0]
                        .last
                        .is_some()
                })
                .await;
                assert_eq!(
                    c.context.diagnostics().devices[0].commands[0]
                        .last
                        .as_ref()
                        .unwrap()
                        .result,
                    Some(Err(CommandError::Stopped))
                );
                assert!(wire.written.borrow().is_empty());
            },
        )
        .await;
    }));
}

#[test]
fn a_state_notification_satisfies_a_reserved_baseline_before_its_first_socket_bytes() {
    use crate::xiaomi::capability::{EventBinding, PropertyBinding};
    let instance = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/xiaomi-pir1.json"
    ))
    .unwrap();
    block_on(
        Rig::custom_lights(&[("one", "xiaomi.motion.pir1", instance)]).run(|c| async move {
            let wire = Rc::new(Wire::default());
            let (handle, driver) = local(&c, wire.clone());
            let work = c.context.live.borrow().devices["one"]
                .admitted
                .clone()
                .unwrap();
            let binding = PropertyBinding {
                siid: 2,
                piid: 1024,
            };
            for _ in 0..20 {
                future::yield_now().await;
            }
            *work.refresh.borrow_mut() = Default::default();
            work.refresh
                .borrow_mut()
                .request_baseline("motion", binding);
            let reads = crate::xiaomi::adapter::refresh::local(c.context.clone(), handle);
            future::or(
                async {
                    driver.run(|_| {}).await.unwrap();
                    panic!("driver unexpectedly stopped");
                },
                future::or(
                    async {
                        reads.await.unwrap();
                        panic!("refresh task unexpectedly stopped");
                    },
                    async {
                        until(|| wire.polls.get() > 0).await;
                        c.clock
                            .automatic
                            .store(false, std::sync::atomic::Ordering::Relaxed);
                        assert!(wire.written.borrow().is_empty());
                        let binding = EventBinding {
                            siid: 2,
                            eiid: 1008,
                        };
                        work.observe_state(
                            work.event_function(binding).unwrap(),
                            Some(true),
                            crate::device::StateInput::Event {
                                siid: binding.siid,
                                eiid: binding.eiid,
                            },
                            Observation {
                                source: PathKind::Local,
                                source_time: Some(1),
                                received_at: 2,
                            },
                            Origin::Notification,
                        );
                        wire.allow(usize::MAX);
                        until(|| work.refresh.borrow().snapshot().inflight.is_empty()).await;
                        assert!(work.refresh.borrow().snapshot().pending.is_empty());
                        c.clock.advance(Duration::from_millis(200));
                        for _ in 0..20 {
                            future::yield_now().await;
                        }
                        assert!(wire.written.borrow().is_empty());
                        assert_eq!(
                            function(&c.context.registry, "one", "motion")
                                .snapshot()
                                .state_sensor()
                                .unwrap()
                                .active,
                            Some(true)
                        );
                    },
                ),
            )
            .await;
        }),
    );
}
