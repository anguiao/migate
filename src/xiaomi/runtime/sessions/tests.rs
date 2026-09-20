mod auth;
mod cloud;
mod gateway;
mod lan;
mod lifecycle;
mod reconciliation;

use super::*;
use crate::storage::Store;
use crate::xiaomi::auth::AuthService;
use crate::xiaomi::discovery::DiscoveryRegistry;
use crate::xiaomi::runtime::RuntimeTransports;
use crate::xiaomi::runtime::XiaomiRuntime;
use crate::xiaomi::runtime::cloud_connection::active_cloud_connection_lifetime;
use crate::xiaomi::runtime::cloud_connection::{
    CloudConnectionControl, CloudConnectionFact, CloudSelectionResult, cloud_connection_fact,
};
use crate::xiaomi::runtime::connection::{ConnectionSetup, gateway_failure_code};
use crate::xiaomi::runtime::connection::{advance_retry, lan_failure_code, startup_failure_code};
use crate::xiaomi::runtime::gateway_connection::{
    GatewayConnectionConfig, GatewayConnectionControl, GatewayConnectionFact,
    GatewayOperationResult, GatewayPublication, gateway_connection_fact,
    gateway_full_connection_lifetime,
};
use crate::xiaomi::runtime::lan_connection::{
    LanAttemptResources, LanConnectionConfig, LanConnectionControl, lan_connection_fact,
    lan_connection_lifetime,
};
use crate::xiaomi::runtime::{
    AdmissionFeature, AdmissionSnapshot, AdmissionStatus, OperationPaths, RuntimeFeature,
};
use crate::xiaomi::runtime::{
    XiaomiBoundaryDiagnostic, XiaomiFailureStage, XiaomiRuntimeComponent, XiaomiRuntimeDiagnostic,
    XiaomiSafeFailureCode, catalog_refresh::CatalogTaskResult, unix_time,
};
use crate::xiaomi::{
    catalog::assemble_catalog,
    cloud::{CloudClient, CloudNotification},
    discovery::{GatewayCandidate, NetworkEpoch, NetworkUpdate},
    gateway::GatewayNotification,
    lan::LanProperty,
    runtime::{AuthenticatedGateway, CloudEvidence, CloudStatus, PushSource, SessionAuthority},
};
use crate::xiaomi::{discovery::GatewayEndpoint, mqtt::MqttConfig, runtime::TransportStartupError};
use async_io::Timer;
use bytes::BytesMut;
use futures_lite::future;
use futures_lite::future::{block_on, poll_once};
use futures_util::{FutureExt, future::LocalBoxFuture};
use mqttbytes::{QoS, v5};
use std::time::{Duration, Instant};
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet, HashMap},
    rc::Rc,
};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    thread,
};

fn mqtt_packet(stream: &mut TcpStream, buffer: &mut BytesMut) -> v5::Packet {
    loop {
        match v5::read(buffer, 256 * 1024) {
            Ok(packet) => return packet,
            Err(mqttbytes::Error::InsufficientBytes(_)) => {
                let mut bytes = [0; 4096];
                let count = stream.read(&mut bytes).unwrap();
                assert!(count > 0);
                buffer.extend_from_slice(&bytes[..count]);
            }
            Err(error) => panic!("invalid MQTT packet: {error:?}"),
        }
    }
}

fn mqtt_write(stream: &mut TcpStream, packet: &v5::Packet) {
    let mut bytes = BytesMut::new();
    match packet {
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
        v5::Packet::ConnAck(value) => {
            bytes.extend_from_slice(&[0x20, 0x03, value.session_present as u8, value.code as u8, 0])
        }
        packet => panic!("unsupported MQTT test packet: {packet:?}"),
    }
    stream.write_all(&bytes).unwrap();
}

fn mqtt_packet_ignoring_ping(stream: &mut TcpStream, buffer: &mut BytesMut) -> v5::Packet {
    loop {
        let packet = mqtt_packet(stream, buffer);
        if matches!(packet, v5::Packet::PingReq) {
            stream.write_all(&[0xd0, 0x00]).unwrap();
            continue;
        }
        return packet;
    }
}

async fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !predicate() {
        assert!(
            Instant::now() < deadline,
            "condition exceeded its hard deadline"
        );
        Timer::after(Duration::from_millis(5)).await;
    }
}

fn receive_request(
    stream: &mut TcpStream,
    buffer: &mut BytesMut,
) -> crate::xiaomi::gateway::MipsEnvelope {
    let v5::Packet::Publish(publish) = mqtt_packet(stream, buffer) else {
        panic!("expected gateway request")
    };
    let envelope = crate::xiaomi::gateway::MipsEnvelope::decode(&publish.payload).unwrap();
    mqtt_write(stream, &v5::Packet::PubRec(v5::PubRec::new(publish.pkid)));
    assert!(matches!(
        mqtt_packet_ignoring_ping(stream, buffer),
        v5::Packet::PubRel(_)
    ));
    mqtt_write(stream, &v5::Packet::PubComp(v5::PubComp::new(publish.pkid)));
    envelope
}

fn reply(
    stream: &mut TcpStream,
    _buffer: &mut BytesMut,
    _packet_id: u16,
    topic: &str,
    mid: u32,
    payload: &str,
) {
    let payload = crate::xiaomi::gateway::MipsEnvelope {
        mid,
        return_topic: None,
        payload: payload.into(),
        from: Some("local".into()),
    }
    .encode()
    .unwrap();
    let mut publish = v5::Publish::new(topic, QoS::AtMostOnce, payload);
    publish.pkid = 0;
    mqtt_write(stream, &v5::Packet::Publish(publish));
}

enum CloudBrokerCommand {
    Acknowledge,
    Offline,
    Online,
    Property(bool),
    Stop,
}

struct CloudRuntimeHarness {
    handle: crate::xiaomi::cloud::CloudNotificationHandle,
    notifications: flume::Receiver<CloudNotification>,
    session: LocalBoxFuture<'static, Result<(), TransportStartupError>>,
    subscribed: std::sync::mpsc::Receiver<()>,
    commands: std::sync::mpsc::Sender<CloudBrokerCommand>,
    broker: thread::JoinHandle<()>,
}

fn real_cloud_runtime_connection(did: &str) -> CloudRuntimeHarness {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let expected = did.to_owned();
    let (subscribed, subscription) = std::sync::mpsc::sync_channel(1);
    let (commands, broker_commands) = std::sync::mpsc::channel();
    let broker = thread::spawn(move || {
        let mut stream = listener.accept().unwrap().0;
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut buffer = BytesMut::new();
        assert!(matches!(
            mqtt_packet(&mut stream, &mut buffer),
            v5::Packet::Connect(_)
        ));
        mqtt_write(
            &mut stream,
            &v5::Packet::ConnAck(v5::ConnAck::new(v5::ConnectReturnCode::Success, false)),
        );
        let v5::Packet::Subscribe(subscribe) = mqtt_packet(&mut stream, &mut buffer) else {
            panic!("expected cloud selection subscription")
        };
        assert_eq!(subscribe.filters.len(), 3);
        assert!(
            subscribe
                .filters
                .iter()
                .all(|filter| filter.path.contains(&expected))
        );
        subscribed.send(()).unwrap();
        for command in broker_commands {
            match command {
                CloudBrokerCommand::Acknowledge => mqtt_write(
                    &mut stream,
                    &v5::Packet::SubAck(v5::SubAck::new(
                        subscribe.pkid,
                        vec![v5::SubscribeReasonCode::QoS2; subscribe.filters.len()],
                    )),
                ),
                CloudBrokerCommand::Offline | CloudBrokerCommand::Online => {
                    let event = if matches!(command, CloudBrokerCommand::Online) {
                        "online"
                    } else {
                        "offline"
                    };
                    mqtt_write(
                        &mut stream,
                        &v5::Packet::Publish(v5::Publish::new(
                            format!("device/{expected}/state/change"),
                            QoS::AtMostOnce,
                            format!(r#"{{"device_id":"{expected}","event":"{event}"}}"#),
                        )),
                    );
                }
                CloudBrokerCommand::Property(value) => mqtt_write(
                    &mut stream,
                    &v5::Packet::Publish(v5::Publish::new(
                        format!("device/{expected}/up/properties_changed/2/1"),
                        QoS::AtMostOnce,
                        format!(
                            r#"{{"params":{{"did":"{expected}","siid":2,"piid":1,"value":{value}}}}}"#
                        ),
                    )),
                ),
                CloudBrokerCommand::Stop => break,
            }
        }
        let mut remaining = Vec::new();
        stream.read_to_end(&mut remaining).unwrap();
        buffer.extend_from_slice(&remaining);
        while !buffer.is_empty() {
            let packet = v5::read(&mut buffer, 256 * 1024).unwrap();
            assert!(
                !matches!(packet, v5::Packet::Subscribe(_)),
                "Cloud selection was sent more than once"
            );
        }
    });
    let (handle, notifications, live) = block_on(async {
        let (mqtt, session, handle, notifications) =
            crate::xiaomi::cloud::CloudNotificationSession::new(
                "oauth",
                "access",
                Duration::from_secs(5),
                &address.ip().to_string(),
                address.port(),
                None,
            )
            .unwrap();
        let live = future::or(
            async { mqtt.run().await.map_err(TransportStartupError::Mqtt) },
            async { session.run().await.map_err(TransportStartupError::Mqtt) },
        )
        .boxed_local();
        (handle, notifications, live)
    });
    CloudRuntimeHarness {
        handle,
        notifications,
        session: live,
        subscribed: subscription,
        commands,
        broker,
    }
}
