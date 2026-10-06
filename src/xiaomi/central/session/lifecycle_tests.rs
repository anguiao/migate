use super::super::request::Response;
use super::*;

fn access(name: Value) -> Value {
    json!({"name":name,"model":"cuco.plug.v3","urn":"outlet","online":true,"specV2Access":true,"pushAvailable":true})
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
                CentralEvent::Property {
                    did: "other".into(),
                    binding: PropertyBinding { siid: 2, piid: 1 },
                    value: true,
                },
                CentralEvent::DeviceListChanged {
                    dids: vec!["other".into()],
                },
            ]
        );
    }
}

#[test]
fn incomplete_candidates_do_not_reject_the_complete_device_list() {
    let mut bad = access(json!("pending"));
    bad.as_object_mut().unwrap().remove("pushAvailable");
    let response = parse_response(
        &RequestKind::ListAll,
        &json!({"devList":{"good":access(json!("ready")),"bad":bad}}),
    );
    assert!(
        response.is_ok(),
        "a candidate error must not reject the container"
    );
    let Response::Devices(entries) = response.unwrap() else {
        panic!()
    };
    assert!(
        entries
            .iter()
            .find(|entry| entry.did == "good")
            .unwrap()
            .access
            .is_ok()
    );
    assert_eq!(
        entries
            .iter()
            .find(|entry| entry.did == "bad")
            .unwrap()
            .access,
        Err(CentralError::Protocol)
    );
    assert!(matches!(
        parse_response(&RequestKind::ListAll, &json!({"devList":{"bad did":{}}})),
        Err(CentralError::Protocol)
    ));
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
            .access
            .as_ref()
            .unwrap()
            .name
            .as_deref(),
        Some("插座 一")
    );
    assert!(
        devices
            .iter()
            .filter(|device| device.did != "one")
            .all(|device| device.access.as_ref().unwrap().name.is_none())
    );
    let mut invalid = value.clone();
    invalid["devList"]["two"]["pushAvailable"] = Value::Null;
    let Response::Devices(entries) = parse_response(&RequestKind::ListAll, &invalid).unwrap()
    else {
        panic!()
    };
    assert!(
        entries
            .iter()
            .find(|entry| entry.did == "two")
            .unwrap()
            .access
            .is_err()
    );
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
fn filtered_device_requests_preserve_their_payload_and_response_scope() {
    let handle = queued_handle(Timing::default());
    let observed = Rc::new(RefCell::new(None));
    let callback = observed.clone();
    let dids = vec!["one".into(), "missing".into(), "bad".into()];
    let future = handle.get_devices_filtered(&dids, move |result| {
        *callback.borrow_mut() = Some(result);
    });
    let request = handle.shared.queue.borrow_mut().pop_front().unwrap();
    let publish = request.kind.publication(1, "reply").unwrap();
    assert_eq!(publish.topic, "master/proxy/getDevList");
    let payload = wire::decode_mips(&publish.payload).unwrap().payload;
    assert_eq!(payload["filter"]["did"], json!(dids));
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
    let reply = json!({"devList":{
        "one":access(json!("插座 一")),
        "bad":{},
        "unrelated":access(json!("other")),
        "invalid unrelated did":null
    }});
    (request.complete)(
        parse_response(&request.kind, &reply),
        super::super::RequestStage::BusinessResult,
        None,
    );
    let entries = block_on(future).unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].did, "bad");
    assert_eq!(entries[0].access, Err(CentralError::Protocol));
    assert_eq!(entries[1].did, "one");
    assert_eq!(
        entries[1].access.as_ref().unwrap().name.as_deref(),
        Some("插座 一")
    );
    assert_eq!(*observed.borrow(), Some(Ok(entries)));
}

#[test]
fn empty_or_invalid_device_filters_are_rejected_before_submission() {
    let handle = queued_handle(Timing::default());
    for dids in [vec![], vec!["valid".into(), "invalid did".into()]] {
        let request = handle.get_devices_filtered(&dids, |result| {
            assert_eq!(result, Err(CentralError::InvalidInput));
        });
        assert_eq!(block_on(request), Err(CentralError::InvalidInput));
        assert!(handle.shared.queue.borrow().is_empty());
    }
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
                assert_eq!(
                    devices[0].access.as_ref().unwrap().name.as_deref(),
                    Some("插座 一")
                );
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
