use super::{
    NotificationDriver, NotificationError, NotificationEvent, NotificationHandle, Timing,
    connection::start,
};
use crate::{
    storage::{TokenSet, XiaomiRecord},
    xiaomi::{capability::PropertyBinding, cloud::CLIENT_ID, mqtt},
};
use async_io::Async;
use futures_lite::{
    future,
    io::{AsyncReadExt, AsyncWriteExt},
};
use mqttbytes::{QoS, v5::*};
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    future::Future,
    net::{TcpListener, TcpStream},
    rc::Rc,
    time::Duration,
};

const POWER: PropertyBinding = PropertyBinding { siid: 2, piid: 1 };

fn credentials() -> XiaomiRecord {
    XiaomiRecord {
        uid: "account".into(),
        region: "cn".into(),
        oauth_client_uuid: "550e8400-e29b-41d4-a716-446655440000".into(),
        redirect_uri: "unused".into(),
        tokens: TokenSet {
            access_token: "access-secret".into(),
            refresh_token: "refresh-secret".into(),
            expires_at: 1,
            refresh_at: 1,
        },
        virtual_did: "different-local-identity".into(),
        private_key_pem: "unused".into(),
        certificate_pem: "unused".into(),
    }
}

struct Broker {
    stream: Async<TcpStream>,
    frames: mqtt::Frames,
}

impl Broker {
    async fn next(&mut self) -> Packet {
        loop {
            if let Some(packet) = self.frames.next_packet().unwrap() {
                return packet;
            }
            let mut bytes = [0; 4096];
            let len = self.stream.read(&mut bytes).await.unwrap();
            assert_ne!(len, 0, "client closed before expected packet");
            self.frames.push(&bytes[..len]).unwrap();
        }
    }
    async fn send(&mut self, packet: Packet) {
        self.stream
            .write_all(&mqtt::encode_packet(&packet).unwrap())
            .await
            .unwrap();
        self.stream.flush().await.unwrap();
    }
    async fn setup(&mut self) {
        let Packet::Connect(connect) = self.next().await else {
            panic!("CONNECT expected");
        };
        assert_eq!(connect.client_id, "ha.550e8400e29b41d4a716446655440000");
        assert!(connect.clean_session);
        assert_eq!(connect.protocol, mqttbytes::Protocol::V5);
        let login = connect.login.unwrap();
        assert_eq!(login.username, CLIENT_ID.to_string());
        assert_eq!(login.password, "access-secret");
        self.send(Packet::ConnAck(ConnAck::new(
            ConnectReturnCode::Success,
            false,
        )))
        .await;
    }
    async fn subscribe(&mut self, expected: &str) -> u16 {
        let Packet::Subscribe(sub) = self.next().await else {
            panic!("SUBSCRIBE expected");
        };
        assert_eq!(sub.filters.len(), 1);
        assert_eq!(sub.filters[0].path, expected);
        assert_eq!(sub.filters[0].qos, QoS::ExactlyOnce);
        sub.pkid
    }
    async fn accept(&mut self, pkid: u16) {
        self.send(Packet::SubAck(SubAck {
            pkid,
            return_codes: vec![SubscribeReasonCode::QoS2],
            properties: None,
        }))
        .await;
    }
    async fn property(&mut self, value: Value) {
        self.send(Packet::Publish(Publish::new(
            "device/target/up/properties_changed/2/1",
            QoS::AtMostOnce,
            json!({"params":{"siid":2,"piid":1,"value":value}}).to_string(),
        )))
        .await;
    }
}

async fn exercise<S: Future<Output = ()>, C: Future<Output = ()>>(
    timing: Timing,
    server: impl FnOnce(Broker) -> S,
    client: impl FnOnce(NotificationHandle, NotificationDriver) -> C,
) {
    let listener = Async::new(TcpListener::bind("127.0.0.1:0").unwrap()).unwrap();
    let address = listener.get_ref().local_addr().unwrap();
    future::race(
        async {
            future::zip(
                async {
                    let (stream, _) = listener.accept().await.unwrap();
                    server(Broker {
                        stream,
                        frames: mqtt::Frames::default(),
                    })
                    .await;
                },
                async {
                    let (handle, driver) =
                        crate::xiaomi::cloud::notification_test_support::connect(
                            address,
                            &credentials(),
                            timing.request,
                            timing.keepalive,
                        )
                        .await
                        .unwrap();
                    client(handle, driver).await;
                },
            )
            .await;
        },
        async {
            async_io::Timer::after(Duration::from_secs(3)).await;
            panic!("notification test stalled");
        },
    )
    .await;
}

fn timing() -> Timing {
    Timing {
        request: Duration::from_millis(100),
        keepalive: None,
    }
}

#[test]
fn notification_connect_suback_and_messages_follow_wire_order() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let collected = events.clone();
    let (finished, done) = flume::bounded(1);
    future::block_on(exercise(
        Timing::default(),
        move |mut broker| async move {
            let Packet::Connect(connect) = broker.next().await else {
                panic!();
            };
            assert_eq!(connect.keep_alive, 60);
            assert_eq!(connect.client_id, "ha.550e8400e29b41d4a716446655440000");
            assert_eq!(connect.login.unwrap().username, CLIENT_ID.to_string());
            assert_eq!(connect.properties.unwrap().session_expiry_interval, Some(0));
            broker
                .send(Packet::ConnAck(ConnAck::new(
                    ConnectReturnCode::Success,
                    false,
                )))
                .await;
            let pkid = broker
                .subscribe("device/target/up/properties_changed/2/1")
                .await;
            broker.property(json!(false)).await;
            broker.accept(pkid).await;
            let pkid = broker.subscribe("device/target/state/#").await;
            broker.accept(pkid).await;
            broker.property(json!(true)).await;
            broker
                .send(Packet::Publish(Publish::new(
                    "device/target/state/online",
                    QoS::AtMostOnce,
                    json!({"device_id":"target","event":"online","uid":"account"}).to_string(),
                )))
                .await;
            done.recv_async().await.unwrap();
        },
        move |handle, driver| async move {
            let stop = handle.clone();
            let work = async {
                handle.subscribe_property("target", POWER).await.unwrap();
                handle.subscribe_online("target").await.unwrap();
            };
            let drive = driver.run(move |event| {
                collected.borrow_mut().push(event);
                if collected.borrow().len() == 2 {
                    finished.send(()).unwrap();
                    stop.close();
                }
            });
            let (result, ()) = future::zip(drive, work).await;
            result.unwrap();
        },
    ));
    assert_eq!(
        *events.borrow(),
        [
            NotificationEvent::Property {
                did: "target".into(),
                binding: POWER,
                value: true
            },
            NotificationEvent::Online {
                did: "target".into(),
                online: true
            }
        ]
    );
}

#[test]
fn malformed_notification_payloads_are_isolated_from_valid_notifications() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let collected = events.clone();
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
            for payload in [
                "not JSON".to_owned(),
                json!({"params":{"siid":2,"piid":1,"value":1}}).to_string(),
                json!({"params":{"siid":3,"piid":1,"value":true}}).to_string(),
                json!({"params":{"did":"foreign","siid":2,"piid":1,"value":true}}).to_string(),
                json!({"params":{"siid":"2","piid":1,"value":true}}).to_string(),
                json!({"params":{"siid":2,"piid":1}}).to_string(),
                json!({"did":"foreign","params":{"siid":2,"piid":1,"value":true}}).to_string(),
            ] {
                broker
                    .send(Packet::Publish(Publish::new(
                        "device/target/up/properties_changed/2/1",
                        QoS::AtMostOnce,
                        payload,
                    )))
                    .await;
            }
            for payload in [
                json!({"device_id":"foreign","event":"online"}),
                json!({"device_id":"target","event":"unknown"}),
                json!({"device_id":"target","event":"online","uid":"foreign"}),
                json!({"device_id":"target","event":"online","uid":true}),
            ] {
                broker
                    .send(Packet::Publish(Publish::new(
                        "device/target/state/online",
                        QoS::AtMostOnce,
                        payload.to_string(),
                    )))
                    .await;
            }
            broker.property(json!(false)).await;
            broker
                .send(Packet::Publish(Publish::new(
                    "device/target/state/offline",
                    QoS::AtMostOnce,
                    json!({"device_id":"target","event":"offline"}).to_string(),
                )))
                .await;
            done.recv_async().await.unwrap();
        },
        move |handle, driver| async move {
            let stop = handle.clone();
            let (result, ()) = future::zip(
                driver.run(move |event| {
                    collected.borrow_mut().push(event);
                    if collected.borrow().len() == 2 {
                        finished.send(()).unwrap();
                        stop.close();
                    }
                }),
                async {
                    handle.subscribe_property("target", POWER).await.unwrap();
                    handle.subscribe_online("target").await.unwrap();
                },
            )
            .await;
            result.unwrap();
        },
    ));
    assert_eq!(
        *events.borrow(),
        [
            NotificationEvent::Property {
                did: "target".into(),
                binding: POWER,
                value: false
            },
            NotificationEvent::Online {
                did: "target".into(),
                online: false
            }
        ]
    );
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
                finished.send(()).unwrap();
            })
            .await;
            result.unwrap();
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
        [NotificationEvent::Property {
            did: "target".into(),
            binding: POWER,
            value: true
        }]
    );
}

#[test]
fn qos2_duplicate_and_retired_subscription_messages_do_not_reapply() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let collected = events.clone();
    let (retire, retired) = flume::bounded(1);
    let (finished, done) = flume::bounded(1);
    future::block_on(exercise(
        timing(),
        move |mut broker| async move {
            broker.setup().await;
            let pkid = broker
                .subscribe("device/target/up/properties_changed/2/1")
                .await;
            broker.accept(pkid).await;
            let mut old = Publish::new(
                "device/target/up/properties_changed/2/1",
                QoS::ExactlyOnce,
                json!({"params":{"siid":2,"piid":1,"value":false}}).to_string(),
            );
            old.pkid = 7;
            broker.send(Packet::Publish(old.clone())).await;
            assert!(matches!(broker.next().await, Packet::PubRec(rec) if rec.pkid == 7));
            old.dup = true;
            broker.send(Packet::Publish(old)).await;
            assert!(matches!(broker.next().await, Packet::PubRec(rec) if rec.pkid == 7));
            retire.send(()).unwrap();
            let Packet::Unsubscribe(unsub) = broker.next().await else {
                panic!();
            };
            broker
                .send(Packet::UnsubAck(UnsubAck {
                    pkid: unsub.pkid,
                    reasons: vec![UnsubAckReason::Success],
                    properties: None,
                }))
                .await;
            let pkid = broker
                .subscribe("device/target/up/properties_changed/2/1")
                .await;
            broker.accept(pkid).await;
            broker.send(Packet::PubRel(PubRel::new(7))).await;
            assert!(matches!(broker.next().await, Packet::PubComp(comp) if comp.pkid == 7));
            let mut new = Publish::new(
                "device/target/up/properties_changed/2/1",
                QoS::ExactlyOnce,
                json!({"params":{"siid":2,"piid":1,"value":true}}).to_string(),
            );
            new.pkid = 8;
            broker.send(Packet::Publish(new)).await;
            assert!(matches!(broker.next().await, Packet::PubRec(rec) if rec.pkid == 8));
            broker.send(Packet::PubRel(PubRel::new(8))).await;
            assert!(matches!(broker.next().await, Packet::PubComp(comp) if comp.pkid == 8));
            broker.send(Packet::PubRel(PubRel::new(8))).await;
            assert!(matches!(broker.next().await, Packet::PubComp(comp) if comp.pkid == 8));
            broker.property(json!(false)).await;
            done.recv_async().await.unwrap();
        },
        move |handle, driver| async move {
            let stop = handle.clone();
            let (result, ()) = future::zip(
                driver.run(move |event| {
                    collected.borrow_mut().push(event);
                    if collected.borrow().len() == 2 {
                        finished.send(()).unwrap();
                        stop.close();
                    }
                }),
                async {
                    handle.subscribe_property("target", POWER).await.unwrap();
                    retired.recv_async().await.unwrap();
                    handle.unsubscribe_properties("target").await.unwrap();
                    handle.subscribe_property("target", POWER).await.unwrap();
                },
            )
            .await;
            result.unwrap();
        },
    ));
    assert_eq!(
        *events.borrow(),
        [
            NotificationEvent::Property {
                did: "target".into(),
                binding: POWER,
                value: true,
            },
            NotificationEvent::Property {
                did: "target".into(),
                binding: POWER,
                value: false,
            }
        ]
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
fn default_notification_timing_matches_protocol_bounds() {
    assert_eq!(Timing::default().request, Duration::from_secs(10));
    assert_eq!(Timing::default().keepalive, Some(Duration::from_secs(60)));
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
    assert!(handle.shared.queue.borrow().len() <= 64);
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
            NotificationEvent::Property {
                did: "other".into(),
                binding: POWER,
                value: true
            }
        ]
    );
}

#[test]
fn cloud_connect_authentication_rejections_are_sanitized() {
    for code in [
        ConnectReturnCode::BadUserNamePassword,
        ConnectReturnCode::NotAuthorized,
    ] {
        future::block_on(async {
            let listener = Async::new(TcpListener::bind("127.0.0.1:0").unwrap()).unwrap();
            let address = listener.get_ref().local_addr().unwrap();
            future::zip(
                async {
                    let (stream, _) = listener.accept().await.unwrap();
                    let mut broker = Broker {
                        stream,
                        frames: mqtt::Frames::default(),
                    };
                    let Packet::Connect(connect) = broker.next().await else {
                        panic!();
                    };
                    assert_eq!(connect.login.unwrap().password, "access-secret");
                    broker
                        .send(Packet::ConnAck(ConnAck::new(code, false)))
                        .await;
                },
                async {
                    let stream = Async::<TcpStream>::connect(address).await.unwrap();
                    let error = start(Box::new(stream), &credentials(), timing())
                        .await
                        .err()
                        .unwrap();
                    assert_eq!(error, NotificationError::Unauthorized);
                    assert!(!format!("{error:?} {error}").contains("secret"));
                },
            )
            .await;
        });
    }
    for field in ["region", "uid", "uuid", "token"] {
        let mut record = credentials();
        match field {
            "region" => record.region = "us".into(),
            "uid" => record.uid.clear(),
            "uuid" => record.oauth_client_uuid = "invalid".into(),
            "token" => record.tokens.access_token = "token\n".into(),
            _ => unreachable!(),
        }
        let error = future::block_on(start(
            Box::new(futures_lite::io::Cursor::new(Vec::new())),
            &record,
            timing(),
        ))
        .err()
        .unwrap();
        assert_eq!(error, NotificationError::InvalidInput);
    }
}

#[test]
fn keepalive_responses_reset_the_wait_and_silence_ends_the_connection() {
    let (finished, done) = flume::bounded(1);
    future::block_on(exercise(
        Timing {
            request: Duration::from_millis(25),
            keepalive: Some(Duration::from_millis(20)),
        },
        move |mut broker| async move {
            broker.setup().await;
            assert!(matches!(broker.next().await, Packet::PingReq));
            broker.send(Packet::PingResp).await;
            assert!(matches!(broker.next().await, Packet::PingReq));
            done.recv_async().await.unwrap();
        },
        move |handle, driver| async move {
            assert_eq!(driver.run(|_| {}).await, Err(NotificationError::Timeout));
            assert_eq!(
                handle.subscribe_online("target").await,
                Err(NotificationError::Cancelled)
            );
            finished.send(()).unwrap();
        },
    ));
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
        [NotificationEvent::Property {
            did: "target".into(),
            binding: POWER,
            value: true
        }]
    );
}
