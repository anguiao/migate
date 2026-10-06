use super::*;
use crate::xiaomi::central::{RequestOutcome, RequestStage, RequestTimeout};
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
