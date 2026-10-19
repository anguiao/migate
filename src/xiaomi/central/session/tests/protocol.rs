use super::*;

#[test]
fn positive_pubrec_and_reused_duplicate_packet_id_remain_valid() {
    let values = Rc::new(RefCell::new(Vec::new()));
    let events = values.clone();
    block_on(pair(
        |mut broker| async move {
            broker.setup().await;
            broker.subscription().await;
            let Packet::Publish(read) = broker.next().await else {
                panic!("read expected")
            };
            let mut receipt = PubRec::new(read.pkid);
            receipt.reason = PubRecReason::NoMatchingSubscribers;
            broker.send(Packet::PubRec(receipt)).await;
            broker
                .wait_for(|packet| matches!(packet,Packet::PubRel(ack) if ack.pkid==read.pkid))
                .await;
            broker.send(Packet::PubComp(PubComp::new(read.pkid))).await;
            broker.reply(&read, json!({"value":false}), 20).await;
            for (value, dup) in [(false, false), (true, true)] {
                let mut publish = Publish::new(
                    format!("{}/appMsg/notify/iot/target/property/2.1", broker.client_id),
                    QoS::ExactlyOnce,
                    wire::encode_mips(
                        50,
                        None,
                        &json!({"did":"target","siid":2,"piid":1,"value":value}),
                    ),
                );
                publish.pkid = 21;
                publish.dup = dup;
                publish.retain = true;
                broker.send(Packet::Publish(publish)).await;
                broker
                    .wait_for(|packet| matches!(packet,Packet::PubRec(ack) if ack.pkid==21))
                    .await;
                broker.send(Packet::PubRel(PubRel::new(21))).await;
                broker
                    .wait_for(|packet| matches!(packet,Packet::PubComp(ack) if ack.pkid==21))
                    .await;
            }
            broker.send(Packet::PubRel(PubRel::new(21))).await;
            broker
                .wait_for(|packet| matches!(packet,Packet::PubComp(ack) if ack.pkid==21))
                .await;
            async_io::Timer::after(Duration::from_millis(20)).await;
        },
        |handle, driver| async move {
            let work = async {
                handle
                    .subscribe_property("target", PropertyBinding { siid: 2, piid: 1 })
                    .await
                    .unwrap();
                assert!(
                    !handle
                        .read_property("target", PropertyBinding { siid: 2, piid: 1 }, |outcome| {
                            outcome.result.unwrap();
                        })
                        .await
                        .result
                        .unwrap()
                );
                async_io::Timer::after(Duration::from_millis(10)).await;
                handle.close();
            };
            let (result, ()) = future::zip(
                driver.run(move |event| {
                    if let CentralEvent::PropertySnapshot { value, .. } = event {
                        events.borrow_mut().push(value.as_bool().unwrap())
                    }
                }),
                work,
            )
            .await;
            result.unwrap();
        },
    ));
    assert_eq!(*values.borrow(), [false, true]);
}

#[test]
fn duplicate_inflight_qos2_publication_is_delivered_once() {
    let values = Rc::new(RefCell::new(Vec::new()));
    let observed = values.clone();
    block_on(pair(
        |mut broker| async move {
            broker.setup().await;
            broker.subscription().await;
            let mut publish = Publish::new(
                format!("{}/appMsg/notify/iot/target/property/2.1", broker.client_id),
                QoS::ExactlyOnce,
                wire::encode_mips(
                    42,
                    None,
                    &json!({"did":"target","siid":2,"piid":1,"value":true}),
                ),
            );
            publish.pkid = 30;
            broker.send(Packet::Publish(publish.clone())).await;
            broker
                .wait_for(|packet| matches!(packet,Packet::PubRec(ack) if ack.pkid==30))
                .await;
            publish.dup = true;
            broker.send(Packet::Publish(publish)).await;
            broker
                .wait_for(|packet| matches!(packet,Packet::PubRec(ack) if ack.pkid==30))
                .await;
            broker.send(Packet::PubRel(PubRel::new(30))).await;
            broker
                .wait_for(|packet| matches!(packet,Packet::PubComp(ack) if ack.pkid==30))
                .await;
            broker.send(Packet::PubRel(PubRel::new(30))).await;
            broker
                .wait_for(|packet| matches!(packet,Packet::PubComp(ack) if ack.pkid==30))
                .await;
            async_io::Timer::after(Duration::from_millis(25)).await;
        },
        |handle, driver| async move {
            let work = async {
                handle
                    .subscribe_property("target", PropertyBinding { siid: 2, piid: 1 })
                    .await
                    .unwrap();
                async_io::Timer::after(Duration::from_millis(15)).await;
                handle.close();
            };
            let (result, ()) = future::zip(
                driver.run(move |event| {
                    if let CentralEvent::PropertyValue { value, .. } = event {
                        observed.borrow_mut().push(value.as_bool().unwrap())
                    }
                }),
                work,
            )
            .await;
            result.unwrap();
        },
    ));
    assert_eq!(*values.borrow(), [true]);
}

#[test]
fn rejected_acknowledgements_and_wrong_packet_ids_never_accept_writes() {
    for mode in 0..4 {
        let expected = match mode {
            0 => CentralError::Rejected(135),
            1 | 2 => CentralError::Protocol,
            _ => CentralError::Rejected(146),
        };
        let callbacks = Rc::new(RefCell::new(Vec::new()));
        let results = callbacks.clone();
        let callback_error = expected.clone();
        block_on(pair(
            move |mut broker| async move {
                broker.setup().await;
                let Packet::Publish(write) = broker.next().await else {
                    panic!("write expected")
                };
                match mode {
                    0 => {
                        let mut ack = PubRec::new(write.pkid);
                        ack.reason = PubRecReason::NotAuthorized;
                        broker.send(Packet::PubRec(ack)).await;
                    }
                    1 => {
                        broker
                            .send(Packet::PubRec(PubRec::new(write.pkid + 1)))
                            .await
                    }
                    2 => broker.send(Packet::PubComp(PubComp::new(write.pkid))).await,
                    _ => {
                        broker.send(Packet::PubRec(PubRec::new(write.pkid))).await;
                        broker.wait_for(|packet|matches!(packet,Packet::PubRel(ack) if ack.pkid==write.pkid)).await;
                        let mut ack = PubComp::new(write.pkid);
                        ack.reason = PubCompReason::PacketIdentifierNotFound;
                        broker.send(Packet::PubComp(ack)).await;
                    }
                }
                async_io::Timer::after(Duration::from_millis(20)).await;
            },
            move |handle, driver| async move {
                let work = async {
                    assert_eq!(
                        handle
                            .set_property(
                                "target",
                                PropertyBinding { siid: 2, piid: 1 },
                                true,
                                move |outcome| results.borrow_mut().push(outcome.result)
                            )
                            .await
                            .result,
                        Err(callback_error)
                    );
                    handle.close();
                };
                let (result, ()) = future::zip(driver.run(|_| {}), work).await;
                if mode == 1 || mode == 2 {
                    assert_eq!(result, Err(CentralError::Protocol));
                } else {
                    result.unwrap();
                }
            },
        ));
        assert_eq!(*callbacks.borrow(), [Err(expected)]);
    }
}

#[test]
fn strict_business_values_and_returned_identity_are_required() {
    let binding = PropertyBinding { siid: 2, piid: 1 };
    let read = RequestKind::Read {
        format: super::super::request::ReadFormat::Boolean,
        did: "target".into(),
        binding,
    };
    for value in [
        json!({"value":1}),
        json!({"value":"false"}),
        json!({"did":"other","value":true}),
        json!({"siid":3,"piid":1,"value":true}),
    ] {
        assert!(matches!(
            parse_response(&read, &value),
            Err(CentralError::Protocol)
        ));
    }
    let write = RequestKind::Write {
        did: "target".into(),
        binding,
        value: true,
    };
    for value in [
        json!({"result":[{"did":"other","code":0}]}),
        json!({"result":[{"code":1}]}),
        json!({"result":[{"did":"target","piid":2,"code":0}]}),
        json!({"result":[{"did":"target","code":false}]}),
    ] {
        assert!(matches!(
            parse_response(&write, &value),
            Err(CentralError::Protocol)
        ));
    }
    assert!(matches!(
        parse_response(&write, &json!({"result":[{"did":"target","code":-704}]})),
        Err(CentralError::Rejected(-704))
    ));
}

#[test]
fn invalid_device_messages_do_not_disconnect_the_shared_session() {
    for qos in [QoS::AtMostOnce, QoS::AtLeastOnce, QoS::ExactlyOnce] {
        let events = Rc::new(RefCell::new(Vec::new()));
        let received = events.clone();
        block_on(pair(
            move |mut broker| async move {
                broker.setup().await;
                for did in ["target", "other"] {
                    let Packet::Subscribe(subscription) = broker.next().await else {
                        panic!("property SUBSCRIBE expected")
                    };
                    assert_eq!(
                        subscription.filters[0].path,
                        format!("master/appMsg/notify/iot/{did}/property/2.1")
                    );
                    broker
                        .send(Packet::SubAck(SubAck::new(
                            subscription.pkid,
                            vec![SubscribeReasonCode::QoS2],
                        )))
                        .await;
                }
                let Packet::Publish(read) = broker.next().await else {
                    panic!("read PUBLISH expected")
                };
                broker.acknowledge(&read).await;

                let payloads = [
                    ("appMsg/notify/iot/target/property/2.1", vec![0, 1]),
                    (
                        "appMsg/notify/iot/target/property/2.1",
                        wire::encode_mips(
                            100,
                            None,
                            &json!({"did":"target","siid":2,"piid":1,"value":"on"}),
                        ),
                    ),
                    (
                        "appMsg/notify/iot/target/property/2.1",
                        wire::encode_mips(
                            101,
                            None,
                            &json!({"did":"other","siid":2,"piid":1,"value":false}),
                        ),
                    ),
                    (
                        "appMsg/devListChange",
                        wire::encode_mips(102, None, &json!({})),
                    ),
                    ("reply", vec![0, 1]),
                    (
                        "appMsg/notify/iot/other/property/2.1",
                        wire::encode_mips(
                            103,
                            None,
                            &json!({"did":"other","siid":2,"piid":1,"value":true}),
                        ),
                    ),
                    (
                        "appMsg/devListChange",
                        wire::encode_mips(104, None, &json!({"devList":["other"]})),
                    ),
                ];
                for (index, (topic, payload)) in payloads.into_iter().enumerate() {
                    let pkid = index as u16 + 100;
                    let mut publish =
                        Publish::new(format!("{}/{topic}", broker.client_id), qos, payload);
                    publish.pkid = pkid;
                    broker.send(Packet::Publish(publish)).await;
                    match qos {
                        QoS::AtMostOnce => {}
                        QoS::AtLeastOnce => {
                            broker.wait_for(|packet| matches!(packet, Packet::PubAck(ack) if ack.pkid == pkid)).await;
                        }
                        QoS::ExactlyOnce => {
                            broker.wait_for(|packet| matches!(packet, Packet::PubRec(ack) if ack.pkid == pkid)).await;
                            broker.send(Packet::PubRel(PubRel::new(pkid))).await;
                            broker.wait_for(|packet| matches!(packet, Packet::PubComp(ack) if ack.pkid == pkid)).await;
                        }
                    }
                }
                broker.reply(&read, json!({"value":true}), 200).await;
                assert!(broker.next_optional().await.is_none());
            },
            move |handle, driver| async move {
                let binding = PropertyBinding { siid: 2, piid: 1 };
                let work = async {
                    handle.subscribe_property("target", binding).await.unwrap();
                    handle.subscribe_property("other", binding).await.unwrap();
                    assert!(
                        handle
                            .read_property("other", binding, |_| {})
                            .await
                            .result
                            .unwrap()
                    );
                    handle.close();
                };
                let (result, ()) =
                    future::zip(driver.run(|event| received.borrow_mut().push(event)), work).await;
                result.unwrap();
            },
        ));
        assert_eq!(
            *events.borrow(),
            [
                CentralEvent::PropertyValue {
                    did: "other".into(),
                    binding: PropertyBinding { siid: 2, piid: 1 },
                    value: serde_json::Value::Bool(true),
                },
                CentralEvent::DeviceListChanged {
                    dids: vec!["other".into()],
                },
            ]
        );
    }
}
