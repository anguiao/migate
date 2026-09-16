use super::*;
use bytes::BytesMut;
use futures_lite::future::{self, block_on};
use mqttbytes::v5;
use rcgen::{BasicConstraints, CertificateParams, CertifiedIssuer, DnType, IsCa, KeyPair};
use rustls::{
    RootCertStore,
    pki_types::{CertificateDer, PrivateKeyDer},
    server::WebPkiClientVerifier,
};
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio_rustls::TlsAcceptor;

struct TlsMaterial {
    ca_pem: String,
    server: CertificateDer<'static>,
    server_key: PrivateKeyDer<'static>,
    client_pem: String,
    client_key_pem: String,
}

fn tls_material(server_name: &str) -> TlsMaterial {
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "test CA");
    let ca = CertifiedIssuer::self_signed(ca_params, KeyPair::generate().unwrap()).unwrap();
    let server_key = KeyPair::generate().unwrap();
    let server = CertificateParams::new(vec![server_name.into()])
        .unwrap()
        .signed_by(&server_key, &ca)
        .unwrap();
    let client_key = KeyPair::generate().unwrap();
    let client = CertificateParams::new(vec!["client.test".into()])
        .unwrap()
        .signed_by(&client_key, &ca)
        .unwrap();
    TlsMaterial {
        ca_pem: ca.pem(),
        server: server.der().clone(),
        server_key: PrivateKeyDer::try_from(server_key.serialize_der()).unwrap(),
        client_pem: client.pem(),
        client_key_pem: client_key.serialize_pem(),
    }
}

fn tls_server_config(material: &TlsMaterial, require_client: bool) -> Arc<rustls::ServerConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .unwrap();
    let builder = if require_client {
        let mut roots = RootCertStore::empty();
        let mut reader = std::io::BufReader::new(material.ca_pem.as_bytes());
        for certificate in rustls_pemfile::certs(&mut reader) {
            roots.add(certificate.unwrap()).unwrap();
        }
        builder.with_client_cert_verifier(
            WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider)
                .build()
                .unwrap(),
        )
    } else {
        builder.with_no_client_auth()
    };
    Arc::new(
        builder
            .with_single_cert(
                vec![material.server.clone()],
                material.server_key.clone_key(),
            )
            .unwrap(),
    )
}

async fn read_tls_frame<S: AsyncRead + Unpin>(stream: &mut S) -> Vec<u8> {
    let first = stream.read_u8().await.unwrap();
    let mut bytes = vec![first];
    let mut remaining = 0usize;
    let mut multiplier = 1usize;
    loop {
        let byte = stream.read_u8().await.unwrap();
        bytes.push(byte);
        remaining += usize::from(byte & 127) * multiplier;
        if byte & 128 == 0 {
            break;
        }
        multiplier *= 128;
    }
    let mut body = vec![0; remaining];
    stream.read_exact(&mut body).await.unwrap();
    bytes.extend(body);
    bytes
}

fn frame_body_offset(frame: &[u8]) -> usize {
    let mut index = 1;
    while frame[index] & 128 != 0 {
        index += 1;
    }
    index + 1
}

fn spawn_tls_broker(
    listener: TcpListener,
    config: Arc<rustls::ServerConfig>,
    succeeds: bool,
    connected: Option<flume::Sender<()>>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            listener.set_nonblocking(true).unwrap();
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            let (stream, _) = listener.accept().await.unwrap();
            let accepted = TlsAcceptor::from(config).accept(stream).await;
            if !succeeds {
                assert!(accepted.is_err());
                return;
            }
            let mut stream = accepted.unwrap();
            assert_eq!(read_tls_frame(&mut stream).await[0] >> 4, 1);
            stream.write_all(&[0x20, 0x03, 0, 0, 0]).await.unwrap();
            let subscribe = read_tls_frame(&mut stream).await;
            assert_eq!(subscribe[0], 0x82);
            let offset = frame_body_offset(&subscribe);
            stream
                .write_all(&[0x90, 0x04, subscribe[offset], subscribe[offset + 1], 0, 2])
                .await
                .unwrap();
            if let Some(connected) = connected {
                connected.send_async(()).await.unwrap();
            }
            assert_eq!(read_tls_frame(&mut stream).await, [0xe0, 0]);
        });
    })
}

fn packet(stream: &mut TcpStream, buffer: &mut BytesMut) -> v5::Packet {
    loop {
        match v5::read(buffer, 256 * 1024) {
            Ok(packet) => return packet,
            Err(mqttbytes::Error::InsufficientBytes(_)) => {
                let mut bytes = [0; 4096];
                let count = stream.read(&mut bytes).unwrap();
                assert!(
                    count > 0,
                    "broker connection closed before the expected packet"
                );
                buffer.extend_from_slice(&bytes[..count]);
            }
            Err(mqttbytes::Error::PayloadRequired) => {
                return v5::Packet::Disconnect(v5::Disconnect::new());
            }
            Err(error) => panic!("invalid MQTT packet: {error:?}"),
        }
    }
}

fn write_packet(stream: &mut TcpStream, packet: &v5::Packet) {
    let mut bytes = BytesMut::new();
    match packet {
        v5::Packet::SubAck(value) => {
            value.write(&mut bytes).unwrap();
        }
        v5::Packet::PubRec(value) => {
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
        _ => panic!("unsupported broker packet"),
    }
    stream.write_all(&bytes).unwrap();
}

fn acknowledge_connect(stream: &mut TcpStream, buffer: &mut BytesMut) {
    assert!(matches!(packet(stream, buffer), v5::Packet::Connect(_)));
    write_packet(
        stream,
        &v5::Packet::ConnAck(v5::ConnAck::new(v5::ConnectReturnCode::Success, false)),
    );
}

fn config(address: SocketAddr) -> MqttConfig {
    MqttConfig::new("test-client", None, Duration::from_secs(5))
        .with_endpoint(address.ip().to_string(), address.port())
}

#[test]
fn dispatch_guard_is_checked_before_queue_acceptance() {
    let denied = MqttSendGuard::with_check(|| Ok(false));
    let (_connection, handle, _) =
        MqttConnection::new(config("127.0.0.1:9".parse().unwrap())).unwrap();
    let error = block_on(handle.publish_guarded(
        "a/b",
        vec![1],
        Instant::now() + Duration::from_secs(1),
        denied,
    ))
    .unwrap_err();
    assert_eq!(error.kind(), &MqttErrorKind::Disconnected);
}

#[test]
fn guard_change_after_dispatch_does_not_withdraw_qos2_publish() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (received_tx, received_rx) = flume::bounded(1);
    let (release_tx, release_rx) = flume::bounded(1);
    let (complete_tx, complete_rx) = flume::bounded(1);
    let broker = thread::spawn(move || {
        let mut stream = listener.accept().unwrap().0;
        let mut buffer = BytesMut::new();
        acknowledge_connect(&mut stream, &mut buffer);
        let v5::Packet::Publish(publish) = packet(&mut stream, &mut buffer) else {
            panic!("expected publish")
        };
        assert_eq!(publish.topic, "device/set");
        assert_eq!(&publish.payload[..], b"payload");
        received_tx.send(()).unwrap();
        release_rx.recv().unwrap();
        write_packet(
            &mut stream,
            &v5::Packet::PubRec(v5::PubRec::new(publish.pkid)),
        );
        assert!(matches!(
            packet(&mut stream, &mut buffer),
            v5::Packet::PubRel(_)
        ));
        write_packet(
            &mut stream,
            &v5::Packet::PubComp(v5::PubComp::new(publish.pkid)),
        );
        complete_tx.send(()).unwrap();
        let mut byte = [0];
        match stream.read(&mut byte) {
            Ok(0) => {}
            Ok(1) if byte[0] == 0xe0 => {
                stream.read_exact(&mut byte).unwrap();
                assert_eq!(byte[0], 0);
            }
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
            result => panic!("MQTT connection remained open: {result:?}"),
        }
    });
    block_on(async {
        let (connection, handle, _) = MqttConnection::new(config(address)).unwrap();
        let guard = MqttSendGuard::new();
        let accepted = guard.clone();
        let run = connection.run();
        let app = async move {
            handle
                .publish_guarded(
                    "device/set",
                    b"payload".to_vec(),
                    Instant::now() + Duration::from_secs(2),
                    accepted,
                )
                .await
                .unwrap();
            received_rx.recv_async().await.unwrap();
            guard.revoke();
            release_tx.send_async(()).await.unwrap();
            complete_rx.recv_async().await.unwrap();
            assert!(guard.may_have_been_sent());
            handle.stop().await;
        };
        let (result, ()) = future::zip(run, app).await;
        result.unwrap();
    });
    broker.join().unwrap();
}

fn subscription_failure(
    return_codes: Vec<v5::SubscribeReasonCode>,
    topics: Vec<String>,
) -> MqttError {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let broker = thread::spawn(move || {
        let mut stream = listener.accept().unwrap().0;
        let mut buffer = BytesMut::new();
        acknowledge_connect(&mut stream, &mut buffer);
        let v5::Packet::Subscribe(subscribe) = packet(&mut stream, &mut buffer) else {
            panic!("expected subscribe")
        };
        write_packet(
            &mut stream,
            &v5::Packet::SubAck(v5::SubAck::new(subscribe.pkid, return_codes)),
        );
        let mut byte = [0];
        let _ = stream.read(&mut byte);
    });
    let error = block_on(async {
        let (connection, handle, _) = MqttConnection::new(config(address)).unwrap();
        let run = connection.run();
        let operation = async {
            let error = handle
                .subscribe(topics, Instant::now() + Duration::from_secs(2))
                .await
                .unwrap_err();
            handle.stop().await;
            error
        };
        let (_, error) = future::zip(run, operation).await;
        error
    });
    broker.join().unwrap();
    error
}

#[test]
fn suback_rejection_and_count_mismatch_are_not_success() {
    let rejected = subscription_failure(
        vec![v5::SubscribeReasonCode::NotAuthorized],
        vec!["a/#".into()],
    );
    assert_eq!(rejected.kind(), &MqttErrorKind::Protocol);
    let mismatch = subscription_failure(
        vec![v5::SubscribeReasonCode::QoS2],
        vec!["a/#".into(), "b/#".into()],
    );
    assert_eq!(mismatch.kind(), &MqttErrorKind::Protocol);
}

#[test]
fn queued_operation_does_not_cancel_an_in_progress_connection_poll() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (connected_tx, connected_rx) = flume::bounded(1);
    let (operation_tx, operation_rx) = flume::bounded(1);
    let (release_tx, release_rx) = flume::bounded(1);
    let broker = thread::spawn(move || {
        let mut stream = listener.accept().unwrap().0;
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut buffer = BytesMut::new();
        assert!(matches!(
            packet(&mut stream, &mut buffer),
            v5::Packet::Connect(_)
        ));
        connected_tx.send(()).unwrap();
        release_rx.recv().unwrap();
        write_packet(
            &mut stream,
            &v5::Packet::ConnAck(v5::ConnAck::new(v5::ConnectReturnCode::Success, false)),
        );
        let v5::Packet::Subscribe(subscribe) = packet(&mut stream, &mut buffer) else {
            panic!("expected subscription on the original connection")
        };
        assert_eq!(subscribe.filters[0].path, "held-connect/#");
        write_packet(
            &mut stream,
            &v5::Packet::SubAck(v5::SubAck::new(
                subscribe.pkid,
                vec![v5::SubscribeReasonCode::QoS2],
            )),
        );
        assert!(matches!(
            packet(&mut stream, &mut buffer),
            v5::Packet::Disconnect(_)
        ));
    });
    block_on(async {
        let completed = future::or(
            async {
                let observer = move || {
                    let _ = operation_tx.try_send(());
                };
                let (connection, handle, _) =
                    MqttConnection::new(config(address).with_operation_observer(observer)).unwrap();
                let run = connection.run();
                let app = async {
                    connected_rx.recv_async().await.unwrap();
                    let mut subscription = Box::pin(handle.subscribe(
                        vec!["held-connect/#".into()],
                        Instant::now() + Duration::from_secs(2),
                    ));
                    assert!(future::poll_once(subscription.as_mut()).await.is_none());
                    operation_rx.recv_async().await.unwrap();
                    release_tx.send_async(()).await.unwrap();
                    subscription.await.unwrap();
                    handle.stop().await;
                };
                let (result, ()) = future::zip(run, app).await;
                result.unwrap();
                true
            },
            async {
                async_io::Timer::after(Duration::from_secs(5)).await;
                false
            },
        )
        .await;
        assert!(completed, "MQTT connection-poll scenario timed out");
    });
    broker.join().unwrap();
}

#[test]
fn an_expired_queued_subscription_does_not_reach_the_broker_or_close_the_session() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (first_tx, first_rx) = flume::bounded(1);
    let (release_tx, release_rx) = flume::bounded(1);
    let broker = thread::spawn(move || {
        let mut stream = listener.accept().unwrap().0;
        let mut buffer = BytesMut::new();
        acknowledge_connect(&mut stream, &mut buffer);
        let v5::Packet::Subscribe(first) = packet(&mut stream, &mut buffer) else {
            panic!("expected first subscription")
        };
        assert_eq!(first.filters[0].path, "first/#");
        first_tx.send(()).unwrap();
        release_rx.recv().unwrap();
        write_packet(
            &mut stream,
            &v5::Packet::SubAck(v5::SubAck::new(
                first.pkid,
                vec![v5::SubscribeReasonCode::QoS2],
            )),
        );
        let v5::Packet::Subscribe(third) = packet(&mut stream, &mut buffer) else {
            panic!("expected third subscription")
        };
        assert_eq!(third.filters[0].path, "third/#");
        write_packet(
            &mut stream,
            &v5::Packet::SubAck(v5::SubAck::new(
                third.pkid,
                vec![v5::SubscribeReasonCode::QoS2],
            )),
        );
        let _ = packet(&mut stream, &mut buffer);
    });
    block_on(async {
        let (connection, handle, _) = MqttConnection::new(config(address)).unwrap();
        let run = connection.run();
        let app = async {
            let mut first = Box::pin(handle.subscribe(
                vec!["first/#".into()],
                Instant::now() + Duration::from_secs(2),
            ));
            assert!(future::poll_once(first.as_mut()).await.is_none());
            first_rx.recv_async().await.unwrap();
            let mut expired = Box::pin(handle.subscribe(
                vec!["expired/#".into()],
                Instant::now() + Duration::from_millis(10),
            ));
            assert!(future::poll_once(expired.as_mut()).await.is_none());
            async_io::Timer::after(Duration::from_millis(20)).await;
            let mut third = Box::pin(handle.subscribe(
                vec!["third/#".into()],
                Instant::now() + Duration::from_secs(2),
            ));
            assert!(future::poll_once(third.as_mut()).await.is_none());
            release_tx.send_async(()).await.unwrap();
            first.await.unwrap();
            assert_eq!(expired.await.unwrap_err().kind(), &MqttErrorKind::Timeout);
            third.await.unwrap();
            handle.stop().await;
        };
        let (result, ()) = future::zip(run, app).await;
        result.unwrap();
    });
    broker.join().unwrap();
}

#[test]
fn incoming_packets_up_to_the_session_limit_are_delivered() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let payload = vec![0x5a; 16 * 1024];
    let expected = payload.clone();
    let broker = thread::spawn(move || {
        let mut stream = listener.accept().unwrap().0;
        let mut buffer = BytesMut::new();
        acknowledge_connect(&mut stream, &mut buffer);
        write_packet(
            &mut stream,
            &v5::Packet::Publish(v5::Publish::new(
                "large/message",
                mqttbytes::QoS::AtMostOnce,
                payload,
            )),
        );
        let _ = packet(&mut stream, &mut buffer);
    });
    block_on(async {
        let (connection, handle, messages) = MqttConnection::new(config(address)).unwrap();
        let run = connection.run();
        let app = async {
            let message = messages.recv_async().await.unwrap();
            assert_eq!(message.topic(), "large/message");
            assert_eq!(message.payload(), expected);
            handle.stop().await;
        };
        let (result, ()) = future::zip(run, app).await;
        result.unwrap();
    });
    broker.join().unwrap();
}

#[test]
fn a_new_instance_does_not_replay_the_old_instances_control() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let broker = thread::spawn(move || {
        let (mut first, _) = listener.accept().unwrap();
        let mut first_buffer = BytesMut::new();
        acknowledge_connect(&mut first, &mut first_buffer);
        let v5::Packet::Publish(old) = packet(&mut first, &mut first_buffer) else {
            panic!("expected old publish")
        };
        assert_eq!(old.topic, "old/control");
        drop(first);
        let (mut second, _) = listener.accept().unwrap();
        let mut second_buffer = BytesMut::new();
        acknowledge_connect(&mut second, &mut second_buffer);
        let v5::Packet::Publish(new) = packet(&mut second, &mut second_buffer) else {
            panic!("expected new publish")
        };
        assert_eq!(new.topic, "new/control");
        write_packet(&mut second, &v5::Packet::PubRec(v5::PubRec::new(new.pkid)));
        assert!(matches!(
            packet(&mut second, &mut second_buffer),
            v5::Packet::PubRel(_)
        ));
        write_packet(
            &mut second,
            &v5::Packet::PubComp(v5::PubComp::new(new.pkid)),
        );
    });
    block_on(async {
        let (first, first_handle, _) = MqttConnection::new(config(address)).unwrap();
        let first_run = first.run();
        let first_app = async {
            first_handle
                .publish(
                    "old/control",
                    vec![1],
                    Instant::now() + Duration::from_secs(1),
                )
                .await
                .unwrap();
        };
        let (error, ()) = future::zip(first_run, first_app).await;
        assert!(error.is_err());
        let (second, second_handle, _) = MqttConnection::new(config(address)).unwrap();
        let second_run = second.run();
        let second_app = async {
            second_handle
                .publish(
                    "new/control",
                    vec![2],
                    Instant::now() + Duration::from_secs(1),
                )
                .await
                .unwrap();
        };
        let (error, ()) = future::zip(second_run, second_app).await;
        assert!(error.is_err());
    });
    broker.join().unwrap();
}

#[test]
fn dropping_run_aborts_the_private_eventloop_task() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (connected_tx, connected_rx) = flume::bounded(1);
    let broker = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buffer = BytesMut::new();
        acknowledge_connect(&mut stream, &mut buffer);
        connected_tx.send(()).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let mut byte = [0];
        loop {
            match stream.read(&mut byte) {
                Ok(0) => break,
                Ok(_) => continue,
                Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => break,
                result => panic!("MQTT connection remained open: {result:?}"),
            }
        }
    });
    block_on(async {
        let (connection, _handle, _) = MqttConnection::new(config(address)).unwrap();
        let mut run = Box::pin(connection.run());
        assert!(future::poll_once(run.as_mut()).await.is_none());
        connected_rx.recv_async().await.unwrap();
        drop(run);
    });
    broker.join().unwrap();
}

#[test]
fn stop_is_bounded_when_the_broker_does_not_read() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (release_tx, release_rx) = flume::bounded(1);
    let broker = thread::spawn(move || {
        let (_stream, _) = listener.accept().unwrap();
        release_rx.recv().unwrap();
    });
    let started = Instant::now();
    block_on(async {
        let (connection, handle, _) = MqttConnection::new(config(address)).unwrap();
        let run = connection.run();
        let app = async {
            async_io::Timer::after(Duration::from_millis(20)).await;
            handle.stop().await;
        };
        let (result, ()) = future::zip(run, app).await;
        result.unwrap();
    });
    assert!(started.elapsed() < Duration::from_secs(2));
    release_tx.send(()).unwrap();
    broker.join().unwrap();
}

#[test]
fn production_gateway_tls_accepts_chain_only_hostname_with_mtls_and_rejects_wrong_ca() {
    let material = tls_material("unrelated.gateway.test");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (connected_tx, connected_rx) = flume::bounded(1);
    let server = spawn_tls_broker(
        listener,
        tls_server_config(&material, true),
        true,
        Some(connected_tx),
    );
    let tls = GatewayTlsConfig::new(
        &material.ca_pem,
        &material.client_pem,
        &material.client_key_pem,
    )
    .unwrap();
    block_on(async {
        let (connection, handle, _) =
            MqttConnection::new(config(address).with_tls(tls.client_config())).unwrap();
        let run = connection.run();
        let app = async {
            handle
                .subscribe(
                    vec!["probe/#".into()],
                    Instant::now() + Duration::from_secs(2),
                )
                .await
                .unwrap();
            connected_rx.recv_async().await.unwrap();
            handle.stop().await;
        };
        let (result, ()) = future::zip(run, app).await;
        result.unwrap();
    });
    server.join().unwrap();

    let untrusted = tls_material("untrusted.gateway.test");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = spawn_tls_broker(listener, tls_server_config(&untrusted, false), false, None);
    let tls = GatewayTlsConfig::new(
        &material.ca_pem,
        &material.client_pem,
        &material.client_key_pem,
    )
    .unwrap();
    let error = block_on(async {
        let (connection, _, _) =
            MqttConnection::new(config(address).with_tls(tls.client_config())).unwrap();
        connection.run().await.unwrap_err()
    });
    assert_eq!(error.kind(), &MqttErrorKind::Network);
    server.join().unwrap();
}

#[test]
fn production_cloud_tls_rejects_a_hostname_mismatch() {
    let material = tls_material("wrong.test");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = spawn_tls_broker(listener, tls_server_config(&material, false), false, None);
    let tls = CloudTlsConfig::with_ca_pem("localhost", &material.ca_pem).unwrap();
    let error = block_on(async {
        let config = MqttConfig::new("cloud", None, Duration::from_secs(5))
            .with_endpoint("localhost", address.port())
            .with_tls(tls.client_config_for("localhost").unwrap());
        let (connection, _, _) = MqttConnection::new(config).unwrap();
        connection.run().await.unwrap_err()
    });
    assert_eq!(error.kind(), &MqttErrorKind::Network);
    server.join().unwrap();
}

#[test]
fn production_cloud_tls_connects_when_hostname_and_ca_match() {
    let material = tls_material("localhost");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (connected_tx, connected_rx) = flume::bounded(1);
    let server = spawn_tls_broker(
        listener,
        tls_server_config(&material, false),
        true,
        Some(connected_tx),
    );
    let tls = CloudTlsConfig::with_ca_pem("localhost", &material.ca_pem).unwrap();
    block_on(async {
        let config = MqttConfig::new("cloud", None, Duration::from_secs(5))
            .with_endpoint("localhost", address.port())
            .with_tls(tls.client_config_for("localhost").unwrap());
        let (connection, handle, _) = MqttConnection::new(config).unwrap();
        let run = connection.run();
        let app = async {
            handle
                .subscribe(
                    vec!["probe/#".into()],
                    Instant::now() + Duration::from_secs(2),
                )
                .await
                .unwrap();
            connected_rx.recv_async().await.unwrap();
            handle.stop().await;
        };
        let (result, ()) = future::zip(run, app).await;
        result.unwrap();
    });
    server.join().unwrap();
}
