//! Event topics and deliveries keep their own subscription ownership.
use super::*;
use crate::xiaomi::capability::EventBinding;

const EVENT: EventBinding = EventBinding { siid: 2, eiid: 1 };
const TOPIC: &str = "device/target/up/event_occured/2/1";

fn event(retain: bool, payload: Value) -> Packet {
    let mut publish = Publish::new(TOPIC, QoS::AtMostOnce, payload.to_string());
    publish.retain = retain;
    Packet::Publish(publish)
}

#[test]
fn event_notifications_match_eiid_and_ignore_retained_events() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let collected = events.clone();
    let (finished, done) = flume::bounded(1);
    future::block_on(exercise(
        timing(),
        move |mut broker| async move {
            broker.setup().await;
            let pkid = broker.subscribe(TOPIC).await;
            broker
                .send(event(
                    false,
                    json!({"params":{"siid":2,"eiid":1,"arguments":[]}}),
                ))
                .await;
            broker.accept(pkid).await;
            for payload in [
                json!({"uid":"other","params":{"siid":2,"eiid":1,"arguments":[]}}),
                json!({"params":{"did":"other","siid":2,"eiid":1,"arguments":[]}}),
                json!({"params":{"siid":2,"piid":1,"arguments":[]}}),
                json!({"params":{"siid":2,"eiid":9,"arguments":[]}}),
                json!({"params":{"siid":2,"eiid":1,"arguments":false}}),
            ] {
                broker.send(event(false, payload)).await;
            }
            let payload = json!({"timestamp":101,"params":{"did":"target","siid":2,"eiid":1,"arguments":[{"piid":1005,"value":32.5}]}});
            broker.send(event(true, payload.clone())).await;
            broker.send(event(false, payload)).await;
            done.recv_async().await.unwrap();
        },
        move |handle, driver| async move {
            let stop = handle.clone();
            let (result, subscribed) = future::zip(
                driver.run(move |event| {
                    collected.borrow_mut().push(event);
                    finished.send(()).unwrap();
                    stop.close();
                }),
                handle.subscribe_event("target", EVENT),
            )
            .await;
            result.unwrap();
            subscribed.unwrap();
        },
    ));
    assert_eq!(
        events.borrow().as_slice(),
        &[NotificationEvent::EventOccurred {
            did: "target".into(),
            binding: EVENT,
            arguments: vec![json!({"piid":1005,"value":32.5})],
            source_time: Some(101),
        }]
    );
}

fn qos2_event(pkid: u16) -> Packet {
    let Packet::Publish(mut publish) =
        event(false, json!({"params":{"siid":2,"eiid":1,"arguments":[]}}))
    else {
        unreachable!()
    };
    publish.qos = QoS::ExactlyOnce;
    publish.pkid = pkid;
    Packet::Publish(publish)
}

async fn release(broker: &mut Broker, pkid: u16) {
    broker.send(Packet::PubRel(PubRel::new(pkid))).await;
    assert!(matches!(broker.next().await, Packet::PubComp(ack) if ack.pkid == pkid));
}

async fn unsubscribe(broker: &mut Broker, expected: &[&str]) {
    let Packet::Unsubscribe(unsubscribe) = broker.next().await else {
        panic!("UNSUBSCRIBE expected")
    };
    let mut filters = unsubscribe.filters.clone();
    filters.sort();
    let mut expected = expected
        .iter()
        .map(|value| (*value).to_string())
        .collect::<Vec<_>>();
    expected.sort();
    assert_eq!(filters, expected);
    broker
        .send(Packet::UnsubAck(UnsubAck {
            pkid: unsubscribe.pkid,
            reasons: vec![UnsubAckReason::Success; filters.len()],
            properties: None,
        }))
        .await;
}

#[test]
fn cloud_event_generations_and_scoped_unsubscription_preserve_properties() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let collected = events.clone();
    let observed = events.clone();
    let (invalidate, invalidating) = flume::bounded(1);
    let (resume, resumed) = flume::bounded(1);
    let (resubscribe, resubscribing) = flume::bounded(1);
    let (finished, done) = flume::bounded(1);
    let (completed, completion) = flume::bounded(1);
    future::block_on(exercise(
        timing(),
        move |mut broker| async move {
            broker.setup().await;
            let pkid = broker.subscribe(TOPIC).await;
            broker.accept(pkid).await;
            let pkid = broker
                .subscribe("device/target/up/properties_changed/2/1")
                .await;
            broker.accept(pkid).await;
            broker.send(qos2_event(81)).await;
            assert!(matches!(broker.next().await, Packet::PubRec(ack) if ack.pkid == 81));
            invalidate.send(()).unwrap();
            resumed.recv_async().await.unwrap();
            release(&mut broker, 81).await;
            let fresh = qos2_event(82);
            broker.send(fresh.clone()).await;
            assert!(matches!(broker.next().await, Packet::PubRec(ack) if ack.pkid == 82));
            let Packet::Publish(mut duplicate) = fresh else {
                unreachable!()
            };
            duplicate.dup = true;
            broker.send(Packet::Publish(duplicate)).await;
            assert!(matches!(broker.next().await, Packet::PubRec(ack) if ack.pkid == 82));
            release(&mut broker, 82).await;
            release(&mut broker, 82).await;
            broker.send(qos2_event(83)).await;
            assert!(matches!(broker.next().await, Packet::PubRec(ack) if ack.pkid == 83));
            resubscribe.send(()).unwrap();
            unsubscribe(&mut broker, &[TOPIC]).await;
            let pkid = broker.subscribe(TOPIC).await;
            broker.accept(pkid).await;
            release(&mut broker, 83).await;
            broker.send(qos2_event(84)).await;
            assert!(matches!(broker.next().await, Packet::PubRec(ack) if ack.pkid == 84));
            release(&mut broker, 84).await;
            completed.send(()).unwrap();
            unsubscribe(&mut broker, &[TOPIC]).await;
            broker
                .send(event(
                    false,
                    json!({"params":{"siid":2,"eiid":1,"arguments":[]}}),
                ))
                .await;
            broker.property(json!(true)).await;
            done.recv_async().await.unwrap();
        },
        move |handle, driver| async move {
            let (result, ()) =
                future::zip(
                    driver.run(move |event| collected.borrow_mut().push(event)),
                    async {
                        handle.subscribe_event("target", EVENT).await.unwrap();
                        handle.subscribe_property("target", POWER).await.unwrap();
                        invalidating.recv_async().await.unwrap();
                        handle.invalidate_received_event("target", EVENT);
                        resume.send(()).unwrap();
                        resubscribing.recv_async().await.unwrap();
                        handle.unsubscribe_events("target").await.unwrap();
                        handle.subscribe_event("target", EVENT).await.unwrap();
                        completion.recv_async().await.unwrap();
                        assert_eq!(
                            observed
                                .borrow()
                                .iter()
                                .filter(|event| matches!(
                                    event,
                                    NotificationEvent::EventOccurred { .. }
                                ))
                                .count(),
                            2
                        );
                        handle.unsubscribe_events("target").await.unwrap();
                        loop {
                            if observed.borrow().iter().any(|event| {
                                matches!(event, NotificationEvent::PropertyValue { .. })
                            }) {
                                break;
                            }
                            future::yield_now().await;
                        }
                        assert_eq!(observed.borrow().len(), 3);
                        handle.close();
                    },
                )
                .await;
            result.unwrap();
            finished.send(()).unwrap();
        },
    ));
    assert_eq!(
        events.borrow().last(),
        Some(&NotificationEvent::PropertyValue {
            did: "target".into(),
            binding: POWER,
            value: json!(true),
            update_time: None,
        })
    );
}

#[test]
fn cloud_device_unsubscription_includes_events_and_rejection_stays_isolated() {
    let (finished, done) = flume::bounded(1);
    future::block_on(exercise(
        timing(),
        move |mut broker| async move {
            broker.setup().await;
            let pkid = broker.subscribe(TOPIC).await;
            broker
                .send(Packet::SubAck(SubAck::new(
                    pkid,
                    vec![SubscribeReasonCode::NotAuthorized],
                )))
                .await;
            let pkid = broker.subscribe(TOPIC).await;
            broker.accept(pkid).await;
            let pkid = broker
                .subscribe("device/target/up/properties_changed/2/1")
                .await;
            broker.accept(pkid).await;
            let pkid = broker.subscribe("device/target/state/#").await;
            broker.accept(pkid).await;
            unsubscribe(
                &mut broker,
                &[
                    TOPIC,
                    "device/target/up/properties_changed/2/1",
                    "device/target/state/#",
                ],
            )
            .await;
            done.recv_async().await.unwrap();
        },
        move |handle, driver| async move {
            let (result, ()) = future::zip(driver.run(|_| {}), async {
                assert_eq!(
                    handle.subscribe_event("target", EVENT).await,
                    Err(NotificationError::Unauthorized)
                );
                handle.subscribe_event("target", EVENT).await.unwrap();
                handle.subscribe_property("target", POWER).await.unwrap();
                handle.subscribe_online("target").await.unwrap();
                handle.unsubscribe_device("target").await.unwrap();
                assert!(handle.shared.registrations.borrow().is_empty());
                assert_eq!(handle.shared.outstanding.get(), 0);
                handle.close();
            })
            .await;
            result.unwrap();
            finished.send(()).unwrap();
        },
    ));
}

#[test]
fn cloud_event_requests_validate_input_bound_capacity_and_cancel_on_close() {
    let (finished, done) = flume::bounded(1);
    future::block_on(exercise(
        timing(),
        move |mut broker| async move {
            broker.setup().await;
            done.recv_async().await.unwrap();
        },
        move |handle, driver| async move {
            for (did, binding) in [
                ("target", EventBinding { siid: 0, eiid: 1 }),
                ("target", EventBinding { siid: 2, eiid: 0 }),
                ("bad/#", EVENT),
            ] {
                assert_eq!(
                    handle.subscribe_event(did, binding).await,
                    Err(NotificationError::InvalidInput)
                );
            }
            let pending = (1..=super::super::LIMIT)
                .map(|eiid| {
                    handle.subscribe_event(
                        "target",
                        EventBinding {
                            siid: 2,
                            eiid: eiid as u32,
                        },
                    )
                })
                .collect::<Vec<_>>();
            assert_eq!(
                handle.subscribe_event("other", EVENT).await,
                Err(NotificationError::QueueFull)
            );
            drop(driver);
            for request in pending {
                assert_eq!(request.await, Err(NotificationError::Cancelled));
            }
            assert_eq!(handle.shared.outstanding.get(), 0);
            assert_eq!(
                handle.subscribe_event("target", EVENT).await,
                Err(NotificationError::Cancelled)
            );
            finished.send(()).unwrap();
        },
    ));
}

#[test]
fn cloud_event_subscription_timeout_does_not_affect_existing_property_delivery() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let collected = events.clone();
    let observed = events.clone();
    let (timed_out, timeout) = flume::bounded(1);
    let (finished, done) = flume::bounded(1);
    future::block_on(exercise(
        timing(),
        move |mut broker| async move {
            broker.setup().await;
            let pkid = broker
                .subscribe("device/target/up/properties_changed/2/1")
                .await;
            broker.accept(pkid).await;
            let pkid = broker.subscribe(TOPIC).await;
            timeout.recv_async().await.unwrap();
            broker.accept(pkid).await;
            broker
                .send(event(
                    false,
                    json!({"params":{"siid":2,"eiid":1,"arguments":[]}}),
                ))
                .await;
            broker.property(json!(false)).await;
            done.recv_async().await.unwrap();
        },
        move |handle, driver| async move {
            let (result, ()) = future::zip(
                driver.run(move |event| collected.borrow_mut().push(event)),
                async {
                    handle.subscribe_property("target", POWER).await.unwrap();
                    assert_eq!(
                        handle.subscribe_event("target", EVENT).await,
                        Err(NotificationError::Timeout)
                    );
                    timed_out.send(()).unwrap();
                    loop {
                        if !observed.borrow().is_empty() {
                            break;
                        }
                        future::yield_now().await;
                    }
                    assert!(matches!(
                        observed.borrow().as_slice(),
                        [NotificationEvent::PropertyValue { .. }]
                    ));
                    handle.close();
                },
            )
            .await;
            result.unwrap();
            finished.send(()).unwrap();
        },
    ));
}
