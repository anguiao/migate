use super::*;

#[test]
fn business_acceptance_precedes_pubcomp_and_callbacks_preserve_notification_order() {
    let order = Rc::new(RefCell::new(Vec::new()));
    let events = order.clone();
    let writes = order.clone();
    let accepted = Rc::new(Cell::new(false));
    let server_accepted = accepted.clone();
    block_on(pair(
        move |mut broker| async move {
            broker.setup().await;
            broker.subscription().await;
            let Packet::Publish(write) = broker.next().await else {
                panic!("write PUBLISH expected")
            };
            assert_eq!(write.qos, QoS::ExactlyOnce);
            let request = wire::decode_mips(&write.payload).unwrap();
            assert_eq!(request.payload["rpc"]["method"], "set_properties");
            broker.send(Packet::PubRec(PubRec::new(write.pkid))).await;
            broker
                .reply(&write, json!({"result":[{"did":"target","code":1}]}), 10)
                .await;
            let mut notification = Publish::new(
                format!("{}/appMsg/notify/iot/target/property/2.1", broker.client_id),
                QoS::ExactlyOnce,
                wire::encode_mips(
                    999,
                    None,
                    &json!({"did":"target","siid":2,"piid":1,"value":false}),
                ),
            );
            notification.pkid = 11;
            broker.send(Packet::Publish(notification)).await;
            broker
                .wait_for(|packet| matches!(packet,Packet::PubRec(ack) if ack.pkid==11))
                .await;
            broker.send(Packet::PubRel(PubRel::new(11))).await;
            broker
                .wait_for(|packet| matches!(packet,Packet::PubComp(ack) if ack.pkid==11))
                .await;
            broker
                .wait_for(|packet| matches!(packet,Packet::PubRel(ack) if ack.pkid==write.pkid))
                .await;
            async_io::Timer::after(Duration::from_millis(40)).await;
            assert!(
                server_accepted.get(),
                "business acceptance must precede outgoing PUBCOMP"
            );
            broker.send(Packet::PubComp(PubComp::new(write.pkid))).await;
            async_io::Timer::after(Duration::from_millis(20)).await;
        },
        |handle, driver| async move {
            let drive = driver.run(move |event| {
                if let CentralEvent::PropertyValue { value, .. } = event {
                    events.borrow_mut().push(value.as_bool().unwrap());
                }
            });
            let work = async {
                handle
                    .subscribe_property("target", PropertyBinding { siid: 2, piid: 1 })
                    .await
                    .unwrap();
                handle
                    .set_property(
                        "target",
                        PropertyBinding { siid: 2, piid: 1 },
                        true,
                        move |outcome| {
                            outcome.result.unwrap();
                            accepted.set(true);
                            writes.borrow_mut().push(true);
                        },
                    )
                    .await
                    .result
                    .unwrap();
                async_io::Timer::after(Duration::from_millis(55)).await;
                handle.close();
            };
            let (result, ()) = future::zip(drive, work).await;
            result.unwrap();
        },
    ));
    assert_eq!(*order.borrow(), [true, false]);
}

#[test]
fn coalesced_read_and_access_callbacks_run_before_later_notifications() {
    let order = Rc::new(RefCell::new(Vec::new()));
    let observations = order.clone();
    let accesses = order.clone();
    let events = order.clone();
    block_on(pair(
        |mut broker| async move {
            broker.setup().await;
            broker.subscription().await;
            let Packet::Publish(list) = broker.next().await else {
                panic!("list expected")
            };
            let request = wire::decode_mips(&list.payload).unwrap();
            assert_eq!(list.topic, "master/proxy/getDevList");
            assert_eq!(
                request.payload,
                json!({"filter":{"did":["target"]},"info":["name","model","urn","online","specV2Access","pushAvailable"]})
            );
            broker.acknowledge(&list).await;
            let list_reply=broker.business(&list,json!({"devList":{"unrequested":{"name":"private name"},"target":{"model":"cuco.plug.v3","urn":"urn:miot-spec-v2:device:outlet:0000A002:cuco-v3:1","online":false,"specV2Access":true,"pushAvailable":true}}}));
            broker
                .batch(vec![
                    list_reply,
                    Packet::Publish(Publish::new(
                        format!("{}/appMsg/devListChange", broker.client_id),
                        QoS::AtMostOnce,
                        wire::encode_mips(77, None, &json!({"devList":["target"]})),
                    )),
                ])
                .await;
            let Packet::Publish(read) = broker
                .wait_for(|packet| matches!(packet, Packet::Publish(_)))
                .await
            else {
                unreachable!()
            };
            assert_eq!(read.topic, "master/proxy/get");
            broker.acknowledge(&read).await;
            let reply = broker.business(
                &read,
                json!({"did":"target","siid":2,"piid":1,"value":false}),
            );
            let notify = Packet::Publish(Publish::new(
                format!("{}/appMsg/notify/iot/target/property/2.1", broker.client_id),
                QoS::AtMostOnce,
                wire::encode_mips(
                    78,
                    None,
                    &json!({"did":"target","siid":2,"piid":1,"value":true}),
                ),
            ));
            broker.batch(vec![reply, notify]).await;
            async_io::Timer::after(Duration::from_millis(20)).await;
        },
        |handle, driver| async move {
            let work = async {
                handle
                    .subscribe_property("target", PropertyBinding { siid: 2, piid: 1 })
                    .await
                    .unwrap();
                let entries = handle
                    .get_devices_filtered(&["target".into()], move |outcome| {
                        let entries = outcome.result.unwrap();
                        assert_eq!(entries.len(), 1);
                        assert!(!entries[0].access.as_ref().unwrap().online);
                        accesses.borrow_mut().push("offline");
                    })
                    .await
                    .result
                    .unwrap();
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].access.as_ref().unwrap().model, "cuco.plug.v3");
                assert!(
                    !handle
                        .read_property_value(
                            "target",
                            PropertyBinding { siid: 2, piid: 1 },
                            move |outcome| {
                                assert_eq!(
                                    outcome.result.unwrap().as_bool().unwrap(),
                                    json!(false)
                                );
                                observations.borrow_mut().push("read false");
                            }
                        )
                        .await
                        .result
                        .unwrap()
                        .as_bool()
                        .unwrap()
                );
                handle.close();
            };
            let (result, ()) = future::zip(
                driver.run(move |event| {
                    events.borrow_mut().push(match event {
                        CentralEvent::PropertyValue {
                            value: serde_json::Value::Bool(true),
                            ..
                        } => "notify true",
                        CentralEvent::DeviceListChanged { dids } => {
                            assert_eq!(dids, ["target"]);
                            "list changed"
                        }
                        _ => panic!("unexpected event"),
                    })
                }),
                work,
            )
            .await;
            result.unwrap();
        },
    ));
    assert_eq!(
        *order.borrow(),
        ["offline", "list changed", "read false", "notify true"]
    );
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
                    move |outcome| {
                        outcome.result.unwrap();
                        callbacks.borrow_mut().push((did, siid));
                    },
                ));
            }
            let work = async {
                for write in writes {
                    write.await.result.unwrap();
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
fn result_callbacks_can_submit_requests_after_waiters_are_dropped() {
    block_on(pair(
        |mut broker| async move {
            broker.setup().await;
            for value in [true, false] {
                let Packet::Publish(read) = broker.next().await else {
                    panic!("read expected")
                };
                broker.acknowledge(&read).await;
                let id = wire::decode_mips(&read.payload).unwrap().id;
                broker
                    .send(Packet::Publish(Publish::new(
                        format!("{}/reply", broker.client_id),
                        QoS::AtMostOnce,
                        wire::encode_mips(id, None, &json!({"value":value})),
                    )))
                    .await;
            }
            assert!(broker.next_optional().await.is_none());
        },
        |handle, driver| async move {
            let next = handle.clone();
            drop(handle.read_property_value(
                "target",
                PropertyBinding { siid: 2, piid: 1 },
                move |outcome| {
                    assert_eq!(outcome.result, Ok(json!(true)));
                    let close = next.clone();
                    drop(next.read_property_value(
                        "target",
                        PropertyBinding { siid: 2, piid: 1 },
                        move |outcome| {
                            assert_eq!(outcome.result, Ok(json!(false)));
                            close.close();
                        },
                    ));
                },
            ));
            driver.run(|_| {}).await.unwrap();
        },
    ));
}
