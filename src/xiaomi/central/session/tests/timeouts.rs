use super::*;

#[test]
fn default_request_timeout_is_ten_seconds() {
    assert_eq!(Timing::default().request, Duration::from_secs(10));
}

#[test]
fn refresh_read_timeout_does_not_shorten_other_requests() {
    for request_timeout in [Duration::from_secs(10), Duration::from_millis(25)] {
        let handle = queued_handle(Timing {
            request: request_timeout,
            keepalive: None,
        });
        let binding = PropertyBinding { siid: 2, piid: 1 };
        let before = Instant::now();
        drop(handle.read_property_with_timeout("target", binding, Duration::from_secs(6), |_| {}));
        drop(handle.read_property("target", binding, |_| {}));
        drop(handle.set_property("target", binding, true, |_| {}));
        drop(handle.get_devices(|_| {}));
        drop(handle.get_devices_filtered(&["target".into()], |_| {}));
        let after = Instant::now();
        let queue = handle.shared.queue.borrow();
        assert_eq!(queue.len(), 5);
        for (index, request) in queue.iter().enumerate() {
            let timeout = if index == 0 {
                request_timeout.min(Duration::from_secs(6))
            } else {
                request_timeout
            };
            assert!(request.deadline >= before + timeout);
            assert!(request.deadline <= after + timeout);
        }
    }
}

#[test]
fn partial_reply_survives_timeout_and_late_ids_do_not_complete_next_request() {
    let callbacks = Rc::new(RefCell::new(Vec::new()));
    let results = callbacks.clone();
    block_on(pair(
        |mut broker| async move {
            broker.setup().await;
            let Packet::Publish(old) = broker.next().await else {
                panic!("old read expected")
            };
            broker.acknowledge(&old).await;
            let late = encode_packet(&broker.business(&old, json!({"value":true}))).unwrap();
            broker.stream.write_all(&late[..7]).await.unwrap();
            broker.stream.flush().await.unwrap();
            let Packet::Publish(new) = broker
                .wait_for(|packet| matches!(packet, Packet::Publish(_)))
                .await
            else {
                unreachable!()
            };
            assert_ne!(
                wire::decode_mips(&old.payload).unwrap().id,
                wire::decode_mips(&new.payload).unwrap().id
            );
            broker.stream.write_all(&late[7..]).await.unwrap();
            broker.stream.flush().await.unwrap();
            broker.acknowledge(&new).await;
            let mut wrong_topic = match broker.business(&new, json!({"value":true})) {
                Packet::Publish(publish) => publish,
                _ => unreachable!(),
            };
            wrong_topic.topic = "old-session/reply".into();
            broker
                .batch(vec![
                    Packet::Publish(wrong_topic),
                    broker.business(&old, json!({"value":true})),
                    broker.business(&new, json!({"value":false})),
                ])
                .await;
            async_io::Timer::after(Duration::from_millis(20)).await;
        },
        |handle, driver| async move {
            let work = async {
                let callback = results.clone();
                assert_eq!(
                    handle
                        .read_property(
                            "target",
                            PropertyBinding { siid: 2, piid: 1 },
                            move |outcome| callback.borrow_mut().push(outcome.result)
                        )
                        .await
                        .result,
                    Err(CentralError::Timeout)
                );
                assert_eq!(
                    handle
                        .read_property(
                            "target",
                            PropertyBinding { siid: 2, piid: 1 },
                            move |outcome| results.borrow_mut().push(outcome.result)
                        )
                        .await
                        .result,
                    Ok(false)
                );
                handle.close();
            };
            let (result, ()) =
                future::zip(driver.run(|_| panic!("no notification expected")), work).await;
            result.unwrap();
        },
    ));
    assert_eq!(*callbacks.borrow(), [Err(CentralError::Timeout), Ok(false)]);
}

#[test]
fn keepalive_requires_ping_response_and_does_not_block_requests() {
    for responds in [false, true] {
        block_on(pair_io(
            move |mut broker| async move {
                broker.setup().await;
                assert!(matches!(broker.next().await, Packet::PingReq));
                if responds {
                    broker.send(Packet::PingResp).await;
                    let Packet::Publish(read) = broker
                        .wait_for(|packet| matches!(packet, Packet::Publish(_)))
                        .await
                    else {
                        unreachable!()
                    };
                    broker.acknowledge(&read).await;
                    broker.reply(&read, json!({"value":true}), 33).await;
                }
                async_io::Timer::after(Duration::from_millis(130)).await;
            },
            move |handle, driver| async move {
                if responds {
                    let work = async {
                        async_io::Timer::after(Duration::from_millis(45)).await;
                        assert!(
                            handle
                                .read_property(
                                    "target",
                                    PropertyBinding { siid: 2, piid: 1 },
                                    |outcome| {
                                        outcome.result.unwrap();
                                    }
                                )
                                .await
                                .result
                                .unwrap()
                        );
                        handle.close();
                    };
                    let (result, ()) = future::zip(driver.run(|_| {}), work).await;
                    result.unwrap();
                } else {
                    assert_eq!(driver.run(|_| {}).await, Err(CentralError::Timeout));
                }
            },
            Timing {
                request: Duration::from_millis(100),
                keepalive: Some(Duration::from_millis(30)),
            },
            |stream| stream,
        ));
    }
}

#[test]
fn queued_requests_use_the_network_timeout_and_reject_invalid_bindings() {
    let callbacks = Rc::new(RefCell::new(Vec::new()));
    let results = callbacks.clone();
    block_on(pair(
        |mut broker| async move {
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
            let Packet::Subscribe(subscribe) = broker.next().await else {
                panic!("baseline SUBSCRIBE expected")
            };
            async_io::Timer::after(Duration::from_millis(40)).await;
            broker
                .send(Packet::SubAck(SubAck::new(
                    subscribe.pkid,
                    vec![SubscribeReasonCode::QoS2; 2],
                )))
                .await;
            let Packet::Publish(read) = broker.next().await else {
                panic!("only new read may be sent")
            };
            assert_eq!(read.topic, "master/proxy/get");
            broker.acknowledge(&read).await;
            broker.reply(&read, json!({"value":false}), 70).await;
            async_io::Timer::after(Duration::from_millis(20)).await;
        },
        |handle, driver| async move {
            let callback = results.clone();
            let mut fast = handle.clone();
            fast.timing.request = Duration::from_millis(10);
            let expired = fast.set_property(
                "target",
                PropertyBinding { siid: 2, piid: 1 },
                true,
                move |outcome| callback.borrow_mut().push(outcome.result),
            );
            let work = async {
                assert_eq!(expired.await.result, Err(CentralError::Timeout));
                assert_eq!(
                    handle
                        .set_property(
                            "target",
                            PropertyBinding { siid: 0, piid: 1 },
                            true,
                            |outcome| assert_eq!(outcome.result, Err(CentralError::InvalidInput))
                        )
                        .await
                        .result,
                    Err(CentralError::InvalidInput)
                );
                assert!(
                    !handle
                        .read_property("target", PropertyBinding { siid: 2, piid: 1 }, |outcome| {
                            outcome.result.unwrap();
                        })
                        .await
                        .result
                        .unwrap()
                );
                handle.close();
            };
            let (result, ()) = future::zip(driver.run(|_| {}), work).await;
            result.unwrap();
        },
    ));
    assert_eq!(*callbacks.borrow(), [Err(CentralError::Timeout)]);
}
