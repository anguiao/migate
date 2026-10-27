//! Local events use independent event identifiers and MQTT generations.
use super::*;
use crate::xiaomi::capability::EventBinding;

const EVENT: EventBinding = EventBinding { siid: 2, eiid: 1 };

fn event(retain: bool, payload: serde_json::Value) -> Packet {
    let mut publish = Publish::new(
        "client/appMsg/notify/iot/target/event/2.1",
        QoS::AtMostOnce,
        wire::encode_mips(1, None, &payload),
    );
    publish.retain = retain;
    Packet::Publish(publish)
}

#[test]
fn local_event_notifications_match_eiid_and_ignore_retained_events() {
    block_on(async {
        let (handle, mut driver, _, _) = rig();
        let subscribed = handle.subscribe_event("target", EVENT, |_| {});
        driver.drain_requests().unwrap();
        let mut frames = Frames::default();
        frames
            .push(driver.output.back().unwrap().frame.bytes())
            .unwrap();
        let Some(Packet::Subscribe(subscription)) = frames.next_packet().unwrap() else {
            panic!("subscription expected")
        };
        assert_eq!(
            subscription.filters[0].path,
            "master/appMsg/notify/iot/target/event/2.1"
        );
        let mut events = Vec::new();
        driver
            .packet(
                event(false, json!({"did":"target","siid":2,"eiid":1})),
                &mut |event| events.push(event),
            )
            .unwrap();
        assert!(events.is_empty());
        driver
            .packet(
                Packet::SubAck(SubAck::new(
                    subscription.pkid,
                    vec![SubscribeReasonCode::QoS2],
                )),
                &mut |_| {},
            )
            .unwrap();
        subscribed.await.unwrap();
        for payload in [
            json!({"did":"other","siid":2,"eiid":1,"arguments":[]}),
            json!({"did":"target","siid":2,"piid":1,"arguments":[]}),
            json!({"did":"target","siid":2,"eiid":9,"arguments":[]}),
        ] {
            driver
                .packet(event(false, payload), &mut |event| events.push(event))
                .unwrap();
        }
        driver
            .packet(
                event(
                    true,
                    json!({"did":"target","siid":2,"eiid":1,"arguments":[]}),
                ),
                &mut |event| events.push(event),
            )
            .unwrap();
        driver.packet(event(false, json!({"did":"target","siid":2,"eiid":1,"timestamp":41,"arguments":[{"piid":1005,"value":0.5}]})), &mut |event| events.push(event)).unwrap();
        driver
            .packet(
                event(false, json!({"did":"target","siid":2,"eiid":1})),
                &mut |event| events.push(event),
            )
            .unwrap();
        assert_eq!(
            events,
            [
                CentralEvent::EventOccurred {
                    did: "target".into(),
                    binding: EVENT,
                    arguments: vec![json!({"piid":1005,"value":0.5})],
                    source_time: Some(41)
                },
                CentralEvent::EventOccurred {
                    did: "target".into(),
                    binding: EVENT,
                    arguments: vec![],
                    source_time: None
                },
            ]
        );
    });
}

async fn ready_event(handle: &SessionHandle, driver: &mut SessionDriver) {
    let subscribed = handle.subscribe_event("target", EVENT, |_| {});
    driver.drain_requests().unwrap();
    let pkid = *driver
        .subscriptions
        .iter()
        .find(|(_, subscription)| {
            subscription.request.is_some()
                && matches!(
                    subscription.kind,
                    SubscriptionKind::Notification {
                        binding: NotificationBinding::Event(_),
                        ..
                    }
                )
        })
        .unwrap()
        .0;
    driver
        .packet(
            Packet::SubAck(SubAck::new(pkid, vec![SubscribeReasonCode::QoS2])),
            &mut |_| {},
        )
        .unwrap();
    subscribed.await.unwrap();
    driver.output.clear();
}

fn qos2_event(pkid: u16) -> Packet {
    let Packet::Publish(mut publish) = event(
        false,
        json!({"did":"target","siid":2,"eiid":1,"arguments":[]}),
    ) else {
        unreachable!()
    };
    publish.qos = QoS::ExactlyOnce;
    publish.pkid = pkid;
    Packet::Publish(publish)
}

fn acknowledge_unsubscribe(driver: &mut SessionDriver) {
    let (&pkid, subscription) = driver
        .subscriptions
        .iter()
        .find(|(_, subscription)| subscription.kind == SubscriptionKind::Unsubscribe)
        .unwrap();
    let mut ack = UnsubAck::new(pkid);
    ack.reasons = vec![UnsubAckReason::Success; subscription.count];
    driver.packet(Packet::UnsubAck(ack), &mut |_| {}).unwrap();
    driver.output.clear();
}

#[test]
fn local_event_generations_and_scoped_unsubscription_preserve_other_work() {
    block_on(async {
        let (handle, mut driver, _, _) = rig();
        ready_property(&handle, &mut driver, "target").await;
        ready_event(&handle, &mut driver).await;
        let read = handle.read_property_value("target", BINDING, |_| {});
        driver.drain_requests().unwrap();
        let mut events = Vec::new();
        driver
            .packet(qos2_event(81), &mut |event| events.push(event))
            .unwrap();
        handle.invalidate_received_event("target", EVENT);
        driver.drain_notification_invalidations();
        driver
            .packet(
                Packet::PubRel(mqttbytes::v5::PubRel::new(81)),
                &mut |event| events.push(event),
            )
            .unwrap();
        assert!(events.is_empty());
        let fresh = qos2_event(82);
        driver
            .packet(fresh.clone(), &mut |event| events.push(event))
            .unwrap();
        let Packet::Publish(mut duplicate) = fresh else {
            unreachable!()
        };
        duplicate.dup = true;
        driver
            .packet(Packet::Publish(duplicate), &mut |event| events.push(event))
            .unwrap();
        for _ in 0..2 {
            driver
                .packet(
                    Packet::PubRel(mqttbytes::v5::PubRel::new(82)),
                    &mut |event| events.push(event),
                )
                .unwrap();
        }
        assert_eq!(events.len(), 1);
        driver
            .packet(qos2_event(83), &mut |event| events.push(event))
            .unwrap();
        let unsubscribe = handle.unsubscribe_events("target");
        driver.drain_cancellations();
        assert_eq!(driver.notification_topics.len(), 1);
        assert!(
            driver
                .notification_topics
                .contains_key("client/appMsg/notify/iot/target/property/2.1")
        );
        assert_eq!(driver.pending.len(), 1);
        driver.drain_requests().unwrap();
        acknowledge_unsubscribe(&mut driver);
        unsubscribe.await.unwrap();
        ready_event(&handle, &mut driver).await;
        driver
            .packet(
                Packet::PubRel(mqttbytes::v5::PubRel::new(83)),
                &mut |event| events.push(event),
            )
            .unwrap();
        assert_eq!(
            events.len(),
            1,
            "retired QoS2 event reappeared after resubscription"
        );
        let unsubscribe = handle.unsubscribe_properties("target");
        driver.drain_cancellations();
        assert_eq!(driver.notification_topics.len(), 1);
        assert!(
            driver
                .notification_topics
                .contains_key("client/appMsg/notify/iot/target/event/2.1")
        );
        assert_eq!(driver.pending.len(), 1);
        driver.drain_requests().unwrap();
        acknowledge_unsubscribe(&mut driver);
        unsubscribe.await.unwrap();
        driver
            .packet(
                event(false, json!({"did":"target","siid":2,"eiid":1})),
                &mut |event| events.push(event),
            )
            .unwrap();
        assert_eq!(events.len(), 2);
        let unsubscribe = handle.unsubscribe_device("target");
        driver.drain_cancellations();
        assert!(driver.notification_topics.is_empty());
        assert_eq!(read.await.result, Err(CentralError::Cancelled));
        driver.drain_requests().unwrap();
        acknowledge_unsubscribe(&mut driver);
        unsubscribe.await.unwrap();
        driver
            .packet(
                event(false, json!({"did":"target","siid":2,"eiid":1})),
                &mut |event| events.push(event),
            )
            .unwrap();
        assert_eq!(events.len(), 2);
    });
}

#[test]
fn local_late_event_subacks_and_failures_cannot_restore_retired_subscriptions() {
    block_on(async {
        let (handle, mut driver, budget, _) = rig();
        let pending = handle.subscribe_event("target", EVENT, |_| {});
        driver.drain_requests().unwrap();
        let pkid = *driver.subscriptions.keys().next().unwrap();
        write_ready(&mut driver, &budget, usize::MAX).await;
        let unsubscribe = handle.unsubscribe_events("target");
        driver.drain_cancellations();
        assert_eq!(pending.await, Err(CentralError::Cancelled));
        driver
            .packet(
                Packet::SubAck(SubAck::new(pkid, vec![SubscribeReasonCode::QoS2])),
                &mut |_| {},
            )
            .unwrap();
        assert!(driver.notification_topics.is_empty());
        driver.drain_requests().unwrap();
        acknowledge_unsubscribe(&mut driver);
        unsubscribe.await.unwrap();
        let rejected = handle.subscribe_event("target", EVENT, |_| {});
        driver.drain_requests().unwrap();
        let pkid = *driver.subscriptions.keys().next().unwrap();
        driver
            .packet(
                Packet::SubAck(SubAck::new(pkid, vec![SubscribeReasonCode::NotAuthorized])),
                &mut |_| {},
            )
            .unwrap();
        assert_eq!(rejected.await, Err(CentralError::Rejected(135)));
        assert!(driver.notification_topics.is_empty());
        ready_event(&handle, &mut driver).await;
        assert_eq!(driver.notification_topics.len(), 1);
        assert_eq!(driver.shared.outstanding.get(), 0);
    });
}

#[test]
fn local_event_timeouts_do_not_accept_late_subacks_and_requests_are_bounded() {
    block_on(async {
        let (handle, mut driver, _, _) = rig();
        assert_eq!(
            handle
                .subscribe_event("target", EventBinding { siid: 0, eiid: 1 }, |_| {})
                .await,
            Err(CentralError::InvalidInput)
        );
        let expired = handle.subscribe_event("target", EVENT, |_| {});
        driver.drain_requests().unwrap();
        let pkid = *driver.subscriptions.keys().next().unwrap();
        driver.subscriptions.get_mut(&pkid).unwrap().deadline =
            Instant::now() - Duration::from_millis(1);
        driver.expire().unwrap();
        assert_eq!(expired.await, Err(CentralError::Timeout));
        driver
            .packet(
                Packet::SubAck(SubAck::new(pkid, vec![SubscribeReasonCode::QoS2])),
                &mut |_| {},
            )
            .unwrap();
        assert!(driver.notification_topics.is_empty());
        let pending = (1..=LIMIT)
            .map(|eiid| {
                handle.subscribe_event(
                    "target",
                    EventBinding {
                        siid: 2,
                        eiid: eiid as u32,
                    },
                    |_| {},
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            handle.subscribe_event("other", EVENT, |_| {}).await,
            Err(CentralError::QueueFull)
        );
        driver.finish_all(CentralError::Cancelled);
        for request in pending {
            assert_eq!(request.await, Err(CentralError::Cancelled));
        }
        assert_eq!(handle.shared.outstanding.get(), 0);
        assert_eq!(
            handle.subscribe_event("target", EVENT, |_| {}).await,
            Err(CentralError::Cancelled)
        );
    });
}
