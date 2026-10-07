use super::*;

#[test]
fn mismatched_subscription_ack_finishes_the_request_before_disconnecting() {
    for unsubscribe in [false, true] {
        let (finished, done) = flume::bounded(1);
        block_on(pair(
            move |mut broker| async move {
                broker.setup().await;
                let Packet::Subscribe(subscribe) = broker.next().await else {
                    panic!("SUBSCRIBE expected");
                };
                if unsubscribe {
                    broker
                        .send(Packet::SubAck(SubAck::new(
                            subscribe.pkid,
                            vec![SubscribeReasonCode::QoS2],
                        )))
                        .await;
                    let Packet::Unsubscribe(packet) = broker.next().await else {
                        panic!("UNSUBSCRIBE expected");
                    };
                    broker
                        .send(Packet::SubAck(SubAck::new(
                            packet.pkid,
                            vec![SubscribeReasonCode::QoS2],
                        )))
                        .await;
                } else {
                    broker
                        .send(Packet::UnsubAck(UnsubAck {
                            pkid: subscribe.pkid,
                            reasons: vec![UnsubAckReason::Success],
                            properties: None,
                        }))
                        .await;
                }
                done.recv_async().await.unwrap();
            },
            move |handle, driver| async move {
                let work = async {
                    let binding = PropertyBinding { siid: 2, piid: 1 };
                    if unsubscribe {
                        handle.subscribe_property("target", binding).await.unwrap();
                        handle.unsubscribe_device("target").await
                    } else {
                        handle.subscribe_property("target", binding).await
                    }
                };
                let (driver_result, request_result) = future::zip(driver.run(|_| {}), work).await;
                assert_eq!(driver_result, Err(CentralError::Protocol));
                assert_eq!(request_result, Err(CentralError::Protocol));
                assert_eq!(handle.shared.outstanding.get(), 0);
                finished.send(()).unwrap();
            },
        ));
    }
}

#[test]
fn baseline_and_property_subscription_rejections_are_observed() {
    for baseline in [true, false] {
        block_on(pair(
            move |mut broker| async move {
                let Packet::Connect(connect) = broker.next().await else {
                    panic!("CONNECT expected")
                };
                broker.client_id = connect.client_id;
                broker
                    .send(Packet::ConnAck(ConnAck::new(
                        ConnectReturnCode::Success,
                        false,
                    )))
                    .await;
                let Packet::Subscribe(base) = broker.next().await else {
                    panic!("baseline SUBSCRIBE expected")
                };
                if baseline {
                    broker
                        .send(Packet::SubAck(SubAck::new(
                            base.pkid,
                            vec![
                                SubscribeReasonCode::QoS2,
                                SubscribeReasonCode::NotAuthorized,
                            ],
                        )))
                        .await;
                } else {
                    broker
                        .send(Packet::SubAck(SubAck::new(
                            base.pkid,
                            vec![SubscribeReasonCode::QoS2; 2],
                        )))
                        .await;
                    let Packet::Subscribe(property) = broker.next().await else {
                        panic!("property SUBSCRIBE expected")
                    };
                    broker
                        .send(Packet::SubAck(SubAck::new(
                            property.pkid,
                            vec![SubscribeReasonCode::NotAuthorized],
                        )))
                        .await;
                }
                async_io::Timer::after(Duration::from_millis(20)).await;
            },
            move |handle, driver| async move {
                let work = async {
                    assert_eq!(
                        handle
                            .subscribe_property("target", PropertyBinding { siid: 2, piid: 1 })
                            .await,
                        Err(CentralError::Rejected(135))
                    );
                    handle.close();
                };
                let (result, ()) = future::zip(driver.run(|_| {}), work).await;
                if baseline {
                    assert_eq!(result, Err(CentralError::Rejected(135)));
                } else {
                    result.unwrap();
                }
            },
        ));
    }
}

#[test]
fn receive_maximum_does_not_block_subscription_control_packets() {
    block_on(pair(
        |mut broker| async move {
            let Packet::Connect(connect) = broker.next().await else {
                panic!("connect expected")
            };
            broker.client_id = connect.client_id;
            let mut ack = ConnAck::new(ConnectReturnCode::Success, false);
            let mut properties = ConnAckProperties::new();
            properties.receive_max = Some(1);
            ack.properties = Some(properties);
            broker.send(Packet::ConnAck(ack)).await;
            let Packet::Subscribe(base) = broker.next().await else {
                panic!("base subscribe expected")
            };
            broker
                .send(Packet::SubAck(SubAck::new(
                    base.pkid,
                    vec![SubscribeReasonCode::QoS2; 2],
                )))
                .await;
            let Packet::Publish(first) = broker.next().await else {
                panic!("first read expected")
            };
            broker.subscription().await;
            broker.acknowledge(&first).await;
            broker.reply(&first, json!({"value":true}), 80).await;
            let Packet::Publish(second) = broker
                .wait_for(|packet| matches!(packet, Packet::Publish(_)))
                .await
            else {
                unreachable!()
            };
            broker.acknowledge(&second).await;
            broker.reply(&second, json!({"value":false}), 81).await;
            async_io::Timer::after(Duration::from_millis(20)).await;
        },
        |handle, driver| async move {
            let first =
                handle.read_property("target", PropertyBinding { siid: 2, piid: 1 }, |outcome| {
                    outcome.result.unwrap();
                });
            let second =
                handle.read_property("target", PropertyBinding { siid: 2, piid: 1 }, |outcome| {
                    outcome.result.unwrap();
                });
            let sub = handle.subscribe_property("target", PropertyBinding { siid: 2, piid: 1 });
            let work = async {
                sub.await.unwrap();
                assert!(first.await.result.unwrap());
                assert!(!second.await.result.unwrap());
                handle.close();
            };
            let (result, ()) = future::zip(driver.run(|_| {}), work).await;
            result.unwrap();
        },
    ));
}
