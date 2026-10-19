use super::*;

#[test]
fn exhausted_unacknowledged_subscriptions_close_the_connection() {
    let (finished, done) = flume::bounded(1);
    future::block_on(exercise(
        timing(),
        move |mut broker| async move {
            broker.setup().await;
            for siid in 1..=super::super::LIMIT {
                broker
                    .subscribe(&format!("device/target/up/properties_changed/{siid}/1"))
                    .await;
            }
            done.recv_async().await.unwrap();
        },
        move |handle, driver| async move {
            let pending: Vec<_> = (1..=super::super::LIMIT)
                .map(|siid| {
                    handle.subscribe_property(
                        "target",
                        PropertyBinding {
                            siid: siid as u32,
                            piid: 1,
                        },
                    )
                })
                .collect();
            let work = async {
                for subscription in pending {
                    assert_eq!(subscription.await, Err(NotificationError::Timeout));
                }
                assert_eq!(
                    handle.subscribe_property("other", POWER).await,
                    Err(NotificationError::Timeout)
                );
            };
            let (result, ()) = future::zip(driver.run(|_| {}), work).await;
            assert_eq!(result, Err(NotificationError::Timeout));
            assert_eq!(handle.shared.outstanding.get(), 0);
            finished.send(()).unwrap();
        },
    ));
}

#[test]
fn mismatched_subscription_ack_finishes_the_request_before_disconnecting() {
    for unsubscribe in [false, true] {
        let (finished, done) = flume::bounded(1);
        future::block_on(exercise(
            timing(),
            move |mut broker| async move {
                broker.setup().await;
                let mut pkid = broker
                    .subscribe("device/target/up/properties_changed/2/1")
                    .await;
                if unsubscribe {
                    broker.accept(pkid).await;
                    let Packet::Unsubscribe(packet) = broker.next().await else {
                        panic!("UNSUBSCRIBE expected");
                    };
                    pkid = packet.pkid;
                    broker.accept(pkid).await;
                } else {
                    broker
                        .send(Packet::UnsubAck(UnsubAck {
                            pkid,
                            reasons: vec![UnsubAckReason::Success],
                            properties: None,
                        }))
                        .await;
                }
                done.recv_async().await.unwrap();
            },
            move |handle, driver| async move {
                let work = async {
                    if unsubscribe {
                        handle.subscribe_property("target", POWER).await.unwrap();
                        handle.unsubscribe_device("target").await
                    } else {
                        handle.subscribe_property("target", POWER).await
                    }
                };
                let (driver_result, request_result) = future::zip(driver.run(|_| {}), work).await;
                assert_eq!(driver_result, Err(NotificationError::Protocol));
                assert_eq!(request_result, Err(NotificationError::Protocol));
                assert_eq!(handle.shared.outstanding.get(), 0);
                finished.send(()).unwrap();
            },
        ));
    }
}

#[test]
fn subscription_rejection_is_per_request_and_ble_online_is_skipped() {
    let (finished, done) = flume::bounded(1);
    future::block_on(exercise(
        timing(),
        move |mut broker| async move {
            broker.setup().await;
            let pkid = broker
                .subscribe("device/target/up/properties_changed/2/1")
                .await;
            broker
                .send(Packet::SubAck(SubAck {
                    pkid,
                    return_codes: vec![SubscribeReasonCode::NotAuthorized],
                    properties: None,
                }))
                .await;
            let pkid = broker
                .subscribe("device/other/up/properties_changed/2/1")
                .await;
            broker.accept(pkid).await;
            done.recv_async().await.unwrap();
        },
        move |handle, driver| async move {
            let (result, ()) = future::zip(driver.run(|_| {}), async {
                assert_eq!(
                    handle.subscribe_property("target", POWER).await,
                    Err(NotificationError::Unauthorized)
                );
                handle.subscribe_online("blt.123").await.unwrap();
                handle.subscribe_online("proxy.123").await.unwrap();
                handle.subscribe_property("other", POWER).await.unwrap();
                handle.close();
            })
            .await;
            result.unwrap();
            finished.send(()).unwrap();
        },
    ));
}

#[test]
fn late_suback_after_timeout_does_not_restore_retired_subscription() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let collected = events.clone();
    let (finished, done) = flume::bounded(1);
    future::block_on(exercise(
        Timing {
            request: Duration::from_millis(25),
            keepalive: None,
        },
        move |mut broker| async move {
            broker.setup().await;
            let old = broker
                .subscribe("device/target/up/properties_changed/2/1")
                .await;
            let current = broker
                .subscribe("device/target/up/properties_changed/2/1")
                .await;
            assert_ne!(old, current);
            broker.accept(old).await;
            broker.property(json!(false)).await;
            broker.accept(current).await;
            broker.property(json!(true)).await;
            done.recv_async().await.unwrap();
        },
        move |handle, driver| async move {
            let stop = handle.clone();
            let (result, ()) = future::zip(
                driver.run(move |event| {
                    collected.borrow_mut().push(event);
                    finished.send(()).unwrap();
                    stop.close();
                }),
                async {
                    assert_eq!(
                        handle.subscribe_property("target", POWER).await,
                        Err(NotificationError::Timeout)
                    );
                    handle.subscribe_property("target", POWER).await.unwrap();
                },
            )
            .await;
            result.unwrap();
        },
    ));
    assert_eq!(
        *events.borrow(),
        [NotificationEvent::PropertyValue {
            did: "target".into(),
            binding: POWER,
            value: serde_json::Value::Bool(true)
        }]
    );
}

#[test]
fn pending_subscriptions_finish_on_disconnect() {
    future::block_on(exercise(
        timing(),
        |mut broker| async move {
            broker.setup().await;
            broker
                .subscribe("device/target/up/properties_changed/2/1")
                .await;
            broker.send(Packet::Disconnect(Disconnect::new())).await;
        },
        |handle, driver| async move {
            let (result, sub) = future::zip(
                driver.run(|_| {}),
                handle.subscribe_property("target", POWER),
            )
            .await;
            assert_eq!(result, Err(NotificationError::Disconnected));
            assert_eq!(sub, Err(NotificationError::Disconnected));
        },
    ));
}

#[test]
fn retiring_queued_subscriptions_keeps_the_request_queue_bounded() {
    let (handle, driver) = NotificationDriver::new(
        Box::new(futures_lite::io::Cursor::new(Vec::new())),
        mqtt::Frames::default(),
        "account".into(),
        timing(),
        mqtt::MAX_FRAME_SIZE,
    );
    for siid in 1..=64 {
        drop(handle.subscribe_property("target", PropertyBinding { siid, piid: 1 }));
    }
    assert_eq!(
        future::block_on(handle.subscribe_property("other", POWER)),
        Err(NotificationError::QueueFull)
    );
    drop(handle.unsubscribe_device("target"));
    assert_eq!(handle.shared.queue.borrow().len(), 1);
    assert_eq!(handle.shared.outstanding.get(), 1);
    for siid in 1..64 {
        drop(handle.subscribe_property("other", PropertyBinding { siid, piid: 1 }));
    }
    assert_eq!(handle.shared.queue.borrow().len(), 64);
    assert_eq!(handle.shared.outstanding.get(), 64);
    assert_eq!(
        future::block_on(handle.subscribe_property("full", POWER)),
        Err(NotificationError::QueueFull)
    );
    drop(driver);
    assert_eq!(handle.shared.outstanding.get(), 0);
}

#[test]
fn unsubscribe_properties_keeps_online_until_device_is_removed() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let collected = events.clone();
    let (observed, observation) = flume::bounded(1);
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
            let Packet::Unsubscribe(unsub) = broker.next().await else {
                panic!();
            };
            assert_eq!(unsub.filters, ["device/target/up/properties_changed/2/1"]);
            broker
                .send(Packet::UnsubAck(UnsubAck {
                    pkid: unsub.pkid,
                    reasons: vec![UnsubAckReason::Success],
                    properties: None,
                }))
                .await;
            broker.property(json!(true)).await;
            broker
                .send(Packet::Publish(Publish::new(
                    "device/target/state/offline",
                    QoS::AtMostOnce,
                    json!({"device_id":"target","event":"offline"}).to_string(),
                )))
                .await;
            let Packet::Unsubscribe(unsub) = broker.next().await else {
                panic!();
            };
            assert_eq!(unsub.filters, ["device/target/state/#"]);
            broker
                .send(Packet::UnsubAck(UnsubAck {
                    pkid: unsub.pkid,
                    reasons: vec![UnsubAckReason::Success],
                    properties: None,
                }))
                .await;
            broker
                .send(Packet::Publish(Publish::new(
                    "device/target/state/online",
                    QoS::AtMostOnce,
                    json!({"device_id":"target","event":"online"}).to_string(),
                )))
                .await;
            let pkid = broker
                .subscribe("device/other/up/properties_changed/2/1")
                .await;
            broker.accept(pkid).await;
            broker
                .send(Packet::Publish(Publish::new(
                    "device/other/up/properties_changed/2/1",
                    QoS::AtMostOnce,
                    json!({"params":{"did":"other","siid":2,"piid":1,"value":true}}).to_string(),
                )))
                .await;
            done.recv_async().await.unwrap();
        },
        move |handle, driver| async move {
            let stop = handle.clone();
            let (result, ()) = future::zip(
                driver.run(move |event| {
                    collected.borrow_mut().push(event);
                    if collected.borrow().len() == 1 {
                        observed.send(()).unwrap();
                    }
                    if collected.borrow().len() == 2 {
                        finished.send(()).unwrap();
                        stop.close();
                    }
                }),
                async {
                    handle.subscribe_property("target", POWER).await.unwrap();
                    handle.subscribe_online("target").await.unwrap();
                    handle.unsubscribe_properties("target").await.unwrap();
                    observation.recv_async().await.unwrap();
                    handle.unsubscribe_device("target").await.unwrap();
                    handle.subscribe_property("other", POWER).await.unwrap();
                },
            )
            .await;
            result.unwrap();
        },
    ));
    assert_eq!(
        *events.borrow(),
        [
            NotificationEvent::Online {
                did: "target".into(),
                online: false
            },
            NotificationEvent::PropertyValue {
                did: "other".into(),
                binding: POWER,
                value: serde_json::Value::Bool(true)
            }
        ]
    );
}

#[test]
fn cancel_pending_subscription_suppresses_its_late_suback() {
    let (retire, retired) = flume::bounded(1);
    let (finished, done) = flume::bounded(1);
    let events = Rc::new(RefCell::new(Vec::new()));
    let collected = events.clone();
    future::block_on(exercise(
        timing(),
        move |mut broker| async move {
            broker.setup().await;
            let old = broker
                .subscribe("device/target/up/properties_changed/2/1")
                .await;
            retire.send(()).unwrap();
            let Packet::Unsubscribe(unsub) = broker.next().await else {
                panic!();
            };
            broker.accept(old).await;
            broker.property(json!(false)).await;
            broker
                .send(Packet::UnsubAck(UnsubAck {
                    pkid: unsub.pkid,
                    reasons: vec![UnsubAckReason::Success],
                    properties: None,
                }))
                .await;
            let current = broker
                .subscribe("device/target/up/properties_changed/2/1")
                .await;
            assert_ne!(old, current);
            broker.accept(current).await;
            broker.property(json!(true)).await;
            done.recv_async().await.unwrap();
        },
        move |handle, driver| async move {
            let stop = handle.clone();
            let (result, ()) = future::zip(
                driver.run(move |event| {
                    collected.borrow_mut().push(event);
                    finished.send(()).unwrap();
                    stop.close();
                }),
                async {
                    let original = handle.subscribe_property("target", POWER);
                    retired.recv_async().await.unwrap();
                    handle.unsubscribe_properties("target").await.unwrap();
                    assert_eq!(original.await, Err(NotificationError::Cancelled));
                    handle.subscribe_property("target", POWER).await.unwrap();
                },
            )
            .await;
            result.unwrap();
        },
    ));
    assert_eq!(
        *events.borrow(),
        [NotificationEvent::PropertyValue {
            did: "target".into(),
            binding: POWER,
            value: serde_json::Value::Bool(true)
        }]
    );
}
