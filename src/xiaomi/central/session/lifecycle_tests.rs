use super::super::request::Response;
use super::*;

fn access(name: Value) -> Value {
    json!({"name":name,"model":"cuco.plug.v3","urn":"outlet","online":true,"specV2Access":true,"pushAvailable":true})
}

#[test]
fn full_device_lists_preserve_names_validate_flags_and_single_did_scope() {
    let value = json!({"devList":{"one":access(json!("插座 一")),"two":access(json!("")),"three":access(Value::Null)}});
    let Response::Devices(devices) = parse_response(&RequestKind::ListAll, &value).unwrap() else {
        panic!()
    };
    assert_eq!(devices.len(), 3);
    assert_eq!(
        devices
            .iter()
            .find(|device| device.did == "one")
            .unwrap()
            .name
            .as_deref(),
        Some("插座 一")
    );
    assert!(
        devices
            .iter()
            .filter(|device| device.did != "one")
            .all(|device| device.name.is_none())
    );
    let mut invalid = value.clone();
    invalid["devList"]["two"]["pushAvailable"] = Value::Null;
    assert!(matches!(
        parse_response(&RequestKind::ListAll, &invalid),
        Err(CentralError::Protocol)
    ));
    assert!(matches!(
        parse_response(&RequestKind::List { did: "one".into() }, &invalid),
        Ok(Response::Device(Some(_)))
    ));
    let publish = RequestKind::ListAll.publication(1, "reply").unwrap();
    let payload = wire::decode_mips(&publish.payload).unwrap().payload;
    assert!(payload.get("filter").is_none());
    assert_eq!(
        payload["info"],
        json!([
            "name",
            "model",
            "urn",
            "online",
            "specV2Access",
            "pushAvailable"
        ])
    );
}

#[test]
fn full_and_single_device_requests_share_the_session_and_keep_their_response_scope() {
    block_on(pair(
        |mut broker| async move {
            broker.setup().await;
            let Packet::Publish(all) = broker.next().await else {
                panic!()
            };
            assert!(
                wire::decode_mips(&all.payload)
                    .unwrap()
                    .payload
                    .get("filter")
                    .is_none()
            );
            broker.acknowledge(&all).await;
            let Packet::Publish(single) = broker.next().await else {
                panic!()
            };
            assert_eq!(
                wire::decode_mips(&single.payload).unwrap().payload["filter"]["did"],
                json!(["missing"])
            );
            broker.acknowledge(&single).await;
            broker
                .reply(&single, json!({"devList":{"unrelated":null}}), 50)
                .await;
            broker
                .reply(
                    &all,
                    json!({"devList":{"one":access(json!("插座 一")),"two":access(Value::Null)}}),
                    51,
                )
                .await;
            assert!(broker.next_optional().await.is_none());
        },
        |handle, driver| async move {
            let all = handle.get_devices(|result| assert_eq!(result.unwrap().len(), 2));
            let single =
                handle.get_device_list("missing", |result| assert_eq!(result.unwrap(), None));
            let work = async {
                assert_eq!(single.await.unwrap(), None);
                let devices = all.await.unwrap();
                assert_eq!(devices[0].did, "one");
                assert_eq!(devices[0].name.as_deref(), Some("插座 一"));
                assert_eq!(devices[1].did, "two");
                handle.close();
            };
            let (result, ()) = future::zip(driver.run(|_| {}), work).await;
            result.unwrap();
        },
    ));
}

#[test]
fn writes_for_different_devices_and_bindings_can_complete_in_reverse_order() {
    block_on(pair(
        |mut broker| async move {
            broker.setup().await;
            let mut writes = Vec::new();
            for _ in 0..3 {
                let Packet::Publish(write) = broker.next().await else {
                    panic!()
                };
                assert_eq!(write.topic, "master/proxy/rpcReq");
                broker.acknowledge(&write).await;
                writes.push(write);
            }
            for (index, write) in writes.iter().rev().enumerate() {
                let payload = wire::decode_mips(&write.payload).unwrap().payload;
                let property = &payload["rpc"]["params"][0];
                broker.reply(write, json!({"result":[{"did":property["did"],"siid":property["siid"],"piid":property["piid"],"code":0}]}), 80 + index as u16).await;
            }
            assert!(broker.next_optional().await.is_none());
        },
        |handle, driver| async move {
            let callbacks = Rc::new(RefCell::new(Vec::new()));
            let mut writes = Vec::new();
            for (did, siid) in [("one", 2), ("one", 3), ("two", 2)] {
                let callbacks = callbacks.clone();
                writes.push(handle.set_property(
                    did,
                    PropertyBinding { siid, piid: 1 },
                    true,
                    move |result| {
                        result.unwrap();
                        callbacks.borrow_mut().push((did, siid));
                    },
                ));
            }
            let work = async {
                for write in writes {
                    write.await.unwrap();
                }
                assert_eq!(*callbacks.borrow(), [("two", 2), ("one", 3), ("one", 2)]);
                handle.close();
            };
            let (result, ()) = future::zip(driver.run(|_| {}), work).await;
            result.unwrap();
        },
    ));
}

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

fn notification(client_id: &str, did: &str, value: bool) -> Packet {
    Packet::Publish(Publish::new(
        format!("{client_id}/appMsg/notify/iot/{did}/property/2.1"),
        QoS::AtMostOnce,
        wire::encode_mips(1, None, &json!({"did":did,"siid":2,"piid":1,"value":value})),
    ))
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
                move |result| writing.borrow_mut().push(result),
            );
            let reading = callbacks.clone();
            let read = handle.read_property(
                "target",
                PropertyBinding { siid: 2, piid: 1 },
                move |result| reading.borrow_mut().push(result.map(|_| ())),
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
                assert_eq!(write.await, Err(CentralError::Cancelled));
                assert_eq!(read.await, Err(CentralError::Cancelled));
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
                    if let CentralEvent::Property { did, value, .. } = event {
                        observed.borrow_mut().push((did, value));
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
            let cancelled =
                handle.read_property("target", PropertyBinding { siid: 2, piid: 1 }, |result| {
                    assert_eq!(result, Err(CentralError::Cancelled))
                });
            let survivor =
                handle.read_property("other", PropertyBinding { siid: 2, piid: 1 }, |result| {
                    assert_eq!(result, Ok(false))
                });
            handle.cancel_device("target").unwrap();
            let work = async {
                assert_eq!(cancelled.await, Err(CentralError::Cancelled));
                assert!(!survivor.await.unwrap());
                handle.close();
            };
            let (result, ()) = future::zip(driver.run(|_| {}), work).await;
            result.unwrap();
        },
    ));
}
