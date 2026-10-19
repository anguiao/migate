use super::*;

#[test]
fn online_topics_match_the_device_and_state_topic_levels() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let collected = events.clone();
    let (finished, done) = flume::bounded(1);
    future::block_on(exercise(
        timing(),
        move |mut broker| async move {
            broker.setup().await;
            let pkid = broker.subscribe("device/target/state/#").await;
            broker.accept(pkid).await;
            for topic in [
                "device/other/state/online",
                "device/target/stateful/online",
                "device/target/up/properties_changed/2/1",
                "device/target/state",
                "device/target/state/online/detail",
            ] {
                broker
                    .send(Packet::Publish(Publish::new(
                        topic,
                        QoS::AtMostOnce,
                        json!({"device_id":"target","event":"online"}).to_string(),
                    )))
                    .await;
            }
            done.recv_async().await.unwrap();
        },
        move |handle, driver| async move {
            let stop = handle.clone();
            let (result, subscription) = future::zip(
                driver.run(move |event| {
                    collected.borrow_mut().push(event);
                    if collected.borrow().len() == 2 {
                        finished.send(()).unwrap();
                        stop.close();
                    }
                }),
                handle.subscribe_online("target"),
            )
            .await;
            result.unwrap();
            subscription.unwrap();
        },
    ));
    assert_eq!(
        *events.borrow(),
        vec![
            NotificationEvent::Online {
                did: "target".into(),
                online: true
            };
            2
        ]
    );
}

#[test]
fn received_property_invalidation_keeps_ready_subscriptions_and_online_delivery() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let collected = events.clone();
    let observed = events.clone();
    let (invalidate, invalidating) = flume::bounded(1);
    let (resume, resumed) = flume::bounded(1);
    let (completed, completion) = flume::bounded(1);
    let (finished, done) = flume::bounded(1);
    future::block_on(exercise(
        timing(),
        move |mut broker| async move {
            broker.setup().await;
            let pkid = broker
                .subscribe("device/target/up/properties_changed/2/1")
                .await;
            broker.accept(pkid).await;
            let pkid = broker.subscribe("device/target/state/#").await;
            broker.accept(pkid).await;
            broker.send(received_property(51, false)).await;
            assert!(matches!(broker.next().await, Packet::PubRec(ack) if ack.pkid == 51));
            let mut online = Publish::new(
                "device/target/state/offline",
                QoS::ExactlyOnce,
                json!({"device_id":"target","event":"offline"}).to_string(),
            );
            online.pkid = 52;
            broker.send(Packet::Publish(online)).await;
            assert!(matches!(broker.next().await, Packet::PubRec(ack) if ack.pkid == 52));
            invalidate.send(()).unwrap();
            resumed.recv_async().await.unwrap();
            for pkid in [51, 52] {
                broker.send(Packet::PubRel(PubRel::new(pkid))).await;
                assert!(matches!(broker.next().await, Packet::PubComp(ack) if ack.pkid == pkid));
            }
            broker.send(received_property(53, true)).await;
            assert!(matches!(broker.next().await, Packet::PubRec(ack) if ack.pkid == 53));
            broker.send(Packet::PubRel(PubRel::new(53))).await;
            assert!(matches!(broker.next().await, Packet::PubComp(ack) if ack.pkid == 53));
            completed.send(()).unwrap();
            done.recv_async().await.unwrap();
        },
        move |handle, driver| async move {
            let (result, ()) = future::zip(
                driver.run(move |event| collected.borrow_mut().push(event)),
                async {
                    handle.subscribe_property("target", POWER).await.unwrap();
                    handle.subscribe_online("target").await.unwrap();
                    invalidating.recv_async().await.unwrap();
                    let generations = handle
                        .shared
                        .registrations
                        .borrow()
                        .iter()
                        .map(|(topic, registration)| (topic.clone(), registration.generation))
                        .collect::<Vec<_>>();
                    handle.invalidate_received_properties("target");
                    for (topic, generation) in generations {
                        let registrations = handle.shared.registrations.borrow();
                        assert!(registrations[&topic].ready);
                        assert_eq!(registrations[&topic].generation, generation);
                    }
                    handle.subscribe_property("target", POWER).await.unwrap();
                    assert!(handle.shared.queue.borrow().is_empty());
                    assert_eq!(handle.shared.outstanding.get(), 0);
                    resume.send(()).unwrap();
                    completion.recv_async().await.unwrap();
                    assert_eq!(
                        observed.borrow().as_slice(),
                        &[
                            NotificationEvent::Online {
                                did: "target".into(),
                                online: false
                            },
                            NotificationEvent::PropertyValue {
                                did: "target".into(),
                                binding: POWER,
                                value: serde_json::Value::Bool(true)
                            },
                        ],
                        "a received property from the retired source was dispatched"
                    );
                    handle.close();
                },
            )
            .await;
            result.unwrap();
            finished.send(()).unwrap();
        },
    ));
}

#[test]
fn received_property_invalidation_keeps_a_pending_suback_current() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let collected = events.clone();
    let observed = events.clone();
    let (invalidate, invalidating) = flume::bounded(1);
    let (resume, resumed) = flume::bounded(1);
    let (completed, completion) = flume::bounded(1);
    let (finished, done) = flume::bounded(1);
    future::block_on(exercise(
        timing(),
        move |mut broker| async move {
            broker.setup().await;
            let pkid = broker
                .subscribe("device/target/up/properties_changed/2/1")
                .await;
            invalidate.send(()).unwrap();
            resumed.recv_async().await.unwrap();
            broker.accept(pkid).await;
            broker.property(json!(true)).await;
            let pkid = broker.subscribe("device/target/state/#").await;
            broker.accept(pkid).await;
            broker
                .send(Packet::Publish(Publish::new(
                    "device/target/state/offline",
                    QoS::AtMostOnce,
                    json!({"device_id":"target","event":"offline"}).to_string(),
                )))
                .await;
            completed.send(()).unwrap();
            done.recv_async().await.unwrap();
        },
        move |handle, driver| async move {
            let (result, ()) = future::zip(
                driver.run(move |event| collected.borrow_mut().push(event)),
                async {
                    let pending = handle.subscribe_property("target", POWER);
                    invalidating.recv_async().await.unwrap();
                    let topic = "device/target/up/properties_changed/2/1";
                    let generation = handle.shared.registrations.borrow()[topic].generation;
                    assert!(!handle.shared.registrations.borrow()[topic].ready);
                    handle.invalidate_received_properties("target");
                    assert_eq!(
                        handle.shared.registrations.borrow()[topic].generation,
                        generation
                    );
                    assert_eq!(handle.shared.outstanding.get(), 1);
                    resume.send(()).unwrap();
                    pending.await.unwrap();
                    assert!(handle.shared.registrations.borrow()[topic].ready);
                    handle.subscribe_online("target").await.unwrap();
                    completion.recv_async().await.unwrap();
                    while observed.borrow().len() < 2 {
                        future::yield_now().await;
                    }
                    assert_eq!(
                        observed.borrow().as_slice(),
                        &[
                            NotificationEvent::PropertyValue {
                                did: "target".into(),
                                binding: POWER,
                                value: serde_json::Value::Bool(true)
                            },
                            NotificationEvent::Online {
                                did: "target".into(),
                                online: false
                            },
                        ]
                    );
                    handle.close();
                },
            )
            .await;
            result.unwrap();
            finished.send(()).unwrap();
        },
    ));
}

#[test]
fn malformed_notification_payloads_are_isolated_from_valid_notifications() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let collected = events.clone();
    let (finished, done) = flume::bounded(1);
    future::block_on(exercise(
        timing(),
        move |mut broker| async move {
            broker.setup().await;
            let pkid = broker
                .subscribe("device/target/up/properties_changed/2/1")
                .await;
            broker.accept(pkid).await;
            let pkid = broker.subscribe("device/target/state/#").await;
            broker.accept(pkid).await;
            for payload in [
                "not JSON".to_owned(),
                json!({"params":{"siid":2,"piid":1,"value":"1"}}).to_string(),
                json!({"params":{"siid":3,"piid":1,"value":true}}).to_string(),
                json!({"params":{"did":"foreign","siid":2,"piid":1,"value":true}}).to_string(),
                json!({"params":{"siid":"2","piid":1,"value":true}}).to_string(),
                json!({"params":{"siid":2,"piid":1}}).to_string(),
                json!({"did":"foreign","params":{"siid":2,"piid":1,"value":true}}).to_string(),
            ] {
                broker
                    .send(Packet::Publish(Publish::new(
                        "device/target/up/properties_changed/2/1",
                        QoS::AtMostOnce,
                        payload,
                    )))
                    .await;
            }
            for payload in [
                json!({"device_id":"foreign","event":"online"}),
                json!({"device_id":"target","event":"unknown"}),
                json!({"device_id":"target","event":"online","uid":"foreign"}),
                json!({"device_id":"target","event":"online","uid":true}),
            ] {
                broker
                    .send(Packet::Publish(Publish::new(
                        "device/target/state/online",
                        QoS::AtMostOnce,
                        payload.to_string(),
                    )))
                    .await;
            }
            broker.property(json!(false)).await;
            broker
                .send(Packet::Publish(Publish::new(
                    "device/target/state/offline",
                    QoS::AtMostOnce,
                    json!({"device_id":"target","event":"offline"}).to_string(),
                )))
                .await;
            done.recv_async().await.unwrap();
        },
        move |handle, driver| async move {
            let stop = handle.clone();
            let (result, ()) = future::zip(
                driver.run(move |event| {
                    collected.borrow_mut().push(event);
                    if collected.borrow().len() == 2 {
                        finished.send(()).unwrap();
                        stop.close();
                    }
                }),
                async {
                    handle.subscribe_property("target", POWER).await.unwrap();
                    handle.subscribe_online("target").await.unwrap();
                },
            )
            .await;
            result.unwrap();
        },
    ));
    assert_eq!(
        *events.borrow(),
        [
            NotificationEvent::PropertyValue {
                did: "target".into(),
                binding: POWER,
                value: serde_json::Value::Bool(false)
            },
            NotificationEvent::Online {
                did: "target".into(),
                online: false
            }
        ]
    );
}

#[test]
fn qos2_duplicate_and_retired_subscription_messages_do_not_reapply() {
    for numeric in [false, true] {
        let old_value = if numeric { json!(25) } else { json!(false) };
        let new_value = if numeric { json!(50) } else { json!(true) };

        let events = Rc::new(RefCell::new(Vec::new()));
        let collected = events.clone();
        let (retire, retired) = flume::bounded(1);
        let (finished, done) = flume::bounded(1);
        future::block_on(exercise(
            timing(),
            move |mut broker| async move {
                broker.setup().await;
                let pkid = broker
                    .subscribe("device/target/up/properties_changed/2/1")
                    .await;
                broker.accept(pkid).await;
                let mut old = Publish::new(
                    "device/target/up/properties_changed/2/1",
                    QoS::ExactlyOnce,
                    json!({"params":{"siid":2,"piid":1,"value":old_value}}).to_string(),
                );
                old.pkid = 7;
                broker.send(Packet::Publish(old.clone())).await;
                assert!(matches!(broker.next().await, Packet::PubRec(rec) if rec.pkid == 7));
                old.dup = true;
                broker.send(Packet::Publish(old)).await;
                assert!(matches!(broker.next().await, Packet::PubRec(rec) if rec.pkid == 7));
                retire.send(()).unwrap();
                let Packet::Unsubscribe(unsub) = broker.next().await else {
                    panic!();
                };
                broker
                    .send(Packet::UnsubAck(UnsubAck {
                        pkid: unsub.pkid,
                        reasons: vec![UnsubAckReason::Success],
                        properties: None,
                    }))
                    .await;
                let pkid = broker
                    .subscribe("device/target/up/properties_changed/2/1")
                    .await;
                broker.accept(pkid).await;
                broker.send(Packet::PubRel(PubRel::new(7))).await;
                assert!(matches!(broker.next().await, Packet::PubComp(comp) if comp.pkid == 7));
                let mut new = Publish::new(
                    "device/target/up/properties_changed/2/1",
                    QoS::ExactlyOnce,
                    json!({"params":{"siid":2,"piid":1,"value":new_value}}).to_string(),
                );
                new.pkid = 8;
                broker.send(Packet::Publish(new)).await;
                assert!(matches!(broker.next().await, Packet::PubRec(rec) if rec.pkid == 8));
                broker.send(Packet::PubRel(PubRel::new(8))).await;
                assert!(matches!(broker.next().await, Packet::PubComp(comp) if comp.pkid == 8));
                broker.send(Packet::PubRel(PubRel::new(8))).await;
                assert!(matches!(broker.next().await, Packet::PubComp(comp) if comp.pkid == 8));
                broker.property(old_value).await;
                done.recv_async().await.unwrap();
            },
            move |handle, driver| async move {
                let stop = handle.clone();
                let (result, ()) = future::zip(
                    driver.run(move |event| {
                        collected.borrow_mut().push(event);
                        if collected.borrow().len() == 2 {
                            finished.send(()).unwrap();
                            stop.close();
                        }
                    }),
                    async {
                        handle.subscribe_property("target", POWER).await.unwrap();
                        retired.recv_async().await.unwrap();
                        handle.unsubscribe_properties("target").await.unwrap();
                        handle.subscribe_property("target", POWER).await.unwrap();
                    },
                )
                .await;
                result.unwrap();
            },
        ));
        let event = |value| NotificationEvent::PropertyValue {
            did: "target".into(),
            binding: POWER,
            value,
        };
        assert_eq!(
            *events.borrow(),
            [
                event(if numeric { json!(50) } else { json!(true) }),
                event(if numeric { json!(25) } else { json!(false) })
            ]
        );
    }
}

#[test]
fn numeric_notifications_preserve_payload_identity_and_values() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let collected = events.clone();
    let (finished, done) = flume::bounded(1);
    future::block_on(exercise(
        timing(),
        move |mut broker| async move {
            broker.setup().await;
            let pkid = broker
                .subscribe("device/target/up/properties_changed/2/1")
                .await;
            broker.accept(pkid).await;
            for payload in [
                json!({"params":{"did":"other","siid":2,"piid":1,"value":50}}),
                json!({"params":{"siid":2,"piid":2,"value":50}}),
                json!({"params":{"siid":2,"piid":1,"value":null}}),
                json!({"uid":"foreign","params":{"siid":2,"piid":1,"value":50}}),
            ] {
                broker
                    .send(Packet::Publish(Publish::new(
                        "device/target/up/properties_changed/2/1",
                        QoS::AtMostOnce,
                        payload.to_string(),
                    )))
                    .await;
            }
            broker.property(json!(65535)).await;
            broker.property(json!(35.5)).await;
            done.recv_async().await.unwrap();
        },
        move |handle, driver| async move {
            let stop = handle.clone();
            let (result, subscription) = future::zip(
                driver.run(move |event| {
                    collected.borrow_mut().push(event);
                    if collected.borrow().len() == 2 {
                        finished.send(()).unwrap();
                        stop.close();
                    }
                }),
                handle.subscribe_property("target", POWER),
            )
            .await;
            result.unwrap();
            subscription.unwrap();
        },
    ));
    assert_eq!(
        *events.borrow(),
        vec![
            NotificationEvent::PropertyValue {
                did: "target".into(),
                binding: POWER,
                value: json!(65535)
            },
            NotificationEvent::PropertyValue {
                did: "target".into(),
                binding: POWER,
                value: json!(35.5)
            },
        ]
    );
}
