use super::*;
use crate::xiaomi::central::{
    RequestOutcome, RequestStage, RequestTimeout, SendDecision, SendHooks,
};
use futures_lite::future::{block_on, poll_once};
use mqttbytes::{
    QoS,
    v5::{SubAck, SubscribeReasonCode},
};
use serde_json::json;
use std::{
    cell::{Cell, RefCell},
    time::Duration,
};

struct ControlledIo {
    budget: Rc<Cell<usize>>,
    written: Rc<Cell<usize>>,
}
impl AsyncRead for ControlledIo {
    fn poll_read(
        self: Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
        _: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        Poll::Pending
    }
}
impl AsyncWrite for ControlledIo {
    fn poll_write(
        self: Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let count = bytes.len().min(self.budget.get());
        if count == 0 {
            return Poll::Pending;
        }
        self.budget.set(self.budget.get() - count);
        self.written.set(self.written.get() + count);
        Poll::Ready(Ok(count))
    }
    fn poll_flush(
        self: Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_close(
        self: Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}
fn rig() -> (
    SessionHandle,
    SessionDriver,
    Rc<Cell<usize>>,
    Rc<Cell<usize>>,
) {
    let budget = Rc::new(Cell::new(0));
    let written = Rc::new(Cell::new(0));
    let (handle, mut driver) = SessionDriver::new(
        Box::new(ControlledIo {
            budget: budget.clone(),
            written: written.clone(),
        }),
        wire::Frames::default(),
        "client",
        Timing::default(),
        wire::MAX_FRAME_SIZE,
        LIMIT,
    )
    .unwrap();
    driver.subscriptions.clear();
    driver.output.clear();
    driver.ready = true;
    (handle, driver, budget, written)
}
async fn write_ready(driver: &mut SessionDriver, budget: &Cell<usize>, count: usize) {
    budget.set(count);
    let _ = poll_once(future::poll_fn(|cx| driver.poll_io(cx))).await;
}
fn reply(driver: &mut SessionDriver, mid: u32, value: serde_json::Value) {
    driver
        .packet(
            Packet::Publish(Publish::new(
                "client/reply",
                QoS::AtMostOnce,
                wire::encode_mips(mid, None, &value),
            )),
            &mut |_| {},
        )
        .unwrap();
}
const BINDING: PropertyBinding = PropertyBinding { siid: 2, piid: 1 };

fn received_property(did: &str, pkid: u16, value: bool) -> Packet {
    let mut publish = Publish::new(
        format!("client/appMsg/notify/iot/{did}/property/2.1"),
        QoS::ExactlyOnce,
        wire::encode_mips(1, None, &json!({"did":did,"siid":2,"piid":1,"value":value})),
    );
    publish.pkid = pkid;
    Packet::Publish(publish)
}

async fn ready_property(handle: &SessionHandle, driver: &mut SessionDriver, did: &str) {
    let subscribed = handle.subscribe_property(did, BINDING);
    driver.drain_requests().unwrap();
    let pkid = *driver.subscriptions.keys().next().unwrap();
    driver
        .packet(
            Packet::SubAck(SubAck::new(pkid, vec![SubscribeReasonCode::QoS2])),
            &mut |_| {},
        )
        .unwrap();
    subscribed.await.unwrap();
    driver.output.clear();
}

#[test]
fn received_property_invalidation_keeps_subscriptions_and_business_requests() {
    block_on(async {
        let (handle, mut driver, budget, _) = rig();
        ready_property(&handle, &mut driver, "target").await;
        let pending = handle.subscribe_property("target", PropertyBinding { siid: 3, piid: 1 });
        let read = handle.read_property("target", BINDING, |_| {});
        let write = handle.set_property("target", BINDING, true, |_| {});
        driver.drain_requests().unwrap();
        let subscription_id = *driver.subscriptions.keys().next().unwrap();
        let read_id = *driver
            .pending
            .iter()
            .find(|(_, request)| {
                matches!(
                    request.kind,
                    super::super::request::RequestKind::Read { .. }
                )
            })
            .unwrap()
            .0;
        let write_id = *driver.pending.keys().find(|id| **id != read_id).unwrap();
        for _ in 0..3 {
            write_ready(&mut driver, &budget, usize::MAX).await;
        }
        budget.set(0);
        assert_eq!(
            driver.pending[&read_id].stage,
            RequestStage::MayHaveBeenSent
        );
        assert_eq!(
            driver.pending[&write_id].stage,
            RequestStage::MayHaveBeenSent
        );
        let events = Rc::new(RefCell::new(Vec::new()));
        let mut on_event = |event| events.borrow_mut().push(event);
        driver
            .packet(received_property("target", 55, true), &mut on_event)
            .unwrap();
        let outputs_before = driver.output.len();
        handle.invalidate_received_properties("target");
        for packet in [
            Packet::PubRel(mqttbytes::v5::PubRel::new(55)),
            received_property("target", 56, false),
            Packet::PubRel(mqttbytes::v5::PubRel::new(56)),
        ] {
            driver
                .frames
                .push(&wire::encode_packet(&packet).unwrap())
                .unwrap();
        }
        assert!(poll_once(driver.drive(&mut on_event)).await.is_none());
        assert_eq!(
            events.borrow().as_slice(),
            &[CentralEvent::Property {
                did: "target".into(),
                binding: BINDING,
                value: false
            }],
            "a received property from the retired source was dispatched"
        );
        assert!(driver.ready);
        assert_eq!(driver.property_topics.len(), 1);
        assert_eq!(driver.subscriptions.len(), 1);
        assert!(driver.subscriptions[&subscription_id].request.is_some());
        assert_eq!(driver.pending.len(), 2);
        assert_eq!(driver.output.len(), outputs_before + 3);
        let mut outputs = wire::Frames::default();
        for output in driver.output.iter().skip(outputs_before) {
            outputs.push(&output.bytes).unwrap();
        }
        assert!(
            matches!(outputs.next_packet().unwrap(), Some(Packet::PubComp(ack)) if ack.pkid == 55)
        );
        assert!(
            matches!(outputs.next_packet().unwrap(), Some(Packet::PubRec(ack)) if ack.pkid == 56)
        );
        assert!(
            matches!(outputs.next_packet().unwrap(), Some(Packet::PubComp(ack)) if ack.pkid == 56)
        );
        assert!(outputs.next_packet().unwrap().is_none());
        driver
            .packet(
                Packet::SubAck(SubAck::new(
                    subscription_id,
                    vec![SubscribeReasonCode::QoS2],
                )),
                &mut on_event,
            )
            .unwrap();
        pending.await.unwrap();
        reply(&mut driver, read_id, json!({"value":false}));
        reply(
            &mut driver,
            write_id,
            json!({"result":[{"did":"target","code":0}]}),
        );
        assert!(!read.await.unwrap());
        write.await.unwrap();
    });
}

#[test]
fn received_property_invalidation_takes_effect_between_buffered_frames_in_one_poll() {
    block_on(async {
        let (handle, mut driver, _, _) = rig();
        ready_property(&handle, &mut driver, "target").await;
        ready_property(&handle, &mut driver, "other").await;
        driver
            .packet(received_property("target", 61, true), &mut |_| {})
            .unwrap();
        let mut marker = match received_property("other", 62, true) {
            Packet::Publish(publish) => publish,
            _ => unreachable!(),
        };
        marker.qos = QoS::AtMostOnce;
        for packet in [
            Packet::Publish(marker),
            Packet::PubRel(mqttbytes::v5::PubRel::new(61)),
        ] {
            driver
                .frames
                .push(&wire::encode_packet(&packet).unwrap())
                .unwrap();
        }
        let events = Rc::new(RefCell::new(Vec::new()));
        let mut on_event = |event| {
            if matches!(&event, CentralEvent::Property { did, .. } if did == "other") {
                handle.invalidate_received_properties("target");
            }
            events.borrow_mut().push(event);
        };
        assert!(poll_once(driver.drive(&mut on_event)).await.is_none());
        assert_eq!(
            events.borrow().as_slice(),
            &[CentralEvent::Property {
                did: "other".into(),
                binding: BINDING,
                value: true
            }]
        );
        assert!(driver.ready);
        assert_eq!(driver.property_topics.len(), 2);
    });
}

fn hooks(
    allowed: Rc<Cell<bool>>,
    checks: Rc<Cell<usize>>,
    first_send: Rc<Cell<usize>>,
) -> SendHooks {
    SendHooks::new(
        move || {
            checks.set(checks.get() + 1);
            if allowed.get() {
                SendDecision::Send
            } else {
                SendDecision::Withdraw
            }
        },
        move || first_send.set(first_send.get() + 1),
    )
}

fn unsent<T>(error: CentralError) -> RequestOutcome<T> {
    RequestOutcome {
        result: Err(error),
        stage: RequestStage::NotSent,
        timeout: None,
    }
}

fn count_result<T>(calls: &Rc<Cell<usize>>) -> impl FnOnce(RequestOutcome<T>) + use<T> {
    let calls = calls.clone();
    move |_| calls.set(calls.get() + 1)
}

#[test]
fn guarded_write_rechecks_after_pending_and_withdraws_without_sending() {
    block_on(async {
        let (handle, mut driver, budget, written) = rig();
        let allowed = Rc::new(Cell::new(true));
        let checks = Rc::new(Cell::new(0));
        let first_send = Rc::new(Cell::new(0));
        let callbacks = Rc::new(RefCell::new(Vec::new()));
        let observed = callbacks.clone();
        let write = handle.set_property_guarded(
            "target",
            BINDING,
            true,
            hooks(allowed.clone(), checks.clone(), first_send.clone()),
            move |outcome| observed.borrow_mut().push(outcome),
        );
        driver.drain_requests().unwrap();
        let mid = *driver.pending.keys().next().unwrap();
        write_ready(&mut driver, &budget, 0).await;
        let pending_checks = checks.get();
        assert!(pending_checks > 0);
        assert_eq!(first_send.get(), 0);
        assert_eq!(written.get(), 0);
        assert!(callbacks.borrow().is_empty());
        allowed.set(false);
        write_ready(&mut driver, &budget, usize::MAX).await;
        assert!(checks.get() > pending_checks);
        let expected = unsent(CentralError::Withdrawn);
        assert_eq!(write.await, expected);
        assert_eq!(
            callbacks.borrow().as_slice(),
            std::slice::from_ref(&expected)
        );
        assert_eq!(first_send.get(), 0);
        assert_eq!(written.get(), 0);
        assert!(driver.output.is_empty());
        assert!(driver.pending.is_empty());
        assert!(driver.publications.is_empty());
        assert_eq!(handle.shared.outstanding.get(), 0);
        reply(
            &mut driver,
            mid,
            json!({"result":[{"did":"target","code":0}]}),
        );
        driver.finish_all(CentralError::Cancelled);
        assert_eq!(callbacks.borrow().len(), 1);
        assert_eq!(handle.shared.outstanding.get(), 0);
    });
}

#[test]
fn guarded_read_rechecks_after_pending_and_withdraws_without_sending() {
    block_on(async {
        let (handle, mut driver, budget, written) = rig();
        let allowed = Rc::new(Cell::new(true));
        let checks = Rc::new(Cell::new(0));
        let first_send = Rc::new(Cell::new(0));
        let callbacks = Rc::new(RefCell::new(Vec::new()));
        let observed = callbacks.clone();
        let read = handle.read_property_with_timeout_guarded(
            "target",
            BINDING,
            Duration::from_secs(6),
            hooks(allowed.clone(), checks.clone(), first_send.clone()),
            move |outcome| observed.borrow_mut().push(outcome),
        );
        driver.drain_requests().unwrap();
        let mid = *driver.pending.keys().next().unwrap();
        write_ready(&mut driver, &budget, 0).await;
        let pending_checks = checks.get();
        assert!(pending_checks > 0);
        allowed.set(false);
        write_ready(&mut driver, &budget, usize::MAX).await;
        assert!(checks.get() > pending_checks);
        let expected = unsent(CentralError::Withdrawn);
        assert_eq!(read.await, expected);
        assert_eq!(
            callbacks.borrow().as_slice(),
            std::slice::from_ref(&expected)
        );
        assert_eq!(written.get(), 0);
        assert_eq!(first_send.get(), 0);
        assert!(driver.pending.is_empty());
        assert!(driver.output.is_empty());
        assert!(driver.publications.is_empty());
        assert_eq!(handle.shared.outstanding.get(), 0);
        reply(&mut driver, mid, json!({"value":false}));
        driver.finish_all(CentralError::Cancelled);
        assert_eq!(callbacks.borrow().len(), 1);
    });
}

#[test]
fn guarded_write_locks_the_decision_and_reports_first_send_once() {
    block_on(async {
        let (handle, mut driver, budget, written) = rig();
        let allowed = Rc::new(Cell::new(true));
        let checks = Rc::new(Cell::new(0));
        let first_send = Rc::new(Cell::new(0));
        let callbacks = Rc::new(RefCell::new(Vec::new()));
        let observed = callbacks.clone();
        let write = handle.set_property_guarded(
            "target",
            BINDING,
            true,
            hooks(allowed.clone(), checks.clone(), first_send.clone()),
            move |outcome| observed.borrow_mut().push(outcome),
        );
        driver.drain_requests().unwrap();
        let mid = *driver.pending.keys().next().unwrap();
        write_ready(&mut driver, &budget, 0).await;
        assert_eq!(first_send.get(), 0);
        write_ready(&mut driver, &budget, 3).await;
        assert_eq!(written.get(), 3);
        assert_eq!(first_send.get(), 1);
        assert_eq!(driver.pending[&mid].stage, RequestStage::MayHaveBeenSent);
        let committed_checks = checks.get();
        allowed.set(false);
        write_ready(&mut driver, &budget, 2).await;
        write_ready(&mut driver, &budget, usize::MAX).await;
        assert!(written.get() > 5);
        assert_eq!(checks.get(), committed_checks);
        assert_eq!(first_send.get(), 1);
        assert!(callbacks.borrow().is_empty());
        reply(
            &mut driver,
            mid,
            json!({"result":[{"did":"target","code":0}]}),
        );
        let expected = RequestOutcome {
            result: Ok(()),
            stage: RequestStage::BusinessResult,
            timeout: None,
        };
        assert_eq!(write.await, expected);
        assert_eq!(
            callbacks.borrow().as_slice(),
            std::slice::from_ref(&expected)
        );
        assert_eq!(handle.shared.outstanding.get(), 0);
    });
}

#[test]
fn guarded_read_locks_the_decision_after_first_bytes() {
    block_on(async {
        let (handle, mut driver, budget, written) = rig();
        let allowed = Rc::new(Cell::new(true));
        let checks = Rc::new(Cell::new(0));
        let first_send = Rc::new(Cell::new(0));
        let callbacks = Rc::new(Cell::new(0));
        let read = handle.read_property_with_timeout_guarded(
            "target",
            BINDING,
            Duration::from_secs(6),
            hooks(allowed.clone(), checks.clone(), first_send.clone()),
            count_result(&callbacks),
        );
        driver.drain_requests().unwrap();
        let mid = *driver.pending.keys().next().unwrap();
        write_ready(&mut driver, &budget, 3).await;
        assert_eq!(written.get(), 3);
        assert_eq!(first_send.get(), 1);
        let committed_checks = checks.get();
        allowed.set(false);
        handle.cancel_unsent_business();
        driver.drain_cancellations();
        assert!(driver.pending.contains_key(&mid));
        write_ready(&mut driver, &budget, usize::MAX).await;
        assert_eq!(checks.get(), committed_checks);
        assert_eq!(first_send.get(), 1);
        assert!(written.get() > 3);
        reply(&mut driver, mid, json!({"value":false}));
        assert_eq!(
            read.await,
            RequestOutcome {
                result: Ok(false),
                stage: RequestStage::BusinessResult,
                timeout: None,
            }
        );
        assert_eq!(callbacks.get(), 1);
        assert_eq!(handle.shared.outstanding.get(), 0);
    });
}

#[test]
fn a_guard_cancellation_is_cancelled_rather_than_withdrawn() {
    block_on(async {
        let (handle, mut driver, budget, written) = rig();
        let callbacks = Rc::new(Cell::new(0));
        let first_send = Rc::new(Cell::new(0));
        let sent = first_send.clone();
        let write = handle.set_property_guarded(
            "target",
            BINDING,
            true,
            SendHooks::new(|| SendDecision::Cancel, move || sent.set(sent.get() + 1)),
            count_result(&callbacks),
        );
        driver.drain_requests().unwrap();
        write_ready(&mut driver, &budget, usize::MAX).await;
        assert_eq!(write.await, unsent(CentralError::Cancelled));
        assert_eq!(callbacks.get(), 1);
        assert_eq!(first_send.get(), 0);
        assert_eq!(written.get(), 0);
        assert_eq!(handle.shared.outstanding.get(), 0);
        assert!(driver.pending.is_empty());
        assert!(driver.publications.is_empty());
        driver.finish_all(CentralError::Cancelled);
        assert_eq!(callbacks.get(), 1);
    });
}

#[test]
fn cancelling_unsent_business_keeps_writes_and_subscriptions() {
    block_on(async {
        for dispatched in [false, true] {
            let (handle, mut driver, budget, written) = rig();
            let callbacks = Rc::new(Cell::new(0));
            let list = handle.get_devices_detailed(count_result(&callbacks));
            let filtered =
                handle.get_devices_filtered_detailed(&["target".into()], count_result(&callbacks));
            let device = handle.get_device_list_detailed("target", count_result(&callbacks));
            let read = handle.read_property_detailed("target", BINDING, count_result(&callbacks));
            let write = handle.set_property_detailed("target", BINDING, true, |_| {});
            let subscription = handle.subscribe_property("target", BINDING);
            if dispatched {
                driver.drain_requests().unwrap();
            }
            handle.cancel_unsent_business();
            driver.drain_cancellations();
            driver.drain_requests().unwrap();
            assert_eq!(list.await, unsent(CentralError::Cancelled));
            assert_eq!(filtered.await, unsent(CentralError::Cancelled));
            assert_eq!(device.await, unsent(CentralError::Cancelled));
            assert_eq!(read.await, unsent(CentralError::Cancelled));
            assert_eq!(callbacks.get(), 4);
            assert_eq!(written.get(), 0);
            assert_eq!(handle.shared.outstanding.get(), 2);
            assert_eq!(driver.pending.len(), 1);
            assert_eq!(driver.publications.len(), 1);
            assert_eq!(driver.subscriptions.len(), 1);
            assert_eq!(driver.output.len(), 2);
            let mid = *driver.pending.keys().next().unwrap();
            assert_eq!(driver.pending[&mid].kind.operation(), "set");
            let subscription_id = *driver.subscriptions.keys().next().unwrap();
            write_ready(&mut driver, &budget, usize::MAX).await;
            write_ready(&mut driver, &budget, usize::MAX).await;
            reply(
                &mut driver,
                mid,
                json!({"result":[{"did":"target","code":0}]}),
            );
            driver
                .packet(
                    Packet::SubAck(SubAck::new(
                        subscription_id,
                        vec![SubscribeReasonCode::QoS2],
                    )),
                    &mut |_| {},
                )
                .unwrap();
            assert_eq!(write.await.result, Ok(()));
            subscription.await.unwrap();
            assert_eq!(callbacks.get(), 4);
            assert_eq!(handle.shared.outstanding.get(), 0);
            assert_eq!(driver.property_topics.len(), 1);
        }
    });
}

#[test]
fn cancelling_unsent_business_preserves_partial_reads_and_qos2_frames() {
    block_on(async {
        let (handle, mut driver, budget, written) = rig();
        let read = handle.read_property_detailed("target", BINDING, |_| {});
        driver.drain_requests().unwrap();
        let mid = *driver.pending.keys().next().unwrap();
        let pkid = *driver.publications.keys().next().unwrap();
        write_ready(&mut driver, &budget, 3).await;
        let list = handle.get_devices_detailed(|_| {});
        handle.cancel_unsent_business();
        driver.drain_cancellations();
        assert_eq!(list.await, unsent(CentralError::Cancelled));
        assert_eq!(driver.output.front().unwrap().offset, 3);
        assert!(driver.pending.contains_key(&mid));
        assert!(driver.publications.contains_key(&pkid));
        write_ready(&mut driver, &budget, usize::MAX).await;
        driver
            .packet(
                Packet::PubRec(mqttbytes::v5::PubRec::new(pkid)),
                &mut |_| {},
            )
            .unwrap();
        assert_eq!(driver.output.len(), 1);
        handle.cancel_unsent_business();
        driver.drain_cancellations();
        assert_eq!(driver.output.len(), 1);
        assert!(driver.publications.contains_key(&pkid));
        write_ready(&mut driver, &budget, usize::MAX).await;
        driver
            .packet(
                Packet::PubComp(mqttbytes::v5::PubComp::new(pkid)),
                &mut |_| {},
            )
            .unwrap();
        assert!(driver.publications.is_empty());
        reply(&mut driver, mid, json!({"value":true}));
        assert_eq!(read.await.result, Ok(true));
        assert!(written.get() > 3);
        assert_eq!(handle.shared.outstanding.get(), 0);
    });
}

#[test]
fn cancelling_unsent_business_releases_capacity_once() {
    block_on(async {
        for dispatched in [false, true] {
            let (handle, mut driver, _, _) = rig();
            let callbacks = Rc::new(Cell::new(0));
            for _ in 0..31 {
                drop(handle.get_devices_detailed(count_result(&callbacks)));
                drop(handle.read_property_detailed("target", BINDING, count_result(&callbacks)));
            }
            let write = handle.set_property_detailed("target", BINDING, true, |_| {});
            let subscription = handle.subscribe_property("target", BINDING);
            assert_eq!(handle.shared.outstanding.get(), LIMIT);
            assert_eq!(
                handle
                    .set_property_detailed("full", BINDING, false, |_| {})
                    .await,
                unsent(CentralError::QueueFull)
            );
            if dispatched {
                driver.drain_requests().unwrap();
            }
            handle.cancel_unsent_business();
            driver.drain_cancellations();
            assert_eq!(callbacks.get(), LIMIT - 2);
            assert_eq!(handle.shared.outstanding.get(), 2);
            handle.cancel_unsent_business();
            driver.drain_cancellations();
            assert_eq!(callbacks.get(), LIMIT - 2);
            assert_eq!(handle.shared.outstanding.get(), 2);
            let replacement = handle.read_property_detailed("fresh", BINDING, |_| {});
            assert_eq!(handle.shared.outstanding.get(), 3);
            driver.drain_requests().unwrap();
            driver.finish_all(CentralError::Cancelled);
            assert_eq!(replacement.await, unsent(CentralError::Cancelled));
            assert_eq!(write.await, unsent(CentralError::Cancelled));
            assert_eq!(subscription.await, Err(CentralError::Cancelled));
            assert_eq!(callbacks.get(), LIMIT - 2);
            assert_eq!(handle.shared.outstanding.get(), 0);
        }
    });
}

#[test]
fn a_response_timeout_callback_cancels_unsent_lists_before_the_same_socket_poll() {
    block_on(async {
        let (handle, mut driver, budget, written) = rig();
        let callbacks = Rc::new(RefCell::new(Vec::new()));
        let observed = callbacks.clone();
        let cancel = handle.clone();
        let write = handle.set_property_detailed("target", BINDING, true, move |outcome| {
            observed.borrow_mut().push(outcome);
            cancel.cancel_unsent_business();
        });
        driver.drain_requests().unwrap();
        let write_id = *driver.pending.keys().next().unwrap();
        let publication_id = *driver.publications.keys().next().unwrap();
        write_ready(&mut driver, &budget, usize::MAX).await;
        assert_eq!(
            driver.pending[&write_id].stage,
            RequestStage::MayHaveBeenSent
        );
        let sent = written.get();
        assert!(sent > 0);

        let list_callbacks = Rc::new(Cell::new(0));
        let list = handle.get_devices_detailed(count_result(&list_callbacks));
        driver.drain_requests().unwrap();
        assert_eq!(driver.output.len(), 1);
        assert_eq!(driver.output.front().unwrap().offset, 0);
        driver.pending.get_mut(&write_id).unwrap().deadline =
            Instant::now() - Duration::from_secs(1);
        driver
            .publications
            .get_mut(&publication_id)
            .unwrap()
            .deadline = Instant::now() + Duration::from_secs(60);

        write_ready(&mut driver, &budget, usize::MAX).await;
        assert_eq!(written.get(), sent, "the cancelled list reached the socket");
        assert_eq!(list.await, unsent(CentralError::Cancelled));
        let expected = RequestOutcome {
            result: Err(CentralError::Timeout),
            stage: RequestStage::MayHaveBeenSent,
            timeout: Some(RequestTimeout::Response),
        };
        assert_eq!(write.await, expected);
        assert_eq!(
            callbacks.borrow().as_slice(),
            std::slice::from_ref(&expected)
        );
        assert_eq!(list_callbacks.get(), 1);
        assert!(driver.pending.is_empty());
        assert!(driver.output.is_empty());
        assert_eq!(driver.publications.len(), 1);
        assert!(driver.publications.contains_key(&publication_id));
        assert_eq!(handle.shared.outstanding.get(), 0);
        driver.finish_all(CentralError::Cancelled);
        assert_eq!(callbacks.borrow().len(), 1);
        assert_eq!(list_callbacks.get(), 1);
    });
}

#[test]
fn detailed_outcomes_keep_unsent_queue_and_output_timeouts_unsent() {
    block_on(async {
        for dispatched in [false, true] {
            let (handle, mut driver, budget, written) = rig();
            let write = handle.set_property_detailed("target", BINDING, true, |_| {});
            if dispatched {
                driver.drain_requests().unwrap();
                let past = Instant::now() - Duration::from_secs(1);
                driver.pending.values_mut().next().unwrap().deadline = past;
                driver.output.front_mut().unwrap().deadline = past;
            } else {
                handle
                    .shared
                    .queue
                    .borrow_mut()
                    .front_mut()
                    .unwrap()
                    .deadline = Instant::now() - Duration::from_secs(1);
            }
            driver.expire().unwrap();
            assert_eq!(
                write.await,
                RequestOutcome {
                    result: Err(CentralError::Timeout),
                    stage: RequestStage::NotSent,
                    timeout: Some(RequestTimeout::Queue)
                }
            );
            write_ready(&mut driver, &budget, usize::MAX).await;
            assert_eq!(written.get(), 0);
            assert!(driver.pending.is_empty());
            assert!(driver.output.is_empty());
        }
    });
}

#[test]
fn detailed_outcomes_only_mark_possible_send_after_socket_bytes() {
    block_on(async {
        let (handle, mut driver, budget, written) = rig();
        let write = handle.set_property_detailed("target", BINDING, true, |_| {});
        driver.drain_requests().unwrap();
        write_ready(&mut driver, &budget, 0).await;
        assert_eq!(
            driver.pending.values().next().unwrap().stage,
            RequestStage::NotSent
        );
        write_ready(&mut driver, &budget, 3).await;
        assert_eq!(written.get(), 3);
        assert_eq!(
            driver.pending.values().next().unwrap().stage,
            RequestStage::MayHaveBeenSent
        );
        driver.output.front_mut().unwrap().deadline = Instant::now() - Duration::from_secs(1);
        assert_eq!(driver.expire(), Err(CentralError::Timeout));
        driver.finish_all(CentralError::Timeout);
        assert_eq!(
            write.await,
            RequestOutcome {
                result: Err(CentralError::Timeout),
                stage: RequestStage::MayHaveBeenSent,
                timeout: Some(RequestTimeout::Transport)
            }
        );
        assert_eq!(written.get(), 3);
    });
}

#[test]
fn detailed_outcomes_distinguish_response_timeout_from_transport_and_business_results() {
    block_on(async {
        for case in ["response", "transport", "rejected", "accepted", "malformed"] {
            let (handle, mut driver, budget, _) = rig();
            let callbacks = Rc::new(RefCell::new(Vec::new()));
            let observed = callbacks.clone();
            let write = handle.set_property_detailed("target", BINDING, true, move |outcome| {
                observed.borrow_mut().push(outcome)
            });
            driver.drain_requests().unwrap();
            let mid = *driver.pending.keys().next().unwrap();
            write_ready(&mut driver, &budget, usize::MAX).await;
            let expected = match case {
                "response" => {
                    driver.pending.get_mut(&mid).unwrap().deadline =
                        Instant::now() - Duration::from_secs(1);
                    driver.expire().unwrap();
                    RequestOutcome {
                        result: Err(CentralError::Timeout),
                        stage: RequestStage::MayHaveBeenSent,
                        timeout: Some(RequestTimeout::Response),
                    }
                }
                "transport" => {
                    driver.publications.values_mut().next().unwrap().deadline =
                        Instant::now() - Duration::from_secs(1);
                    assert_eq!(driver.expire(), Err(CentralError::Timeout));
                    driver.finish_all(CentralError::Timeout);
                    RequestOutcome {
                        result: Err(CentralError::Timeout),
                        stage: RequestStage::MayHaveBeenSent,
                        timeout: Some(RequestTimeout::Transport),
                    }
                }
                "rejected" => {
                    reply(&mut driver, mid, json!({"error":{"code":-704042011}}));
                    RequestOutcome {
                        result: Err(CentralError::Rejected(-704042011)),
                        stage: RequestStage::BusinessResult,
                        timeout: None,
                    }
                }
                "accepted" => {
                    reply(
                        &mut driver,
                        mid,
                        json!({"result":[{"did":"target","code":0}]}),
                    );
                    RequestOutcome {
                        result: Ok(()),
                        stage: RequestStage::BusinessResult,
                        timeout: None,
                    }
                }
                _ => {
                    reply(
                        &mut driver,
                        mid,
                        json!({"result":[{"did":"foreign","code":0}]}),
                    );
                    RequestOutcome {
                        result: Err(CentralError::Protocol),
                        stage: RequestStage::MayHaveBeenSent,
                        timeout: None,
                    }
                }
            };
            assert_eq!(
                callbacks.borrow().as_slice(),
                std::slice::from_ref(&expected)
            );
            assert_eq!(write.await, expected);
            reply(
                &mut driver,
                mid,
                json!({"result":[{"did":"target","code":0}]}),
            );
            assert_eq!(callbacks.borrow().len(), 1);
        }
    });
}

#[test]
fn detailed_cancellation_removes_unsent_frames_without_replay() {
    block_on(async {
        for dispatched in [false, true] {
            let (handle, mut driver, budget, written) = rig();
            let write = handle.set_property_detailed("target", BINDING, true, |_| {});
            if dispatched {
                driver.drain_requests().unwrap();
            }
            handle.cancel_device("target").unwrap();
            driver.drain_cancellations();
            assert_eq!(
                write.await,
                RequestOutcome {
                    result: Err(CentralError::Cancelled),
                    stage: RequestStage::NotSent,
                    timeout: None
                }
            );
            write_ready(&mut driver, &budget, usize::MAX).await;
            assert_eq!(written.get(), 0);
            assert!(driver.pending.is_empty());
        }
    });
}

#[test]
fn property_unsubscription_keeps_business_writes_and_retires_late_subacks() {
    block_on(async {
        let (handle, mut driver, budget, _) = rig();
        let subscribed = handle.subscribe_property("target", BINDING);
        driver.drain_requests().unwrap();
        let pkid = *driver.subscriptions.keys().next().unwrap();
        write_ready(&mut driver, &budget, usize::MAX).await;
        driver
            .packet(
                Packet::SubAck(SubAck::new(pkid, vec![SubscribeReasonCode::QoS2])),
                &mut |_| {},
            )
            .unwrap();
        subscribed.await.unwrap();
        let pending = handle.subscribe_property("target", PropertyBinding { siid: 3, piid: 1 });
        let write = handle.set_property_detailed("target", BINDING, true, |_| {});
        driver.drain_requests().unwrap();
        let subscription_id = *driver.subscriptions.keys().next().unwrap();
        let mid = *driver.pending.keys().next().unwrap();
        write_ready(&mut driver, &budget, usize::MAX).await;
        write_ready(&mut driver, &budget, usize::MAX).await;
        let read = handle.read_property_detailed("target", BINDING, |_| {});
        let unsubscribe = handle.unsubscribe_properties("target");
        driver.drain_cancellations();
        assert!(driver.property_topics.is_empty());
        assert!(driver.pending.contains_key(&mid));
        assert_eq!(pending.await, Err(CentralError::Cancelled));
        driver.drain_requests().unwrap();
        let read_id = *driver.pending.keys().find(|id| **id != mid).unwrap();
        let unsubscription_id = *driver
            .subscriptions
            .keys()
            .find(|id| **id != subscription_id)
            .unwrap();
        write_ready(&mut driver, &budget, usize::MAX).await;
        driver
            .packet(
                Packet::SubAck(SubAck::new(
                    subscription_id,
                    vec![SubscribeReasonCode::QoS2],
                )),
                &mut |_| {},
            )
            .unwrap();
        assert!(driver.property_topics.is_empty());
        reply(
            &mut driver,
            mid,
            json!({"result":[{"did":"target","code":0}]}),
        );
        assert_eq!(write.await.result, Ok(()));
        reply(&mut driver, read_id, json!({"value":false}));
        assert_eq!(
            read.await,
            RequestOutcome {
                result: Ok(false),
                stage: RequestStage::BusinessResult,
                timeout: None
            }
        );
        let mut ack = mqttbytes::v5::UnsubAck::new(unsubscription_id);
        ack.reasons = vec![mqttbytes::v5::UnsubAckReason::Success; 2];
        driver.packet(Packet::UnsubAck(ack), &mut |_| {}).unwrap();
        unsubscribe.await.unwrap();
    });
}

#[test]
fn a_qos2_property_received_before_unsubscription_cannot_reappear_after_resubscription() {
    block_on(async {
        let (handle, mut driver, budget, _) = rig();
        let subscribed = handle.subscribe_property("target", BINDING);
        driver.drain_requests().unwrap();
        let pkid = *driver.subscriptions.keys().next().unwrap();
        write_ready(&mut driver, &budget, usize::MAX).await;
        driver
            .packet(
                Packet::SubAck(SubAck::new(pkid, vec![SubscribeReasonCode::QoS2])),
                &mut |_| {},
            )
            .unwrap();
        subscribed.await.unwrap();
        let mut publish = Publish::new(
            "client/appMsg/notify/iot/target/property/2.1",
            QoS::ExactlyOnce,
            wire::encode_mips(
                1,
                None,
                &json!({"did":"target","siid":2,"piid":1,"value":true}),
            ),
        );
        publish.pkid = 55;
        let events = Rc::new(RefCell::new(Vec::new()));
        let mut on_event = |event| events.borrow_mut().push(event);
        driver
            .packet(Packet::Publish(publish), &mut on_event)
            .unwrap();
        assert!(events.borrow().is_empty());
        let unsubscribe = handle.unsubscribe_properties("target");
        driver.drain_cancellations();
        driver.drain_requests().unwrap();
        let unsubscription_id = *driver.subscriptions.keys().next().unwrap();
        let mut ack = mqttbytes::v5::UnsubAck::new(unsubscription_id);
        ack.reasons = vec![mqttbytes::v5::UnsubAckReason::Success];
        driver.packet(Packet::UnsubAck(ack), &mut on_event).unwrap();
        unsubscribe.await.unwrap();
        let subscribed = handle.subscribe_property("target", BINDING);
        driver.drain_requests().unwrap();
        let subscription_id = *driver.subscriptions.keys().next().unwrap();
        driver
            .packet(
                Packet::SubAck(SubAck::new(
                    subscription_id,
                    vec![SubscribeReasonCode::QoS2],
                )),
                &mut on_event,
            )
            .unwrap();
        subscribed.await.unwrap();
        driver
            .packet(
                Packet::PubRel(mqttbytes::v5::PubRel::new(55)),
                &mut on_event,
            )
            .unwrap();
        assert!(
            events.borrow().is_empty(),
            "the old observation became active again"
        );
        let mut fresh = Publish::new(
            "client/appMsg/notify/iot/target/property/2.1",
            QoS::ExactlyOnce,
            wire::encode_mips(
                2,
                None,
                &json!({"did":"target","siid":2,"piid":1,"value":false}),
            ),
        );
        fresh.pkid = 56;
        driver
            .packet(Packet::Publish(fresh), &mut on_event)
            .unwrap();
        driver
            .packet(
                Packet::PubRel(mqttbytes::v5::PubRel::new(56)),
                &mut on_event,
            )
            .unwrap();
        assert_eq!(
            events.borrow().as_slice(),
            &[CentralEvent::Property {
                did: "target".into(),
                binding: BINDING,
                value: false
            }]
        );
    });
}
