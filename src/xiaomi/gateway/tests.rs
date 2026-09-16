use super::*;
use crate::{
    device::{
        AccountId, CommandOutcome, DeviceCommand, DeviceDid, DeviceService, FeatureIdentity,
        HomeId, Property, PropertyState, VacuumOperationalState,
    },
    storage::{Store, TokenSet, XiaomiRecord},
    xiaomi::{
        catalog::WireValue,
        catalog::compile_spec,
        discovery::NetworkEpoch,
        mqtt::{MqttConfig, MqttConnection, MqttSendGuard},
    },
};
use bytes::BytesMut;
use futures_lite::future::{self, block_on};
use futures_util::FutureExt;
use mqttbytes::{QoS, v5};
use std::rc::Rc;
use std::{
    cell::RefCell,
    collections::VecDeque,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

struct PendingClientPublish {
    publish: v5::Publish,
    completed: bool,
}

thread_local! {
    static PENDING_CLIENT_PUBLISHES: RefCell<VecDeque<PendingClientPublish>> = const { RefCell::new(VecDeque::new()) };
}

fn routed(payload: &str) -> Vec<u8> {
    MipsEnvelope {
        mid: 0,
        return_topic: None,
        payload: payload.into(),
        from: Some("local".into()),
    }
    .encode()
    .unwrap()
}

fn acknowledge_connect(stream: &mut TcpStream, buffer: &mut BytesMut) {
    PENDING_CLIENT_PUBLISHES.with(|pending| pending.borrow_mut().clear());
    assert!(matches!(
        mqtt_packet(stream, buffer),
        v5::Packet::Connect(_)
    ));
    mqtt_write(
        stream,
        &v5::Packet::ConnAck(v5::ConnAck::new(v5::ConnectReturnCode::Success, false)),
    );
}

fn acknowledge_subscribe(stream: &mut TcpStream, buffer: &mut BytesMut) -> v5::Subscribe {
    let v5::Packet::Subscribe(subscribe) = mqtt_packet(stream, buffer) else {
        panic!("expected subscription")
    };
    mqtt_write(
        stream,
        &v5::Packet::SubAck(v5::SubAck::new(
            subscribe.pkid,
            vec![v5::SubscribeReasonCode::QoS2; subscribe.filters.len()],
        )),
    );
    subscribe
}

#[test]
fn mips_round_trip_uses_utf8_byte_lengths_and_rejects_duplicate_required_fields() {
    let envelope = MipsEnvelope::request(7, "虚拟/reply", r#"{"name":"灯"}"#).unwrap();
    assert_eq!(
        MipsEnvelope::decode(&envelope.encode().unwrap()).unwrap(),
        envelope
    );
    let mut duplicate = envelope.encode().unwrap();
    duplicate.extend_from_slice(&[4, 0, 0, 0, 0, 7, 0, 0, 0]);
    assert!(MipsEnvelope::decode(&duplicate).is_err());
    for malformed in [&duplicate[..3], &[5, 0, 0, 0, 2, 0xff, 0xff][..]] {
        assert!(MipsEnvelope::decode(malformed).is_err());
    }
}

#[test]
fn gateway_notifications_validate_topic_payload_and_preserve_event_arguments() {
    let property = GatewayNotification::parse(
        "virtual/appMsg/notify/iot/did/property/2.3",
        &routed(r#"{"did":"did","siid":2,"piid":3,"value":24}"#),
        false,
        &["did".into()],
        8,
        3,
    )
    .unwrap();
    assert_eq!(property.value(), Some(&WireValue::Integer(24)));
    let event = GatewayNotification::parse(
        "virtual/appMsg/notify/iot/did/event/2.4",
        &routed(r#"{"did":"did","siid":2,"eiid":4,"arguments":[{"piid":7,"value":true},{"piid":8,"value":9}]}"#),
        false,
        &["did".into()],
        8,
        3,
    )
    .unwrap();
    assert!(matches!(event.arguments(), Some(EventArguments::Keyed(values)) if values.len() == 2));
    let property_payload = routed(r#"{"did":"did","siid":2,"piid":3,"value":1}"#);
    let event_payload = routed(r#"{"did":"did","siid":2,"eiid":4}"#);
    for (topic, payload, retained) in [
        (
            "virtual/appMsg/notify/iot/other/property/2.3",
            property_payload.as_slice(),
            false,
        ),
        (
            "virtual/appMsg/notify/iot/did/property/2.9",
            property_payload.as_slice(),
            false,
        ),
        (
            "virtual/appMsg/notify/iot/did/event/2.4",
            event_payload.as_slice(),
            true,
        ),
    ] {
        assert!(
            GatewayNotification::parse(topic, payload, retained, &["did".into()], 8, 3).is_err()
        );
    }
    let zero_argument = GatewayNotification::parse(
        "virtual/appMsg/notify/iot/did/event/2.4",
        &event_payload,
        false,
        &["did".into()],
        8,
        3,
    )
    .unwrap();
    assert!(
        matches!(zero_argument.arguments(), Some(EventArguments::Positional(values)) if values.is_empty())
    );
    let changed = GatewayNotification::parse(
        "virtual/appMsg/devListChange",
        &routed(r#"{"devList":["did","new.did"]}"#),
        false,
        &[],
        8,
        4,
    )
    .unwrap();
    assert!(matches!(
        changed,
        GatewayNotification::DeviceListChanged {
            dids,
            generation: 4,
            ..
        } if dids == ["did", "new.did"]
    ));
}

fn mqtt_packet(stream: &mut TcpStream, buffer: &mut BytesMut) -> v5::Packet {
    loop {
        match v5::read(buffer, 256 * 1024) {
            Ok(packet) => return packet,
            Err(mqttbytes::Error::InsufficientBytes(_)) => {
                let mut chunk = [0; 4096];
                let count = stream.read(&mut chunk).unwrap();
                assert!(count > 0);
                buffer.extend_from_slice(&chunk[..count]);
            }
            Err(mqttbytes::Error::PayloadRequired) => {
                return v5::Packet::Disconnect(v5::Disconnect::new());
            }
            Err(error) => panic!("invalid test packet: {error:?}"),
        }
    }
}

fn mqtt_write(stream: &mut TcpStream, packet: &v5::Packet) {
    let mut bytes = BytesMut::new();
    match packet {
        v5::Packet::ConnAck(value) => {
            bytes.extend_from_slice(&[
                0x20,
                0x03,
                value.session_present as u8,
                value.code as u8,
                0,
            ]);
        }
        v5::Packet::SubAck(value) => {
            value.write(&mut bytes).unwrap();
        }
        v5::Packet::UnsubAck(value) => {
            value.write(&mut bytes).unwrap();
        }
        v5::Packet::PubRec(value) => {
            value.write(&mut bytes).unwrap();
        }
        v5::Packet::PubRel(value) => {
            value.write(&mut bytes).unwrap();
        }
        v5::Packet::PubComp(value) => {
            value.write(&mut bytes).unwrap();
        }
        v5::Packet::Publish(value) => {
            value.write(&mut bytes).unwrap();
        }
        _ => panic!("unsupported test packet"),
    }
    stream.write_all(&bytes).unwrap();
}

fn finish_mqtt(stream: &mut TcpStream, buffer: &mut BytesMut) {
    loop {
        match mqtt_packet(stream, buffer) {
            v5::Packet::PubRec(received) => {
                mqtt_write(stream, &v5::Packet::PubRel(v5::PubRel::new(received.pkid)));
            }
            v5::Packet::PubComp(_) => {}
            v5::Packet::Disconnect(_) => return,
            packet => panic!("unexpected MQTT packet while closing: {packet:?}"),
        }
    }
}

fn receive_mips_request(
    stream: &mut TcpStream,
    buffer: &mut BytesMut,
    expected_topic: &str,
) -> MipsEnvelope {
    if let Some(request) = PENDING_CLIENT_PUBLISHES.with(|pending| pending.borrow_mut().pop_front())
    {
        assert_eq!(request.publish.topic, expected_topic);
        if !request.completed {
            complete_client_publish(stream, buffer, request.publish.pkid);
        }
        return MipsEnvelope::decode(&request.publish.payload).unwrap();
    }
    let request = loop {
        match mqtt_packet(stream, buffer) {
            v5::Packet::Publish(request) => break request,
            v5::Packet::PubRec(received) => {
                mqtt_write(stream, &v5::Packet::PubRel(v5::PubRel::new(received.pkid)));
            }
            v5::Packet::PubComp(_) => {}
            packet => panic!("expected gateway request publish, got {packet:?}"),
        }
    };
    assert_eq!(request.topic, expected_topic);
    let envelope = MipsEnvelope::decode(&request.payload).unwrap();
    assert_eq!(envelope.return_topic.as_deref(), Some("virtual/reply"));
    mqtt_write(stream, &v5::Packet::PubRec(v5::PubRec::new(request.pkid)));
    complete_client_publish(stream, buffer, request.pkid);
    envelope
}

fn complete_client_publish(stream: &mut TcpStream, buffer: &mut BytesMut, packet_id: u16) {
    loop {
        match mqtt_packet(stream, buffer) {
            v5::Packet::PubRel(released) if released.pkid == packet_id => {
                mqtt_write(
                    stream,
                    &v5::Packet::PubComp(v5::PubComp::new(released.pkid)),
                );
                return;
            }
            v5::Packet::Publish(request) => {
                mqtt_write(stream, &v5::Packet::PubRec(v5::PubRec::new(request.pkid)));
                queue_client_publish(request);
            }
            v5::Packet::PubRel(released) => {
                mqtt_write(
                    stream,
                    &v5::Packet::PubComp(v5::PubComp::new(released.pkid)),
                );
                mark_client_publish_completed(released.pkid);
            }
            v5::Packet::PubComp(_) => {}
            packet => panic!("unexpected packet while completing client publish: {packet:?}"),
        }
    }
}

fn queue_client_publish(publish: v5::Publish) {
    PENDING_CLIENT_PUBLISHES.with(|pending| {
        pending.borrow_mut().push_back(PendingClientPublish {
            publish,
            completed: false,
        });
    });
}

fn mark_client_publish_completed(packet_id: u16) {
    PENDING_CLIENT_PUBLISHES.with(|pending| {
        if let Some(request) = pending
            .borrow_mut()
            .iter_mut()
            .find(|request| request.publish.pkid == packet_id)
        {
            request.completed = true;
        }
    });
}

fn send_mips_reply(
    stream: &mut TcpStream,
    buffer: &mut BytesMut,
    packet_id: u16,
    mid: u32,
    payload: &str,
) {
    let response = MipsEnvelope {
        mid,
        return_topic: None,
        payload: payload.into(),
        from: Some("local".into()),
    }
    .encode()
    .unwrap();
    let mut publish = v5::Publish::new("virtual/reply", QoS::ExactlyOnce, response);
    publish.pkid = packet_id;
    mqtt_write(stream, &v5::Packet::Publish(publish));
    loop {
        match mqtt_packet(stream, buffer) {
            v5::Packet::PubRec(received) if received.pkid == packet_id => break,
            v5::Packet::Publish(request) => {
                mqtt_write(stream, &v5::Packet::PubRec(v5::PubRec::new(request.pkid)));
                queue_client_publish(request);
            }
            v5::Packet::PubRel(released) => {
                mqtt_write(
                    stream,
                    &v5::Packet::PubComp(v5::PubComp::new(released.pkid)),
                );
                mark_client_publish_completed(released.pkid);
            }
            v5::Packet::PubComp(_) => {}
            packet => panic!("unexpected packet before PUBREC: {packet:?}"),
        }
    }
    mqtt_write(stream, &v5::Packet::PubRel(v5::PubRel::new(packet_id)));
    loop {
        match mqtt_packet(stream, buffer) {
            v5::Packet::PubComp(completed) if completed.pkid == packet_id => break,
            v5::Packet::Publish(request) => {
                mqtt_write(stream, &v5::Packet::PubRec(v5::PubRec::new(request.pkid)));
                queue_client_publish(request);
            }
            v5::Packet::PubRel(released) => {
                mqtt_write(
                    stream,
                    &v5::Packet::PubComp(v5::PubComp::new(released.pkid)),
                );
                mark_client_publish_completed(released.pkid);
            }
            packet => panic!("unexpected packet before PUBCOMP: {packet:?}"),
        }
    }
}

#[test]
fn real_mqtt_gateway_get_dev_list_is_the_only_session_evidence() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (first_request_tx, first_request_rx) = flume::bounded(1);
    let (complete_tx, complete_rx) = flume::bounded(1);
    let broker_stage = Arc::new(AtomicUsize::new(0));
    let observed_stage = broker_stage.clone();
    let broker = thread::spawn(move || {
        let mut stream = listener.accept().unwrap().0;
        broker_stage.store(1, Ordering::Release);
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut buffer = BytesMut::new();
        assert!(matches!(
            mqtt_packet(&mut stream, &mut buffer),
            v5::Packet::Connect(_)
        ));
        broker_stage.store(2, Ordering::Release);
        mqtt_write(
            &mut stream,
            &v5::Packet::ConnAck(v5::ConnAck::new(v5::ConnectReturnCode::Success, false)),
        );
        let v5::Packet::Subscribe(subscribe) = mqtt_packet(&mut stream, &mut buffer) else {
            panic!("expected initial subscription")
        };
        broker_stage.store(3, Ordering::Release);
        assert_eq!(
            subscribe
                .filters
                .iter()
                .map(|filter| filter.path.as_str())
                .collect::<Vec<_>>(),
            ["virtual/#", "master/appMsg/devListChange"]
        );
        mqtt_write(
            &mut stream,
            &v5::Packet::SubAck(v5::SubAck::new(
                subscribe.pkid,
                vec![v5::SubscribeReasonCode::QoS2; 2],
            )),
        );
        broker_stage.store(4, Ordering::Release);
        let rejected = receive_mips_request(&mut stream, &mut buffer, "master/proxy/getDevList");
        send_mips_reply(
            &mut stream,
            &mut buffer,
            70,
            rejected.mid,
            r#"{"error":{"code":-704042011}}"#,
        );
        let first = receive_mips_request(&mut stream, &mut buffer, "master/proxy/getDevList");
        first_request_tx.send(()).unwrap();
        let second = receive_mips_request(&mut stream, &mut buffer, "master/proxy/getDevList");
        let first_devices = r#"{"devList":{"first.did":{"name":"First","urn":"urn:miot-spec-v2:device:light:0000A001:demo:1","model":"vendor.light","online":true}}}"#;
        let second_devices = r#"{"devList":{"second.did":{"name":"Second","urn":"urn:miot-spec-v2:device:light:0000A001:demo:1","model":"vendor.light","online":true}}}"#;
        send_mips_reply(&mut stream, &mut buffer, 71, second.mid, second_devices);
        send_mips_reply(&mut stream, &mut buffer, 72, first.mid, first_devices);
        let set = receive_mips_request(&mut stream, &mut buffer, "master/proxy/rpcReq");
        let set_json: serde_json::Value = serde_json::from_str(&set.payload).unwrap();
        assert_eq!(set_json["rpc"]["method"], "set_properties");
        assert_eq!(set_json["rpc"]["params"][0]["value"].as_i64(), Some(24));
        send_mips_reply(
            &mut stream,
            &mut buffer,
            73,
            set.mid,
            r#"{"result":[{"did":"first.did","siid":2,"piid":3,"code":0}]}"#,
        );
        let action = receive_mips_request(&mut stream, &mut buffer, "master/proxy/rpcReq");
        let action_json: serde_json::Value = serde_json::from_str(&action.payload).unwrap();
        assert_eq!(action_json["rpc"]["method"], "action");
        assert_eq!(
            action_json["rpc"]["params"]["in"],
            serde_json::json!([24, true])
        );
        send_mips_reply(
            &mut stream,
            &mut buffer,
            74,
            action.mid,
            r#"{"result":{"did":"first.did","siid":2,"aiid":4,"code":0}}"#,
        );
        complete_tx.send(()).unwrap();
        finish_mqtt(&mut stream, &mut buffer);
    });

    block_on(async {
        let completed = future::or(
            async {
                let (mqtt, mqtt_handle, messages) = MqttConnection::new(
                    MqttConfig::new("gateway-client", None, Duration::from_secs(5))
                        .with_endpoint(address.ip().to_string(), address.port()),
                )
                .unwrap();
                let (session, gateway, _) = GatewaySession::new(
                    "virtual",
                    123,
                    "192.0.2.10",
                    NetworkEpoch::new(9),
                    mqtt_handle.clone(),
                    messages,
                )
                .unwrap();
                let mut driver = Box::pin(mqtt.run());
                let mut session = Box::pin(session.run(Instant::now() + Duration::from_secs(1)));
                let mut app = Box::pin(async {
                    let rejected = gateway
                        .get_devices(
                            Instant::now() + Duration::from_secs(1),
                            MqttSendGuard::new(),
                        )
                        .await
                        .unwrap_err();
                    assert_eq!(rejected.kind(), &GatewayErrorKind::Business(-704042011));
                    let mut first = Box::pin(gateway.get_devices(
                        Instant::now() + Duration::from_secs(1),
                        MqttSendGuard::new(),
                    ));
                    assert!(future::poll_once(first.as_mut()).await.is_none());
                    first_request_rx.recv_async().await.unwrap();
                    let (first, second) = future::zip(
                        first,
                        gateway.get_devices(
                            Instant::now() + Duration::from_secs(1),
                            MqttSendGuard::new(),
                        ),
                    )
                    .await;
                    let first = first.unwrap();
                    let second = second.unwrap();
                    for evidence in [&first, &second] {
                        assert_eq!(evidence.gateway_did, 123);
                        assert_eq!(evidence.peer_did, "192.0.2.10");
                        assert_eq!(evidence.epoch, NetworkEpoch::new(9));
                        assert_eq!(evidence.devices.len(), 1);
                    }
                    assert_eq!(first.devices[0].did, "first.did");
                    assert_eq!(first.devices[0].name, "First");
                    assert_eq!(second.devices[0].did, "second.did");
                    assert_eq!(second.devices[0].name, "Second");
                    gateway
                        .set_property(
                            "first.did",
                            2,
                            3,
                            WireValue::Integer(24),
                            Instant::now() + Duration::from_secs(1),
                            MqttSendGuard::new(),
                        )
                        .await
                        .unwrap();
                    gateway
                        .invoke_action(
                            "first.did",
                            2,
                            4,
                            vec![WireValue::Integer(24), WireValue::Boolean(true)],
                            Instant::now() + Duration::from_secs(1),
                            MqttSendGuard::new(),
                        )
                        .await
                        .unwrap();
                    complete_rx.recv_async().await.unwrap();
                    mqtt_handle.stop().await;
                });
                enum First {
                    App,
                    Driver(Result<(), crate::xiaomi::mqtt::MqttError>),
                    Session(Result<(), GatewayError>),
                }
                let first = future::or(
                    async {
                        app.as_mut().await;
                        First::App
                    },
                    future::or(async { First::Driver(driver.as_mut().await) }, async {
                        First::Session(session.as_mut().await)
                    }),
                )
                .await;
                match first {
                    First::App => {}
                    First::Driver(result) => {
                        panic!(
                            "MQTT driver exited before the gateway scenario at broker stage {}: {result:?}",
                            observed_stage.load(Ordering::Acquire)
                        )
                    }
                    First::Session(result) => {
                        let driver_result = future::poll_once(driver.as_mut()).await;
                        panic!(
                            "gateway session exited before the scenario at broker stage {}; driver={driver_result:?}: {result:?}",
                            observed_stage.load(Ordering::Acquire)
                        )
                    }
                }
                let (mqtt_result, session_result) = future::zip(driver, session).await;
                mqtt_result.unwrap();
                assert_eq!(
                    session_result.unwrap_err().kind(),
                    &GatewayErrorKind::Transport
                );
                true
            },
            async {
                async_io::Timer::after(Duration::from_secs(5)).await;
                false
            },
        )
        .await;
        assert!(
            completed,
            "gateway MQTT scenario exceeded its hard deadline"
        );
    });
    broker.join().unwrap();
}

#[test]
fn cancelled_gateway_selection_does_not_close_the_session_or_relabel_removed_notifications() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (first_seen, first_ready) = flume::bounded(1);
    let (release_first, release) = flume::bounded(1);
    let (overlap_seen, overlap_ready) = flume::bounded(1);
    let (release_overlap, overlap_release) = flume::bounded(1);
    let (done, broker_done) = flume::bounded(1);
    let broker = thread::spawn(move || {
        let mut stream = listener.accept().unwrap().0;
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut buffer = BytesMut::new();
        acknowledge_connect(&mut stream, &mut buffer);
        acknowledge_subscribe(&mut stream, &mut buffer);
        let v5::Packet::Subscribe(first) = mqtt_packet(&mut stream, &mut buffer) else {
            panic!("expected first gateway notification subscription")
        };
        assert!(
            first
                .filters
                .iter()
                .all(|filter| filter.path.contains("old.did"))
        );
        first_seen.send(()).unwrap();
        release.recv_timeout(Duration::from_secs(3)).unwrap();
        mqtt_write(
            &mut stream,
            &v5::Packet::SubAck(v5::SubAck::new(
                first.pkid,
                vec![v5::SubscribeReasonCode::QoS2; first.filters.len()],
            )),
        );
        let latest = loop {
            match mqtt_packet(&mut stream, &mut buffer) {
                v5::Packet::Unsubscribe(remove) => {
                    assert!(remove.filters.iter().all(|topic| topic.contains("old.did")));
                    let mut ack = v5::UnsubAck::new(remove.pkid);
                    ack.reasons = vec![v5::UnsubAckReason::Success; remove.filters.len()];
                    mqtt_write(&mut stream, &v5::Packet::UnsubAck(ack));
                }
                v5::Packet::Subscribe(latest) => break latest,
                packet => panic!("unexpected gateway selection packet: {packet:?}"),
            }
        };
        assert!(
            latest
                .filters
                .iter()
                .all(|filter| filter.path.contains("new.did"))
        );
        mqtt_write(
            &mut stream,
            &v5::Packet::SubAck(v5::SubAck::new(
                latest.pkid,
                vec![v5::SubscribeReasonCode::QoS2; latest.filters.len()],
            )),
        );
        for did in ["old.did", "new.did"] {
            mqtt_write(
                &mut stream,
                &v5::Packet::Publish(v5::Publish::new(
                    format!("virtual/appMsg/notify/iot/{did}/property/2.3"),
                    QoS::AtMostOnce,
                    routed(&format!(r#"{{"did":"{did}","siid":2,"piid":3,"value":9}}"#)),
                )),
            );
            mqtt_write(
                &mut stream,
                &v5::Packet::Publish(v5::Publish::new(
                    format!("virtual/appMsg/notify/iot/{did}/event/2.4"),
                    QoS::AtMostOnce,
                    routed(&format!(
                        r#"{{"did":"{did}","siid":2,"eiid":4,"arguments":[]}}"#
                    )),
                )),
            );
        }
        let overlap = loop {
            match mqtt_packet(&mut stream, &mut buffer) {
                v5::Packet::Unsubscribe(remove) => {
                    assert!(remove.filters.iter().all(|topic| topic.contains("new.did")));
                    let mut ack = v5::UnsubAck::new(remove.pkid);
                    ack.reasons = vec![v5::UnsubAckReason::Success; remove.filters.len()];
                    mqtt_write(&mut stream, &v5::Packet::UnsubAck(ack));
                }
                v5::Packet::Subscribe(overlap) => break overlap,
                packet => panic!("unexpected overlapping gateway packet: {packet:?}"),
            }
        };
        assert!(
            overlap
                .filters
                .iter()
                .all(|filter| filter.path.contains("middle.did"))
        );
        overlap_seen.send(()).unwrap();
        overlap_release
            .recv_timeout(Duration::from_secs(3))
            .unwrap();
        mqtt_write(
            &mut stream,
            &v5::Packet::SubAck(v5::SubAck::new(
                overlap.pkid,
                vec![v5::SubscribeReasonCode::QoS2; overlap.filters.len()],
            )),
        );
        let final_selection = loop {
            match mqtt_packet(&mut stream, &mut buffer) {
                v5::Packet::Unsubscribe(remove) => {
                    assert!(
                        remove
                            .filters
                            .iter()
                            .all(|topic| topic.contains("middle.did"))
                    );
                    let mut ack = v5::UnsubAck::new(remove.pkid);
                    ack.reasons = vec![v5::UnsubAckReason::Success; remove.filters.len()];
                    mqtt_write(&mut stream, &v5::Packet::UnsubAck(ack));
                }
                v5::Packet::Subscribe(final_selection) => break final_selection,
                packet => panic!("unexpected final gateway selection packet: {packet:?}"),
            }
        };
        assert!(
            final_selection
                .filters
                .iter()
                .all(|filter| filter.path.contains("final.did"))
        );
        mqtt_write(
            &mut stream,
            &v5::Packet::SubAck(v5::SubAck::new(
                final_selection.pkid,
                vec![v5::SubscribeReasonCode::QoS2; final_selection.filters.len()],
            )),
        );
        broker_done.recv_timeout(Duration::from_secs(3)).unwrap();
    });

    block_on(async {
        let completed = future::or(
            async {
                let (connection, mqtt, messages) = MqttConnection::new(
                    MqttConfig::new("gateway-selection", None, Duration::from_secs(5))
                        .with_endpoint(address.ip().to_string(), address.port()),
                )
                .unwrap();
                let (session, handle, notifications) = GatewaySession::new(
                    "virtual",
                    123,
                    "192.0.2.10",
                    NetworkEpoch::new(9),
                    mqtt.clone(),
                    messages,
                )
                .unwrap();
                let mut driver = Box::pin(connection.run());
                let mut session = Box::pin(session.run_guarded(
                    Instant::now() + Duration::from_secs(2),
                    MqttSendGuard::new(),
                ));
                let app = async {
                    let mut abandoned = Box::pin(handle.select_notifications(
                        vec!["old.did".into()],
                        Instant::now() + Duration::from_secs(2),
                    ));
                    assert!(future::poll_once(abandoned.as_mut()).await.is_none());
                    first_ready.recv_async().await.unwrap();
                    drop(abandoned);
                    let mut latest = Box::pin(handle.select_notifications(
                        vec!["new.did".into()],
                        Instant::now() + Duration::from_secs(2),
                    ));
                    assert!(future::poll_once(latest.as_mut()).await.is_none());
                    release_first.send_async(()).await.unwrap();
                    let generation = latest.await.unwrap();
                    let notification = notifications.recv_async().await.unwrap();
                    assert!(matches!(
                        notification,
                        GatewayNotification::Property { did, generation: actual, .. }
                            if did == "new.did" && actual == generation
                    ));
                    let event = notifications.recv_async().await.unwrap();
                    assert!(matches!(
                        event,
                        GatewayNotification::Event { did, generation: actual, .. }
                            if did == "new.did" && actual == generation
                    ));
                    let mut overlap = Box::pin(handle.select_notifications(
                        vec!["middle.did".into()],
                        Instant::now() + Duration::from_secs(2),
                    ));
                    assert!(future::poll_once(overlap.as_mut()).await.is_none());
                    overlap_ready.recv_async().await.unwrap();
                    let mut final_selection = Box::pin(handle.select_notifications(
                        vec!["final.did".into()],
                        Instant::now() + Duration::from_secs(2),
                    ));
                    assert!(future::poll_once(final_selection.as_mut()).await.is_none());
                    release_overlap.send_async(()).await.unwrap();
                    assert_eq!(
                        overlap.await.unwrap_err().kind(),
                        &GatewayErrorKind::Superseded
                    );
                    final_selection.await.unwrap();
                    done.send_async(()).await.unwrap();
                    mqtt.stop().await;
                };
                futures_lite::pin!(app);
                enum Ready {
                    App,
                    Driver,
                    Session,
                }
                match future::or(
                    app.map(|()| Ready::App),
                    future::or(
                        driver.as_mut().map(|_| Ready::Driver),
                        session.as_mut().map(|_| Ready::Session),
                    ),
                )
                .await
                {
                    Ready::App => {}
                    Ready::Driver => panic!("MQTT driver exited before gateway assertions"),
                    Ready::Session => panic!("gateway session exited before assertions"),
                }
                true
            },
            async {
                async_io::Timer::after(Duration::from_secs(5)).await;
                false
            },
        )
        .await;
        assert!(
            completed,
            "gateway selection scenario exceeded its deadline"
        );
    });
    broker.join().unwrap();
}
