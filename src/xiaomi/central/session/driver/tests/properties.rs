use super::*;

#[test]
fn received_property_invalidation_keeps_subscriptions_and_business_requests() {
    block_on(async {
        let (handle, mut driver, budget, _) = rig();
        ready_property(&handle, &mut driver, "target").await;
        let pending = handle.subscribe_property("target", PropertyBinding { siid: 3, piid: 1 });
        let read = handle.read_property_value("target", BINDING, |_| {});
        let write = handle.set_property("target", BINDING, true, |_| {});
        driver.drain_requests().unwrap();
        let subscription_id = *driver.subscriptions.keys().next().unwrap();
        let read_id = *driver
            .pending
            .iter()
            .find(|(_, request)| matches!(request.kind, RequestKind::Read { .. }))
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
                .push(&encode_packet(&packet).unwrap())
                .unwrap();
        }
        assert!(poll_once(driver.drive(&mut on_event)).await.is_none());
        assert_eq!(
            events.borrow().as_slice(),
            &[CentralEvent::PropertyValue {
                did: "target".into(),
                binding: BINDING,
                value: serde_json::Value::Bool(false)
            }],
            "a received property from the retired source was dispatched"
        );
        assert!(driver.ready);
        assert_eq!(driver.property_topics.len(), 1);
        assert_eq!(driver.subscriptions.len(), 1);
        assert!(driver.subscriptions[&subscription_id].request.is_some());
        assert_eq!(driver.pending.len(), 2);
        assert_eq!(driver.output.len(), outputs_before + 3);
        let mut outputs = Frames::default();
        for output in driver.output.iter().skip(outputs_before) {
            outputs.push(output.frame.bytes()).unwrap();
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
        assert_eq!(read.await.result.unwrap(), json!(false));
        write.await.result.unwrap();
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
                .push(&encode_packet(&packet).unwrap())
                .unwrap();
        }
        let events = Rc::new(RefCell::new(Vec::new()));
        let mut on_event = |event| {
            if matches!(&event, CentralEvent::PropertyValue { did, .. } if did == "other") {
                handle.invalidate_received_properties("target");
            }
            events.borrow_mut().push(event);
        };
        assert!(poll_once(driver.drive(&mut on_event)).await.is_none());
        assert_eq!(
            events.borrow().as_slice(),
            &[CentralEvent::PropertyValue {
                did: "other".into(),
                binding: BINDING,
                value: serde_json::Value::Bool(true)
            }]
        );
        assert!(driver.ready);
        assert_eq!(driver.property_topics.len(), 2);
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
        let write = handle.set_property("target", BINDING, true, |_| {});
        driver.drain_requests().unwrap();
        let subscription_id = *driver.subscriptions.keys().next().unwrap();
        let mid = *driver.pending.keys().next().unwrap();
        write_ready(&mut driver, &budget, usize::MAX).await;
        write_ready(&mut driver, &budget, usize::MAX).await;
        let read = handle.read_property_value("target", BINDING, |_| {});
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
                result: Ok(json!(false)),
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
            &[CentralEvent::PropertyValue {
                did: "target".into(),
                binding: BINDING,
                value: serde_json::Value::Bool(false)
            }]
        );
    });
}
