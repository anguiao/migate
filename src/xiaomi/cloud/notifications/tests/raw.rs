use super::*;

fn member_publish(pkid: u16, siid: u32, piid: u32, value: Value) -> Packet {
    let mut publish = Publish::new(
        format!("device/target/up/properties_changed/{siid}/{piid}"),
        QoS::ExactlyOnce,
        json!({"params":{"siid":siid,"piid":piid,"value":value}}).to_string(),
    );
    publish.pkid = pkid;
    Packet::Publish(publish)
}

/// The client closes only after the broker confirms the final PUBCOMP, so the
/// driver shutdown never races an outstanding acknowledgement.
async fn close_after(handle: NotificationHandle, completion: flume::Receiver<()>) {
    completion.recv_async().await.unwrap();
    handle.close();
}

#[test]
fn raw_member_topic_delivers_valid_property_events() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let collected = events.clone();
    let (completed, completion) = flume::bounded(1);
    let (closed, closed_rx) = flume::bounded(1);
    future::block_on(exercise(
        timing(),
        move |mut broker| async move {
            broker.setup().await;
            let pkid = broker
                .subscribe("device/target/up/properties_changed/#")
                .await;
            broker.accept(pkid).await;
            for (pkid, value) in [(41, json!(true)), (42, json!(37.5))] {
                broker.send(member_publish(pkid, 2, 1, value)).await;
                assert!(matches!(broker.next().await, Packet::PubRec(ack) if ack.pkid == pkid));
                broker.send(Packet::PubRel(PubRel::new(pkid))).await;
                assert!(matches!(broker.next().await, Packet::PubComp(ack) if ack.pkid == pkid));
            }
            completed.send(()).unwrap();
            closed_rx.recv_async().await.unwrap();
        },
        move |handle, driver| async move {
            let stop = handle.clone();
            let (result, (subscription, ())) = future::zip(
                driver.run(move |event| collected.borrow_mut().push(event)),
                future::zip(
                    handle.subscribe_raw_properties("target"),
                    close_after(stop, completion),
                ),
            )
            .await;
            result.unwrap();
            subscription.unwrap();
            closed.send(()).unwrap();
        },
    ));
    assert_eq!(
        *events.borrow(),
        [
            NotificationEvent::MemberProperty {
                did: "target".into(),
                siid: 2,
                piid: 1,
                value: json!(true),
                retained: false,
            },
            NotificationEvent::MemberProperty {
                did: "target".into(),
                siid: 2,
                piid: 1,
                value: json!(37.5),
                retained: false,
            },
        ]
    );
}

#[test]
fn raw_member_topics_reject_invalid_properties() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let collected = events.clone();
    let (completed, completion) = flume::bounded(1);
    let (closed, closed_rx) = flume::bounded(1);
    future::block_on(exercise(
        timing(),
        move |mut broker| async move {
            broker.setup().await;
            let pkid = broker
                .subscribe("device/target/up/properties_changed/#")
                .await;
            broker.accept(pkid).await;
            let invalid = [
                json!({"params":{"siid":0,"piid":1,"value":true}}),
                json!({"params":{"siid":2,"piid":0,"value":true}}),
                json!({"params":{"siid":2,"piid":1}}),
                json!({"params":{"siid":2,"piid":1,"value":"on"}}),
                json!({"did":"other","params":{"siid":2,"piid":1,"value":true}}),
                json!({"params":{"did":"other","siid":2,"piid":1,"value":true}}),
            ];
            for (index, payload) in invalid.into_iter().enumerate() {
                let mut publish = Publish::new(
                    "device/target/up/properties_changed/2/1",
                    QoS::AtMostOnce,
                    payload.to_string(),
                );
                publish.pkid = index as u16 + 51;
                broker.send(Packet::Publish(publish)).await;
            }
            broker.send(member_publish(61, 2, 2, json!(false))).await;
            assert!(matches!(broker.next().await, Packet::PubRec(ack) if ack.pkid == 61));
            broker.send(Packet::PubRel(PubRel::new(61))).await;
            assert!(matches!(broker.next().await, Packet::PubComp(ack) if ack.pkid == 61));
            completed.send(()).unwrap();
            closed_rx.recv_async().await.unwrap();
        },
        move |handle, driver| async move {
            let stop = handle.clone();
            let (result, (subscription, ())) = future::zip(
                driver.run(move |event| collected.borrow_mut().push(event)),
                future::zip(
                    handle.subscribe_raw_properties("target"),
                    close_after(stop, completion),
                ),
            )
            .await;
            result.unwrap();
            subscription.unwrap();
            closed.send(()).unwrap();
        },
    ));
    assert_eq!(
        *events.borrow(),
        [NotificationEvent::MemberProperty {
            did: "target".into(),
            siid: 2,
            piid: 2,
            value: json!(false),
            retained: false,
        }]
    );
}

#[test]
fn exact_property_registration_wins_over_the_raw_wildcard() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let collected = events.clone();
    let (completed, completion) = flume::bounded(1);
    let (closed, closed_rx) = flume::bounded(1);
    future::block_on(exercise(
        timing(),
        move |mut broker| async move {
            broker.setup().await;
            let pkid = broker
                .subscribe("device/target/up/properties_changed/2/1")
                .await;
            broker.accept(pkid).await;
            let pkid = broker
                .subscribe("device/target/up/properties_changed/#")
                .await;
            broker.accept(pkid).await;
            broker.send(received_property(71, true)).await;
            assert!(matches!(broker.next().await, Packet::PubRec(ack) if ack.pkid == 71));
            broker.send(Packet::PubRel(PubRel::new(71))).await;
            assert!(matches!(broker.next().await, Packet::PubComp(ack) if ack.pkid == 71));
            completed.send(()).unwrap();
            closed_rx.recv_async().await.unwrap();
        },
        move |handle, driver| async move {
            let stop = handle.clone();
            let (result, (_, ())) = future::zip(
                driver.run(move |event| collected.borrow_mut().push(event)),
                future::zip(
                    async {
                        handle.subscribe_property("target", POWER).await.unwrap();
                        handle.subscribe_raw_properties("target").await.unwrap();
                    },
                    close_after(stop, completion),
                ),
            )
            .await;
            result.unwrap();
            closed.send(()).unwrap();
        },
    ));
    assert_eq!(
        *events.borrow(),
        [NotificationEvent::Property {
            did: "target".into(),
            binding: POWER,
            value: true,
        }]
    );
}

#[test]
fn raw_member_wildcard_is_retired_with_property_deliveries() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let collected = events.clone();
    let (completed, completion) = flume::bounded(1);
    let (closed, closed_rx) = flume::bounded(1);
    // The runner unsubscribes only after the broker observed the PUBREC, so
    // the captured QoS2 receipt is real and never a setup race.
    let (captured, captured_rx) = flume::bounded(1);
    future::block_on(exercise(
        timing(),
        move |mut broker| async move {
            broker.setup().await;
            let pkid = broker
                .subscribe("device/target/up/properties_changed/#")
                .await;
            broker.accept(pkid).await;
            broker.send(member_publish(81, 2, 1, json!(true))).await;
            assert!(matches!(broker.next().await, Packet::PubRec(ack) if ack.pkid == 81));
            captured.send(()).unwrap();
            let Packet::Unsubscribe(unsubscribe) = broker.next().await else {
                panic!("UNSUBSCRIBE expected");
            };
            assert_eq!(
                unsubscribe.filters[0],
                "device/target/up/properties_changed/#"
            );
            broker
                .send(Packet::UnsubAck(UnsubAck {
                    pkid: unsubscribe.pkid,
                    reasons: vec![UnsubAckReason::Success],
                    properties: None,
                }))
                .await;
            broker.send(Packet::PubRel(PubRel::new(81))).await;
            assert!(matches!(broker.next().await, Packet::PubComp(ack) if ack.pkid == 81));
            completed.send(()).unwrap();
            closed_rx.recv_async().await.unwrap();
        },
        move |handle, driver| async move {
            let stop = handle.clone();
            let (result, ((), ())) = future::zip(
                driver.run(move |event| collected.borrow_mut().push(event)),
                future::zip(
                    async {
                        handle.subscribe_raw_properties("target").await.unwrap();
                        captured_rx.recv_async().await.unwrap();
                        handle.unsubscribe_raw_properties("target").await.unwrap();
                    },
                    close_after(stop, completion),
                ),
            )
            .await;
            result.unwrap();
            closed.send(()).unwrap();
        },
    ));
    assert!(events.borrow().is_empty());
}

#[test]
fn raw_member_topic_must_match_payload_components() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let collected = events.clone();
    let (completed, completion) = flume::bounded(1);
    let (closed, closed_rx) = flume::bounded(1);
    future::block_on(exercise(
        timing(),
        move |mut broker| async move {
            broker.setup().await;
            let pkid = broker
                .subscribe("device/target/up/properties_changed/#")
                .await;
            broker.accept(pkid).await;
            for (index, topic) in [
                "device/target/up/properties_changed/99/88",
                "device/target/up/properties_changed/2",
                "device/target/up/properties_changed/2/1/extra",
                "device/target/up/properties_changed/abc/1",
                "device/target/up/properties_changed/2/0",
            ]
            .into_iter()
            .enumerate()
            {
                broker
                    .send(Packet::Publish(Publish::new(
                        topic,
                        QoS::AtMostOnce,
                        json!({"params":{"siid":2,"piid":1,"value":true}}).to_string(),
                    )))
                    .await;
                let _ = index;
            }
            broker.send(member_publish(91, 2, 1, json!(true))).await;
            assert!(matches!(broker.next().await, Packet::PubRec(ack) if ack.pkid == 91));
            broker.send(Packet::PubRel(PubRel::new(91))).await;
            assert!(matches!(broker.next().await, Packet::PubComp(ack) if ack.pkid == 91));
            completed.send(()).unwrap();
            closed_rx.recv_async().await.unwrap();
        },
        move |handle, driver| async move {
            let stop = handle.clone();
            let (result, (subscription, ())) = future::zip(
                driver.run(move |event| collected.borrow_mut().push(event)),
                future::zip(
                    handle.subscribe_raw_properties("target"),
                    close_after(stop, completion),
                ),
            )
            .await;
            result.unwrap();
            subscription.unwrap();
            closed.send(()).unwrap();
        },
    ));
    assert_eq!(
        *events.borrow(),
        [NotificationEvent::MemberProperty {
            did: "target".into(),
            siid: 2,
            piid: 1,
            value: json!(true),
            retained: false,
        }]
    );
}

#[test]
fn retained_raw_member_events_keep_their_origin() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let collected = events.clone();
    let (completed, completion) = flume::bounded(1);
    let (closed, closed_rx) = flume::bounded(1);
    future::block_on(exercise(
        timing(),
        move |mut broker| async move {
            broker.setup().await;
            let pkid = broker
                .subscribe("device/target/up/properties_changed/#")
                .await;
            broker.accept(pkid).await;
            let mut publish = member_publish(101, 2, 1, json!(true));
            let Packet::Publish(ref mut packet) = publish else {
                unreachable!()
            };
            packet.retain = true;
            broker.send(publish).await;
            assert!(matches!(broker.next().await, Packet::PubRec(ack) if ack.pkid == 101));
            broker.send(Packet::PubRel(PubRel::new(101))).await;
            assert!(matches!(broker.next().await, Packet::PubComp(ack) if ack.pkid == 101));
            completed.send(()).unwrap();
            closed_rx.recv_async().await.unwrap();
        },
        move |handle, driver| async move {
            let stop = handle.clone();
            let (result, (subscription, ())) = future::zip(
                driver.run(move |event| collected.borrow_mut().push(event)),
                future::zip(
                    handle.subscribe_raw_properties("target"),
                    close_after(stop, completion),
                ),
            )
            .await;
            result.unwrap();
            subscription.unwrap();
            closed.send(()).unwrap();
        },
    ));
    assert_eq!(
        *events.borrow(),
        [NotificationEvent::MemberProperty {
            did: "target".into(),
            siid: 2,
            piid: 1,
            value: json!(true),
            retained: true,
        }]
    );
}

#[test]
fn exact_source_handoff_does_not_invalidate_raw_member_delivery() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let collected = events.clone();
    let (completed, completion) = flume::bounded(1);
    let (closed, closed_rx) = flume::bounded(1);
    let (published, published_rx) = flume::bounded(1);
    let (invalidated, invalidated_rx) = flume::bounded(1);
    future::block_on(exercise(
        timing(),
        move |mut broker| async move {
            broker.setup().await;
            let pkid = broker
                .subscribe("device/target/up/properties_changed/#")
                .await;
            broker.accept(pkid).await;
            broker.send(member_publish(111, 2, 1, json!(true))).await;
            assert!(matches!(broker.next().await, Packet::PubRec(ack) if ack.pkid == 111));
            published.send(()).unwrap();
            invalidated_rx.recv_async().await.unwrap();
            broker.send(Packet::PubRel(PubRel::new(111))).await;
            assert!(matches!(broker.next().await, Packet::PubComp(ack) if ack.pkid == 111));
            broker.send(member_publish(112, 2, 1, json!(false))).await;
            assert!(matches!(broker.next().await, Packet::PubRec(ack) if ack.pkid == 112));
            broker.send(Packet::PubRel(PubRel::new(112))).await;
            assert!(matches!(broker.next().await, Packet::PubComp(ack) if ack.pkid == 112));
            completed.send(()).unwrap();
            closed_rx.recv_async().await.unwrap();
        },
        move |handle, driver| async move {
            let stop = handle.clone();
            let (result, (_, ())) = future::zip(
                driver.run(move |event| collected.borrow_mut().push(event)),
                future::zip(
                    async {
                        handle.subscribe_raw_properties("target").await.unwrap();
                        published_rx.recv_async().await.unwrap();
                        // Invalidate the received delivery before its PUBREL arrives.
                        handle.invalidate_received_properties("target");
                        invalidated.send(()).unwrap();
                    },
                    close_after(stop, completion),
                ),
            )
            .await;
            result.unwrap();
            closed.send(()).unwrap();
        },
    ));
    assert_eq!(
        *events.borrow(),
        [
            NotificationEvent::MemberProperty {
                did: "target".into(),
                siid: 2,
                piid: 1,
                value: json!(true),
                retained: false,
            },
            NotificationEvent::MemberProperty {
                did: "target".into(),
                siid: 2,
                piid: 1,
                value: json!(false),
                retained: false,
            },
        ]
    );
}

#[test]
fn raw_removal_keeps_exact_subscription_delivery() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let collected = events.clone();
    let (completed, completion) = flume::bounded(1);
    let (closed, closed_rx) = flume::bounded(1);
    future::block_on(exercise(
        timing(),
        move |mut broker| async move {
            broker.setup().await;
            let pkid = broker
                .subscribe("device/target/up/properties_changed/2/1")
                .await;
            broker.accept(pkid).await;
            let pkid = broker
                .subscribe("device/target/up/properties_changed/#")
                .await;
            broker.accept(pkid).await;
            let Packet::Unsubscribe(unsubscribe) = broker.next().await else {
                panic!("UNSUBSCRIBE expected");
            };
            assert_eq!(
                unsubscribe.filters,
                ["device/target/up/properties_changed/#"]
            );
            broker
                .send(Packet::UnsubAck(UnsubAck {
                    pkid: unsubscribe.pkid,
                    reasons: vec![UnsubAckReason::Success],
                    properties: None,
                }))
                .await;
            broker.send(received_property(91, true)).await;
            assert!(matches!(broker.next().await, Packet::PubRec(ack) if ack.pkid == 91));
            broker.send(Packet::PubRel(PubRel::new(91))).await;
            assert!(matches!(broker.next().await, Packet::PubComp(ack) if ack.pkid == 91));
            completed.send(()).unwrap();
            closed_rx.recv_async().await.unwrap();
        },
        move |handle, driver| async move {
            let stop = handle.clone();
            let (result, ((), ())) = future::zip(
                driver.run(move |event| collected.borrow_mut().push(event)),
                future::zip(
                    async {
                        handle.subscribe_property("target", POWER).await.unwrap();
                        handle.subscribe_raw_properties("target").await.unwrap();
                        // Client-owned synchronization: the driver has
                        // processed the UnsubAck before this returns.
                        handle.unsubscribe_raw_properties("target").await.unwrap();
                    },
                    close_after(stop, completion),
                ),
            )
            .await;
            result.unwrap();
            closed.send(()).unwrap();
        },
    ));
    assert_eq!(
        *events.borrow(),
        [NotificationEvent::Property {
            did: "target".into(),
            binding: POWER,
            value: true,
        }]
    );
}

#[test]
fn exact_removal_keeps_raw_watcher_delivery() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let collected = events.clone();
    let (completed, completion) = flume::bounded(1);
    let (closed, closed_rx) = flume::bounded(1);
    future::block_on(exercise(
        timing(),
        move |mut broker| async move {
            broker.setup().await;
            let pkid = broker
                .subscribe("device/target/up/properties_changed/2/1")
                .await;
            broker.accept(pkid).await;
            let pkid = broker
                .subscribe("device/target/up/properties_changed/#")
                .await;
            broker.accept(pkid).await;
            let Packet::Unsubscribe(unsubscribe) = broker.next().await else {
                panic!("UNSUBSCRIBE expected");
            };
            assert_eq!(
                unsubscribe.filters,
                ["device/target/up/properties_changed/2/1"]
            );
            broker
                .send(Packet::UnsubAck(UnsubAck {
                    pkid: unsubscribe.pkid,
                    reasons: vec![UnsubAckReason::Success],
                    properties: None,
                }))
                .await;
            // No exact registration for 2/2: only the raw watcher can deliver it.
            broker.send(member_publish(92, 2, 2, json!(false))).await;
            assert!(matches!(broker.next().await, Packet::PubRec(ack) if ack.pkid == 92));
            broker.send(Packet::PubRel(PubRel::new(92))).await;
            assert!(matches!(broker.next().await, Packet::PubComp(ack) if ack.pkid == 92));
            completed.send(()).unwrap();
            closed_rx.recv_async().await.unwrap();
        },
        move |handle, driver| async move {
            let stop = handle.clone();
            let (result, ((), ())) = future::zip(
                driver.run(move |event| collected.borrow_mut().push(event)),
                future::zip(
                    async {
                        handle.subscribe_property("target", POWER).await.unwrap();
                        handle.subscribe_raw_properties("target").await.unwrap();
                        handle.unsubscribe_properties("target").await.unwrap();
                    },
                    close_after(stop, completion),
                ),
            )
            .await;
            result.unwrap();
            closed.send(()).unwrap();
        },
    ));
    assert_eq!(
        *events.borrow(),
        [NotificationEvent::MemberProperty {
            did: "target".into(),
            siid: 2,
            piid: 2,
            value: json!(false),
            retained: false,
        }]
    );
}

/// A pending exact registration must not shadow a ready raw wildcard.
#[test]
fn pending_exact_does_not_hide_ready_raw_delivery() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let collected = events.clone();
    let (completed, completion) = flume::bounded(1);
    let (closed, closed_rx) = flume::bounded(1);
    future::block_on(exercise(
        timing(),
        move |mut broker| async move {
            broker.setup().await;
            let pkid = broker
                .subscribe("device/target/up/properties_changed/#")
                .await;
            broker.accept(pkid).await;
            // The exact subscription is enqueued but left unacknowledged.
            let exact = broker
                .subscribe("device/target/up/properties_changed/2/1")
                .await;
            broker
                .send(Packet::Publish(Publish::new(
                    "device/target/up/properties_changed/2/1",
                    QoS::AtMostOnce,
                    json!({"params":{"siid":2,"piid":1,"value":true}}).to_string(),
                )))
                .await;
            broker.accept(exact).await;
            broker.send(member_publish(71, 2, 2, json!(false))).await;
            assert!(matches!(broker.next().await, Packet::PubRec(ack) if ack.pkid == 71));
            broker.send(Packet::PubRel(PubRel::new(71))).await;
            assert!(matches!(broker.next().await, Packet::PubComp(ack) if ack.pkid == 71));
            completed.send(()).unwrap();
            closed_rx.recv_async().await.unwrap();
        },
        move |handle, driver| async move {
            let stop = handle.clone();
            let (result, ((), ())) = future::zip(
                driver.run(move |event| collected.borrow_mut().push(event)),
                future::zip(
                    async {
                        handle.subscribe_raw_properties("target").await.unwrap();
                        handle.subscribe_property("target", POWER).await.unwrap();
                    },
                    close_after(stop, completion),
                ),
            )
            .await;
            result.unwrap();
            closed.send(()).unwrap();
        },
    ));
    assert_eq!(
        *events.borrow(),
        [
            NotificationEvent::MemberProperty {
                did: "target".into(),
                siid: 2,
                piid: 1,
                value: json!(true),
                retained: false,
            },
            NotificationEvent::MemberProperty {
                did: "target".into(),
                siid: 2,
                piid: 2,
                value: json!(false),
                retained: false,
            },
        ]
    );
}

/// A QoS2 receipt captured as raw stays raw when an exact subscription
/// becomes ready before PUBREL.
#[test]
fn qos2_receipt_captured_as_raw_stays_raw_when_exact_becomes_ready() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let collected = events.clone();
    let (completed, completion) = flume::bounded(1);
    let (closed, closed_rx) = flume::bounded(1);
    // The runner registers the exact subscription only after the broker has
    // seen the PUBREC, so the receipt is provably captured as raw.
    let (captured, captured_rx) = flume::bounded(1);
    future::block_on(exercise(
        timing(),
        move |mut broker| async move {
            broker.setup().await;
            let pkid = broker
                .subscribe("device/target/up/properties_changed/#")
                .await;
            broker.accept(pkid).await;
            broker.send(member_publish(81, 2, 1, json!(true))).await;
            assert!(matches!(broker.next().await, Packet::PubRec(ack) if ack.pkid == 81));
            captured.send(()).unwrap();
            // The exact subscription becomes ready while the receipt waits.
            let exact = broker
                .subscribe("device/target/up/properties_changed/2/1")
                .await;
            broker.accept(exact).await;
            broker.send(Packet::PubRel(PubRel::new(81))).await;
            assert!(matches!(broker.next().await, Packet::PubComp(ack) if ack.pkid == 81));
            completed.send(()).unwrap();
            closed_rx.recv_async().await.unwrap();
        },
        move |handle, driver| async move {
            let stop = handle.clone();
            let (result, ((), ())) = future::zip(
                driver.run(move |event| collected.borrow_mut().push(event)),
                future::zip(
                    async {
                        handle.subscribe_raw_properties("target").await.unwrap();
                        captured_rx.recv_async().await.unwrap();
                        handle.subscribe_property("target", POWER).await.unwrap();
                    },
                    close_after(stop, completion),
                ),
            )
            .await;
            result.unwrap();
            closed.send(()).unwrap();
        },
    ));
    assert_eq!(
        *events.borrow(),
        [NotificationEvent::MemberProperty {
            did: "target".into(),
            siid: 2,
            piid: 1,
            value: json!(true),
            retained: false,
        }]
    );
}
