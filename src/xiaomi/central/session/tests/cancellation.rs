use super::*;

#[test]
fn cancellation_precedes_a_late_suback_in_the_same_received_buffer() {
    block_on(pair(
        |mut broker| async move {
            broker.setup().await;
            let Packet::Subscribe(subscribe) = broker.next().await else {
                panic!()
            };
            let list = Publish::new(
                format!("{}/appMsg/devListChange", broker.client_id),
                QoS::AtMostOnce,
                wire::encode_mips(1, None, &json!({"devList":["target"]})),
            );
            let mut bytes = wire::encode_packet(&Packet::Publish(list)).unwrap();
            bytes.extend(
                wire::encode_packet(&Packet::SubAck(SubAck::new(
                    subscribe.pkid,
                    vec![SubscribeReasonCode::QoS2],
                )))
                .unwrap(),
            );
            broker.stream.write_all(&bytes).await.unwrap();
            broker.stream.flush().await.unwrap();
            assert!(broker.next_optional().await.is_none());
        },
        |handle, driver| async move {
            let subscribed =
                handle.subscribe_property("target", PropertyBinding { siid: 2, piid: 1 });
            let cancelling = handle.clone();
            let work = async {
                assert_eq!(subscribed.await, Err(CentralError::Cancelled));
                handle.close();
            };
            let (result, ()) = future::zip(
                driver.run(move |event| {
                    if matches!(event, CentralEvent::DeviceListChanged { .. }) {
                        cancelling.cancel_device("target").unwrap();
                    }
                }),
                work,
            )
            .await;
            result.unwrap();
        },
    ));
}

#[test]
fn cancel_and_unsubscribe_end_target_requests_ignore_late_results_and_keep_other_devices() {
    let ready = Rc::new(Cell::new(false));
    let server_ready = ready.clone();
    block_on(pair(
        move |mut broker| async move {
            broker.setup().await;
            let Packet::Subscribe(target_subscribe) = broker.next().await else {
                panic!()
            };
            let Packet::Subscribe(other_subscribe) = broker.next().await else {
                panic!()
            };
            assert!(target_subscribe.filters[0].path.contains("/target/"));
            assert!(other_subscribe.filters[0].path.contains("/other/"));
            broker
                .send(Packet::SubAck(SubAck::new(
                    other_subscribe.pkid,
                    vec![SubscribeReasonCode::QoS2],
                )))
                .await;
            let Packet::Publish(write) = broker.next().await else {
                panic!()
            };
            broker.acknowledge(&write).await;
            let Packet::Publish(read) = broker.next().await else {
                panic!()
            };
            broker.acknowledge(&read).await;
            server_ready.set(true);
            let Packet::Unsubscribe(unsubscribe) = broker.next().await else {
                panic!()
            };
            assert_eq!(
                unsubscribe.filters,
                ["master/appMsg/notify/iot/target/property/2.1"]
            );
            broker
                .send(Packet::SubAck(SubAck::new(
                    target_subscribe.pkid,
                    vec![SubscribeReasonCode::QoS2],
                )))
                .await;
            broker
                .reply(&write, json!({"result":[{"did":"target","code":0}]}), 90)
                .await;
            broker.reply(&read, json!({"value":true}), 91).await;
            broker
                .send(notification(&broker.client_id, "target", true))
                .await;
            broker
                .send(notification(&broker.client_id, "other", false))
                .await;
            let mut ack = UnsubAck::new(unsubscribe.pkid);
            ack.reasons = vec![UnsubAckReason::Success];
            broker.send(Packet::UnsubAck(ack)).await;
            let Packet::Subscribe(rejoined) = broker.next().await else {
                panic!()
            };
            assert_eq!(
                rejoined.filters[0].path,
                "master/appMsg/notify/iot/target/property/2.1"
            );
            broker
                .send(Packet::SubAck(SubAck::new(
                    rejoined.pkid,
                    vec![SubscribeReasonCode::QoS2],
                )))
                .await;
            broker
                .send(notification(&broker.client_id, "target", false))
                .await;
            assert!(broker.next_optional().await.is_none());
        },
        move |handle, driver| async move {
            let callbacks = Rc::new(RefCell::new(Vec::new()));
            let subscribed =
                handle.subscribe_property("target", PropertyBinding { siid: 2, piid: 1 });
            let other = handle.subscribe_property("other", PropertyBinding { siid: 2, piid: 1 });
            let writing = callbacks.clone();
            let write = handle.set_property(
                "target",
                PropertyBinding { siid: 2, piid: 1 },
                true,
                move |outcome| writing.borrow_mut().push(outcome.result),
            );
            let reading = callbacks.clone();
            let read = handle.read_property_value(
                "target",
                PropertyBinding { siid: 2, piid: 1 },
                move |outcome| reading.borrow_mut().push(outcome.result.map(|_| ())),
            );
            let values = Rc::new(RefCell::new(Vec::new()));
            let observed = values.clone();
            let work = async {
                other.await.unwrap();
                while !ready.get() {
                    future::yield_now().await;
                }
                handle.cancel_device("target").unwrap();
                let removed = handle.unsubscribe_device("target");
                assert_eq!(subscribed.await, Err(CentralError::Cancelled));
                assert_eq!(write.await.result, Err(CentralError::Cancelled));
                assert_eq!(read.await.result, Err(CentralError::Cancelled));
                removed.await.unwrap();
                assert_eq!(
                    *callbacks.borrow(),
                    [Err(CentralError::Cancelled), Err(CentralError::Cancelled)]
                );
                assert_eq!(*values.borrow(), [("other".to_owned(), false)]);
                handle
                    .subscribe_property("target", PropertyBinding { siid: 2, piid: 1 })
                    .await
                    .unwrap();
                while values.borrow().len() < 2 {
                    future::yield_now().await;
                }
                assert_eq!(
                    *values.borrow(),
                    [("other".to_owned(), false), ("target".to_owned(), false)]
                );
                handle.close();
            };
            let (result, ()) = future::zip(
                driver.run(move |event| {
                    if let CentralEvent::PropertyValue { did, value, .. } = event {
                        observed.borrow_mut().push((did, value.as_bool().unwrap()));
                    }
                }),
                work,
            )
            .await;
            result.unwrap();
        },
    ));
}

#[test]
fn queued_cancellation_only_ends_the_named_device() {
    block_on(pair(
        |mut broker| async move {
            broker.setup().await;
            let Packet::Publish(read) = broker.next().await else {
                panic!()
            };
            let payload = wire::decode_mips(&read.payload).unwrap().payload;
            assert_eq!(payload["did"], "other");
            broker.acknowledge(&read).await;
            broker.reply(&read, json!({"value":false}), 99).await;
            assert!(broker.next_optional().await.is_none());
        },
        |handle, driver| async move {
            let cancelled = handle.read_property_value(
                "target",
                PropertyBinding { siid: 2, piid: 1 },
                |outcome| assert_eq!(outcome.result, Err(CentralError::Cancelled)),
            );
            let survivor = handle.read_property_value(
                "other",
                PropertyBinding { siid: 2, piid: 1 },
                |outcome| assert_eq!(outcome.result, Ok(json!(false))),
            );
            handle.cancel_device("target").unwrap();
            let work = async {
                assert_eq!(cancelled.await.result, Err(CentralError::Cancelled));
                assert_eq!(survivor.await.result.unwrap(), json!(false));
                handle.close();
            };
            let (result, ()) = future::zip(driver.run(|_| {}), work).await;
            result.unwrap();
        },
    ));
}
