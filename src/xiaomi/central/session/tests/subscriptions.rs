use super::*;

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
                handle.read_property("target", PropertyBinding { siid: 2, piid: 1 }, |result| {
                    result.unwrap();
                });
            let second =
                handle.read_property("target", PropertyBinding { siid: 2, piid: 1 }, |result| {
                    result.unwrap();
                });
            let sub = handle.subscribe_property("target", PropertyBinding { siid: 2, piid: 1 });
            let work = async {
                sub.await.unwrap();
                assert!(first.await.unwrap());
                assert!(!second.await.unwrap());
                handle.close();
            };
            let (result, ()) = future::zip(driver.run(|_| {}), work).await;
            result.unwrap();
        },
    ));
}
