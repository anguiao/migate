use super::*;
use crate::xiaomi::central::{tests::TlsFixture, wire};
use async_io::Async;
use futures_lite::{
    future::{self, block_on},
    io::{AsyncReadExt, AsyncWriteExt},
};
use futures_rustls::{TlsAcceptor, server::TlsStream};
use std::{
    cell::RefCell,
    net::{TcpListener, TcpStream},
    rc::Rc,
};

struct Broker {
    stream: TlsStream<Async<TcpStream>>,
    frames: wire::Frames,
    client_id: String,
    deferred: VecDeque<Packet>,
}

impl Broker {
    async fn next(&mut self) -> Packet {
        if let Some(packet) = self.deferred.pop_front() {
            return packet;
        }
        self.next_wire().await
    }
    async fn wait_for(&mut self, matches: impl Fn(&Packet) -> bool) -> Packet {
        if let Some(index) = self.deferred.iter().position(&matches) {
            return self.deferred.remove(index).unwrap();
        }
        loop {
            let packet = self.next_wire().await;
            if matches(&packet) {
                return packet;
            }
            self.deferred.push_back(packet);
        }
    }
    async fn next_wire(&mut self) -> Packet {
        loop {
            if let Some(packet) = self.frames.next_packet().unwrap() {
                return packet;
            }
            let mut bytes = [0; 4096];
            let len = self.stream.read(&mut bytes).await.unwrap();
            assert!(len > 0, "client closed before the expected packet");
            self.frames.push(&bytes[..len]).unwrap();
        }
    }
    async fn send(&mut self, packet: Packet) {
        self.stream
            .write_all(&encode_packet(&packet).unwrap())
            .await
            .unwrap();
        self.stream.flush().await.unwrap();
    }
    async fn setup(&mut self) {
        let Packet::Connect(connect) = self.next().await else {
            panic!("CONNECT expected")
        };
        assert!(connect.clean_session);
        assert_eq!(connect.protocol, mqttbytes::Protocol::V5);
        assert!(connect.login.is_none());
        self.client_id = connect.client_id;
        self.send(Packet::ConnAck(ConnAck::new(
            ConnectReturnCode::Success,
            false,
        )))
        .await;
        let Packet::Subscribe(subscribe) = self.next().await else {
            panic!("SUBSCRIBE expected")
        };
        assert_eq!(
            subscribe
                .filters
                .iter()
                .map(|filter| filter.path.as_str())
                .collect::<Vec<_>>(),
            [
                format!("{}/#", self.client_id),
                "master/appMsg/devListChange".into()
            ]
        );
        self.send(Packet::SubAck(SubAck::new(
            subscribe.pkid,
            vec![SubscribeReasonCode::QoS2; 2],
        )))
        .await;
    }
    async fn subscription(&mut self) {
        let Packet::Subscribe(subscribe) = self.next().await else {
            panic!("property SUBSCRIBE expected")
        };
        assert_eq!(
            subscribe.filters[0].path,
            "master/appMsg/notify/iot/target/property/2.1"
        );
        self.send(Packet::SubAck(SubAck::new(
            subscribe.pkid,
            vec![SubscribeReasonCode::QoS2],
        )))
        .await;
    }
    async fn reply(&mut self, outgoing: &Publish, value: Value, pkid: u16) {
        let message = wire::decode_mips(&outgoing.payload).unwrap();
        let mut reply = Publish::new(
            format!("{}/reply", self.client_id),
            QoS::ExactlyOnce,
            wire::encode_mips(message.id, None, &value),
        );
        reply.pkid = pkid;
        self.send(Packet::Publish(reply)).await;
        self.wait_for(|packet|matches!(packet,Packet::PubRec(ack) if ack.pkid==pkid && ack.reason==PubRecReason::Success)).await;
        self.send(Packet::PubRel(PubRel::new(pkid))).await;
        self.wait_for(|packet|matches!(packet,Packet::PubComp(ack) if ack.pkid==pkid && ack.reason==PubCompReason::Success)).await;
    }
}

async fn pair<S, C, SF, CF>(server: S, client: C)
where
    S: FnOnce(Broker) -> SF,
    C: FnOnce(SessionHandle, SessionDriver) -> CF,
    SF: std::future::Future<Output = ()>,
    CF: std::future::Future<Output = ()>,
{
    pair_io(
        server,
        client,
        Timing {
            request: Duration::from_millis(200),
            keepalive: Some(Duration::from_secs(1)),
        },
        |stream| stream,
    )
    .await;
}

async fn pair_io<S, C, SF, CF, W>(server: S, client: C, timing: Timing, wrap: W)
where
    S: FnOnce(Broker) -> SF,
    C: FnOnce(SessionHandle, SessionDriver) -> CF,
    SF: Future<Output = ()>,
    CF: Future<Output = ()>,
    W: FnOnce(Box<dyn Transport>) -> Box<dyn Transport>,
{
    let fixture = TlsFixture::new(false);
    pair_fixture(&fixture, server, client, timing, wrap).await;
}
async fn pair_fixture<S, C, SF, CF, W>(
    fixture: &TlsFixture,
    server: S,
    client: C,
    timing: Timing,
    wrap: W,
) where
    S: FnOnce(Broker) -> SF,
    C: FnOnce(SessionHandle, SessionDriver) -> CF,
    SF: Future<Output = ()>,
    CF: Future<Output = ()>,
    W: FnOnce(Box<dyn Transport>) -> Box<dyn Transport>,
{
    let listener = Async::<TcpListener>::bind(([127, 0, 0, 1], 0)).unwrap();
    let address = listener.get_ref().local_addr().unwrap();
    let acceptor = TlsAcceptor::from(fixture.server_config());
    let accept = async {
        let (socket, _) = listener.accept().await.unwrap();
        let stream = acceptor.accept(socket).await.unwrap();
        server(Broker {
            stream,
            frames: wire::Frames::default(),
            client_id: String::new(),
            deferred: VecDeque::new(),
        })
        .await;
    };
    let connect = async {
        let stream = tls::connect(address, &fixture.credentials, &fixture.ca_pem)
            .await
            .unwrap();
        let (handle, driver) = start(
            wrap(Box::new(stream)),
            &fixture.credentials.virtual_did,
            timing,
        )
        .await
        .unwrap();
        client(handle, driver).await;
    };
    future::race(
        async {
            future::zip(accept, connect).await;
        },
        async {
            async_io::Timer::after(Duration::from_secs(3)).await;
            panic!("isolated broker fixture timed out")
        },
    )
    .await;
}

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
                if let CentralEvent::Property { value, .. } = event {
                    events.borrow_mut().push(value);
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
                        Duration::from_millis(150),
                        move |result| {
                            result.unwrap();
                            accepted.set(true);
                            writes.borrow_mut().push(true);
                        },
                    )
                    .await
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
fn generated_connack_roundtrips() {
    let bytes = encode_packet(&Packet::ConnAck(ConnAck::new(
        ConnectReturnCode::Success,
        false,
    )))
    .unwrap();
    let mut frames = wire::Frames::default();
    frames.push(&bytes).unwrap();
    assert!(matches!(
        frames.next_packet().unwrap(),
        Some(Packet::ConnAck(_))
    ));
}

#[test]
fn list_metadata_requires_explicit_access_flags() {
    let kind = RequestKind::List {
        did: "target".into(),
    };
    for info in [
        json!({"model":"cuco.plug.v3","urn":"outlet"}),
        json!({"model":"cuco.plug.v3","urn":"outlet","online":false,"specV2Access":true,"pushAvailable":0}),
    ] {
        assert!(matches!(
            parse_response(&kind, &json!({"devList":{"target":info}})),
            Err(CentralError::Protocol)
        ));
    }
}

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
                        .read_property("target", PropertyBinding { siid: 2, piid: 1 }, |result| {
                            result.unwrap();
                        })
                        .await
                        .unwrap()
                );
                async_io::Timer::after(Duration::from_millis(10)).await;
                handle.close();
            };
            let (result, ()) = future::zip(
                driver.run(move |event| {
                    if let CentralEvent::Property { value, .. } = event {
                        events.borrow_mut().push(value)
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

impl Broker {
    async fn acknowledge(&mut self, publish: &Publish) {
        self.send(Packet::PubRec(PubRec::new(publish.pkid))).await;
        self.wait_for(|packet| matches!(packet,Packet::PubRel(ack) if ack.pkid==publish.pkid))
            .await;
        self.send(Packet::PubComp(PubComp::new(publish.pkid))).await;
    }
    fn business(&self, publish: &Publish, value: Value) -> Packet {
        Packet::Publish(Publish::new(
            format!("{}/reply", self.client_id),
            QoS::AtMostOnce,
            wire::encode_mips(
                wire::decode_mips(&publish.payload).unwrap().id,
                None,
                &value,
            ),
        ))
    }
    async fn batch(&mut self, packets: Vec<Packet>) {
        let bytes = packets
            .iter()
            .flat_map(|packet| encode_packet(packet).unwrap())
            .collect::<Vec<_>>();
        self.stream.write_all(&bytes).await.unwrap();
        self.stream.flush().await.unwrap();
    }
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
                json!({"filter":{"did":["target"]},"info":["model","urn","online","specV2Access","pushAvailable"]})
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
                let access = handle
                    .get_device_list("target", move |result| {
                        assert!(!result.unwrap().unwrap().online);
                        accesses.borrow_mut().push("offline");
                    })
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(access.model, "cuco.plug.v3");
                assert!(
                    !handle
                        .read_property(
                            "target",
                            PropertyBinding { siid: 2, piid: 1 },
                            move |result| {
                                assert!(!result.unwrap());
                                observations.borrow_mut().push("read false");
                            }
                        )
                        .await
                        .unwrap()
                );
                handle.close();
            };
            let (result, ()) = future::zip(
                driver.run(move |event| {
                    events.borrow_mut().push(match event {
                        CentralEvent::Property { value: true, .. } => "notify true",
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
                            move |result| callback.borrow_mut().push(result)
                        )
                        .await,
                    Err(CentralError::Timeout)
                );
                assert_eq!(
                    handle
                        .read_property(
                            "target",
                            PropertyBinding { siid: 2, piid: 1 },
                            move |result| results.borrow_mut().push(result)
                        )
                        .await,
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
                    if let CentralEvent::Property { value, .. } = event {
                        observed.borrow_mut().push(value)
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
                                Duration::from_millis(100),
                                move |result| results.borrow_mut().push(result)
                            )
                            .await,
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
fn disconnect_and_access_denial_finish_every_pending_callback() {
    for denied in [false, true] {
        let expected = if denied {
            CentralError::Rejected(135)
        } else {
            CentralError::Disconnected
        };
        let callbacks = Rc::new(RefCell::new(Vec::new()));
        let first = callbacks.clone();
        let second = callbacks.clone();
        let error = expected.clone();
        block_on(pair(
            move |mut broker| async move {
                broker.setup().await;
                broker
                    .wait_for(|packet| matches!(packet, Packet::Publish(_)))
                    .await;
                if denied {
                    let mut packet = Disconnect::new();
                    packet.reason_code = DisconnectReasonCode::NotAuthorized;
                    broker.send(Packet::Disconnect(packet)).await;
                } else {
                    broker.send(Packet::Disconnect(Disconnect::new())).await;
                }
                async_io::Timer::after(Duration::from_millis(20)).await;
            },
            move |handle, driver| async move {
                let read = handle.read_property(
                    "target",
                    PropertyBinding { siid: 2, piid: 1 },
                    move |result| first.borrow_mut().push(result.map(|_| ())),
                );
                let write = handle.set_property(
                    "target",
                    PropertyBinding { siid: 2, piid: 1 },
                    true,
                    Duration::from_millis(100),
                    move |result| second.borrow_mut().push(result),
                );
                let (result, (read, write)) =
                    future::zip(driver.run(|_| {}), future::zip(read, write)).await;
                assert_eq!(result, Err(error.clone()));
                assert_eq!(read, Err(error.clone()));
                assert_eq!(write, Err(error));
            },
        ));
        assert_eq!(*callbacks.borrow(), [Err(expected.clone()), Err(expected)]);
    }
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
                                    |result| {
                                        result.unwrap();
                                    }
                                )
                                .await
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

struct ShortIo {
    inner: Box<dyn Transport>,
    mode: Rc<Cell<u8>>,
    written: Rc<RefCell<Vec<u8>>>,
}
impl AsyncRead for ShortIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bytes: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_read(cx, bytes)
    }
}
impl AsyncWrite for ShortIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let mode = self.mode.get();
        if mode == 3 {
            return Poll::Pending;
        }
        let bytes = if mode == 0 {
            bytes
        } else {
            &bytes[..bytes.len().min(3)]
        };
        let result = Pin::new(&mut self.inner).poll_write(cx, bytes);
        if let Poll::Ready(Ok(len)) = result {
            if mode > 0 {
                self.written.borrow_mut().extend_from_slice(&bytes[..len]);
            }
            if mode == 2 {
                self.mode.set(3);
            }
        }
        result
    }
    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_close(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_close(cx)
    }
}

#[test]
fn short_writes_preserve_offsets_and_partial_timeout_closes_without_replay() {
    for stalls in [false, true] {
        let mode = Rc::new(Cell::new(0));
        let control = mode.clone();
        let written = Rc::new(RefCell::new(Vec::new()));
        let bytes = written.clone();
        let callbacks = Rc::new(RefCell::new(Vec::new()));
        let first = callbacks.clone();
        let second = callbacks.clone();
        block_on(pair_io(
            move |mut broker| async move {
                broker.setup().await;
                broker.subscription().await;
                if stalls {
                    let mut prefix = [0; 3];
                    broker.stream.read_exact(&mut prefix).await.unwrap();
                    assert_eq!(prefix[0], 0x34);
                    let mut extra = [0; 4096];
                    assert!(!matches!(broker.stream.read(&mut extra).await,Ok(len) if len>0));
                } else {
                    for pkid in [60, 61] {
                        let Packet::Publish(write) = broker.next().await else {
                            panic!("whole write expected")
                        };
                        broker.acknowledge(&write).await;
                        broker
                            .reply(&write, json!({"result":[{"did":"target","code":0}]}), pkid)
                            .await;
                    }
                    async_io::Timer::after(Duration::from_millis(20)).await;
                }
            },
            move |handle, driver| async move {
                let work = async {
                    handle
                        .subscribe_property("target", PropertyBinding { siid: 2, piid: 1 })
                        .await
                        .unwrap();
                    control.set(if stalls { 2 } else { 1 });
                    let first_write = handle.set_property(
                        "target",
                        PropertyBinding { siid: 2, piid: 1 },
                        true,
                        Duration::from_millis(50),
                        move |result| first.borrow_mut().push(result),
                    );
                    let second_write = handle.set_property(
                        "target",
                        PropertyBinding { siid: 2, piid: 1 },
                        false,
                        Duration::from_millis(100),
                        move |result| second.borrow_mut().push(result),
                    );
                    let (first, second) = future::zip(first_write, second_write).await;
                    if stalls {
                        assert_eq!(first, Err(CentralError::Timeout));
                        assert_eq!(second, Err(CentralError::Timeout));
                    } else {
                        first.unwrap();
                        second.unwrap();
                        handle.close();
                    }
                };
                let (result, ()) = future::zip(driver.run(|_| {}), work).await;
                if stalls {
                    assert_eq!(result, Err(CentralError::Timeout));
                } else {
                    result.unwrap();
                }
            },
            Timing {
                request: Duration::from_millis(200),
                keepalive: None,
            },
            move |inner| {
                Box::new(ShortIo {
                    inner,
                    mode,
                    written: bytes,
                })
            },
        ));
        if stalls {
            assert_eq!(written.borrow().len(), 3);
            assert_eq!(
                *callbacks.borrow(),
                [Err(CentralError::Timeout), Err(CentralError::Timeout)]
            );
        } else {
            let mut frames = wire::Frames::default();
            frames.push(&written.borrow()).unwrap();
            let mut publishes = 0;
            while let Some(packet) = frames.next_packet().unwrap() {
                if matches!(packet, Packet::Publish(_)) {
                    publishes += 1;
                }
            }
            assert_eq!(publishes, 2);
            assert_eq!(*callbacks.borrow(), [Ok(()), Ok(())]);
        }
    }
}

#[test]
fn queued_deadlines_drop_callbacks_and_invalid_budgets_finish_explicitly() {
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
            let expired = handle.set_property(
                "target",
                PropertyBinding { siid: 2, piid: 1 },
                true,
                Duration::from_millis(10),
                move |result| callback.borrow_mut().push(result),
            );
            let work = async {
                assert_eq!(expired.await, Err(CentralError::Timeout));
                assert_eq!(
                    handle
                        .set_property(
                            "target",
                            PropertyBinding { siid: 2, piid: 1 },
                            true,
                            Duration::MAX,
                            |result| assert_eq!(result, Err(CentralError::InvalidInput))
                        )
                        .await,
                    Err(CentralError::InvalidInput)
                );
                assert!(
                    !handle
                        .read_property("target", PropertyBinding { siid: 2, piid: 1 }, |result| {
                            result.unwrap();
                        })
                        .await
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

struct GateIo {
    inner: Box<dyn Transport>,
    opens: Rc<Cell<Option<Instant>>>,
    written: Rc<Cell<usize>>,
}
impl AsyncRead for GateIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bytes: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_read(cx, bytes)
    }
}
impl AsyncWrite for GateIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if self
            .opens
            .get()
            .is_some_and(|instant| Instant::now() < instant)
        {
            return Poll::Pending;
        }
        let result = Pin::new(&mut self.inner).poll_write(cx, bytes);
        if self.opens.get().is_some()
            && let Poll::Ready(Ok(count)) = result
        {
            self.written.set(self.written.get() + count);
        }
        result
    }
    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_close(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_close(cx)
    }
}

#[test]
fn write_gate_opening_at_deadline_does_not_send_expired_request() {
    let opens = Rc::new(Cell::new(None));
    let gate = opens.clone();
    let written = Rc::new(Cell::new(0));
    let count = written.clone();
    block_on(pair_io(
        |mut broker| async move {
            broker.setup().await;
            broker.subscription().await;
            let mut bytes = [0; 4096];
            assert!(!matches!(broker.stream.read(&mut bytes).await,Ok(len) if len>0));
        },
        move |handle, driver| async move {
            let work = async {
                handle
                    .subscribe_property("target", PropertyBinding { siid: 2, piid: 1 })
                    .await
                    .unwrap();
                gate.set(Some(Instant::now() + Duration::from_millis(30)));
                assert_eq!(
                    handle
                        .set_property(
                            "target",
                            PropertyBinding { siid: 2, piid: 1 },
                            true,
                            Duration::from_millis(30),
                            |result| assert_eq!(result, Err(CentralError::Timeout))
                        )
                        .await,
                    Err(CentralError::Timeout)
                );
                handle.close();
            };
            let (result, ()) = future::zip(driver.run(|_| {}), work).await;
            result.unwrap();
        },
        Timing {
            request: Duration::from_millis(200),
            keepalive: None,
        },
        move |inner| {
            Box::new(GateIo {
                inner,
                opens,
                written: count,
            })
        },
    ));
    assert_eq!(written.get(), 0);
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

#[test]
fn dropping_unpolled_driver_completes_queued_callback_and_closes_handle() {
    let callbacks = Rc::new(RefCell::new(Vec::new()));
    let results = callbacks.clone();
    block_on(pair(
        |mut broker| async move {
            assert!(matches!(broker.next().await, Packet::Connect(_)));
            broker
                .send(Packet::ConnAck(ConnAck::new(
                    ConnectReturnCode::Success,
                    false,
                )))
                .await;
            let mut bytes = [0; 4096];
            assert!(!matches!(broker.stream.read(&mut bytes).await,Ok(len) if len>0));
        },
        move |handle, driver| async move {
            let request = handle.set_property(
                "target",
                PropertyBinding { siid: 2, piid: 1 },
                true,
                Duration::from_secs(1),
                move |result| results.borrow_mut().push(result),
            );
            drop(driver);
            assert_eq!(request.await, Err(CentralError::Cancelled));
            assert_eq!(
                handle
                    .read_property(
                        "target",
                        PropertyBinding { siid: 2, piid: 1 },
                        |result| assert_eq!(result, Err(CentralError::Cancelled))
                    )
                    .await,
                Err(CentralError::Cancelled)
            );
        },
    ));
    assert_eq!(*callbacks.borrow(), [Err(CentralError::Cancelled)]);
}

#[test]
fn reconnect_has_new_generation_and_never_completes_from_old_session_id() {
    block_on(async {
        let fixture = TlsFixture::new(false);
        let old = Rc::new(Cell::new(0));
        let old_id = old.clone();
        let generations = Rc::new(RefCell::new(Vec::new()));
        for session in 0..2 {
            let server_old = old_id.clone();
            let client_generations = generations.clone();
            pair_fixture(
                &fixture,
                move |mut broker| async move {
                    broker.setup().await;
                    let Packet::Publish(read) = broker.next().await else {
                        panic!("read expected")
                    };
                    let id = wire::decode_mips(&read.payload).unwrap().id;
                    broker.acknowledge(&read).await;
                    if session == 0 {
                        server_old.set(id);
                    } else {
                        assert_ne!(id, server_old.get());
                        broker
                            .send(Packet::Publish(Publish::new(
                                format!("{}/reply", broker.client_id),
                                QoS::AtMostOnce,
                                wire::encode_mips(server_old.get(), None, &json!({"value":true})),
                            )))
                            .await;
                    }
                    broker.reply(&read, json!({"value":false}), 90).await;
                    async_io::Timer::after(Duration::from_millis(20)).await;
                },
                move |handle, driver| async move {
                    client_generations.borrow_mut().push(handle.generation());
                    let work = async {
                        assert!(
                            !handle
                                .read_property(
                                    "target",
                                    PropertyBinding { siid: 2, piid: 1 },
                                    |result| assert_eq!(result, Ok(false))
                                )
                                .await
                                .unwrap()
                        );
                        handle.close();
                    };
                    let (result, ()) = future::zip(driver.run(|_| {}), work).await;
                    result.unwrap();
                },
                Timing {
                    request: Duration::from_millis(200),
                    keepalive: None,
                },
                |stream| stream,
            )
            .await;
        }
        assert_ne!(generations.borrow()[0], generations.borrow()[1]);
    });
}

#[test]
fn connack_rejection_and_persisted_session_are_rejected_over_tls() {
    for persisted in [false, true] {
        block_on(async {
            let fixture = TlsFixture::new(false);
            let listener = Async::<TcpListener>::bind(([127, 0, 0, 1], 0)).unwrap();
            let address = listener.get_ref().local_addr().unwrap();
            let acceptor = TlsAcceptor::from(fixture.server_config());
            let server = async {
                let (socket, _) = listener.accept().await.unwrap();
                let stream = acceptor.accept(socket).await.unwrap();
                let mut broker = Broker {
                    stream,
                    frames: wire::Frames::default(),
                    client_id: String::new(),
                    deferred: VecDeque::new(),
                };
                assert!(matches!(broker.next().await, Packet::Connect(_)));
                broker
                    .send(Packet::ConnAck(ConnAck::new(
                        if persisted {
                            ConnectReturnCode::Success
                        } else {
                            ConnectReturnCode::NotAuthorized
                        },
                        persisted,
                    )))
                    .await;
            };
            let client = async {
                let stream = tls::connect(address, &fixture.credentials, &fixture.ca_pem)
                    .await
                    .unwrap();
                let error = match start(
                    Box::new(stream),
                    &fixture.credentials.virtual_did,
                    Timing::default(),
                )
                .await
                {
                    Ok(_) => panic!("invalid CONNACK accepted"),
                    Err(error) => error,
                };
                assert_eq!(
                    error,
                    if persisted {
                        CentralError::Protocol
                    } else {
                        CentralError::Rejected(135)
                    }
                );
            };
            future::zip(server, client).await;
        });
    }
}
