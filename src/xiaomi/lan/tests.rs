use super::{
    LanDiscovery, LanErrorKind, LanEventArguments, LanNotification, LanProperty, LanPropertyWrite,
    LanReadOutcome, LanSendGuard, LanSession, LanTarget, LanWriteOutcome,
    packet::{LEGACY_PROBE, decode_packet, encode_packet, native_probe},
};
use crate::device::DeviceCommand;
use crate::xiaomi::{
    catalog::WireValue,
    discovery::{NetworkEpoch, NetworkInterface},
};
use async_io::{Async, Timer};
use futures_lite::future;
use serde_json::{Value, json};
use std::{
    cell::Cell,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

fn decode_hex(value: &str) -> Vec<u8> {
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

#[test]
fn packet_codec_matches_independent_openssl_golden_vector() {
    let vector: serde_json::Value = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/miio-golden-vector.json"
    )))
    .unwrap();
    let token: [u8; 16] = decode_hex(vector["token_hex"].as_str().unwrap())
        .try_into()
        .unwrap();
    let packet = encode_packet(
        vector["did"].as_u64().unwrap(),
        vector["timestamp"].as_u64().unwrap() as u32,
        &token,
        vector["plaintext"].as_str().unwrap().as_bytes(),
    )
    .unwrap();
    assert_eq!(packet, decode_hex(vector["packet_hex"].as_str().unwrap()));
    let decoded = decode_packet(&packet, vector["did"].as_u64().unwrap(), &token).unwrap();
    assert_eq!(
        decoded.timestamp,
        vector["timestamp"].as_u64().unwrap() as u32
    );
    assert_eq!(decoded.message["id"], 173);
}

#[test]
fn packet_codec_rejects_tamper_wrong_identity_length_padding_and_json() {
    let token = [7_u8; 16];
    let packet = encode_packet(42, 10, &token, br#"{"id":1}"#).unwrap();
    let mut tampered = packet.clone();
    *tampered.last_mut().unwrap() ^= 1;
    assert!(decode_packet(&tampered, 42, &token).is_err());
    assert!(decode_packet(&packet, 42, &[8; 16]).is_err());
    assert!(decode_packet(&packet, 43, &token).is_err());
    let mut wrong_length = packet.clone();
    wrong_length[3] -= 1;
    assert!(decode_packet(&wrong_length, 42, &token).is_err());

    let invalid_json = encode_packet(42, 10, &token, b"not-json").unwrap();
    assert!(decode_packet(&invalid_json, 42, &token).is_err());
    assert_eq!(native_probe(9)[16..28], *b"MDID\0\0\0\0\0\0\0\t");
    assert_eq!(&LEGACY_PROBE[..4], &[0x21, 0x31, 0, 32]);
}

#[test]
fn packet_codec_rejects_independent_valid_checksum_invalid_padding_vector() {
    let vector: Value = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/miio-invalid-padding-vector.json"
    )))
    .unwrap();
    let token: [u8; 16] = decode_hex(vector["token_hex"].as_str().unwrap())
        .try_into()
        .unwrap();
    let packet = decode_hex(vector["packet_hex"].as_str().unwrap());
    assert!(decode_packet(&packet, vector["did"].as_u64().unwrap(), &token).is_err());
}

fn loopback_interface() -> NetworkInterface {
    NetworkInterface {
        index: 1,
        name: "test-loopback".into(),
        address: Ipv4Addr::LOCALHOST,
        netmask: Ipv4Addr::new(255, 0, 0, 0),
        prefix_len: 8,
    }
}

fn socket() -> Async<UdpSocket> {
    let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    socket.set_nonblocking(true).unwrap();
    Async::new(socket).unwrap()
}

pub(crate) fn session_pair(
    model: &str,
) -> (
    LanSession,
    super::LanHandle,
    flume::Receiver<LanNotification>,
    Async<UdpSocket>,
    [u8; 16],
) {
    let device = socket();
    let address = match device.get_ref().local_addr().unwrap() {
        SocketAddr::V4(address) => address,
        SocketAddr::V6(_) => unreachable!(),
    };
    let token = [0x31; 16];
    let target = LanTarget::for_test(
        42,
        model,
        address,
        loopback_interface(),
        NetworkEpoch::new(7),
        token,
    );
    let client = socket();
    let (session, handle, events) = LanSession::for_test(target, 99, client).unwrap();
    (session, handle, events, device, token)
}

fn session_pair_requiring_hello() -> (
    LanSession,
    super::LanHandle,
    flume::Receiver<LanNotification>,
    Async<UdpSocket>,
    [u8; 16],
) {
    let device = socket();
    let address = match device.get_ref().local_addr().unwrap() {
        SocketAddr::V4(address) => address,
        SocketAddr::V6(_) => unreachable!(),
    };
    let token = [0x31; 16];
    let target = LanTarget::for_test(
        42,
        "test.device",
        address,
        loopback_interface(),
        NetworkEpoch::new(7),
        token,
    );
    let client = socket();
    let (session, handle, events) =
        LanSession::for_test_requiring_hello(target, 99, client).unwrap();
    (session, handle, events, device, token)
}

pub(crate) async fn receive_request(
    device: &Async<UdpSocket>,
    token: &[u8; 16],
) -> (Value, SocketAddrV4) {
    let mut buffer = [0_u8; 1401];
    let (length, source) = device.recv_from(&mut buffer).await.unwrap();
    let source = match source {
        SocketAddr::V4(source) => source,
        SocketAddr::V6(_) => unreachable!(),
    };
    (
        decode_packet(&buffer[..length], 42, token).unwrap().message,
        source,
    )
}

pub(crate) async fn reply(
    device: &Async<UdpSocket>,
    token: &[u8; 16],
    source: SocketAddrV4,
    timestamp: u32,
    value: Value,
) {
    let packet = encode_packet(42, timestamp, token, value.to_string().as_bytes()).unwrap();
    device.send_to(&packet, source).await.unwrap();
}

pub(crate) async fn reject_native_authentication_probe(
    device: &Async<UdpSocket>,
    token: &[u8; 16],
) {
    let (request, source) = receive_request(device, token).await;
    assert_eq!(request["method"], "get_properties");
    reply(
        device,
        token,
        source,
        40,
        json!({"id":request["id"],"error":{"code":-1}}),
    )
    .await;
}

#[test]
fn authenticated_session_uses_exact_native_envelopes_and_push_lifecycle() {
    let (session, handle, events, device, token) = session_pair("test.device");
    let application = async {
        let deadline = || Instant::now() + Duration::from_secs(1);
        let evidence = handle
            .authenticate(
                LanProperty { siid: 2, piid: 1 },
                deadline(),
                LanSendGuard::new(),
            )
            .await
            .unwrap();
        assert!(evidence.native_supported);
        assert_eq!(evidence.did, 42);
        assert_eq!(evidence.epoch, NetworkEpoch::new(7));
        assert_eq!(evidence.signed_timestamp, 100);

        let writes = handle
            .set_properties(
                &[LanPropertyWrite {
                    property: LanProperty { siid: 2, piid: 2 },
                    value: WireValue::Integer(7),
                }],
                deadline(),
                LanSendGuard::new(),
            )
            .await
            .unwrap();
        assert_eq!(writes, vec![LanWriteOutcome::Accepted]);
        handle
            .invoke_action(
                2,
                3,
                &[WireValue::Integer(9)],
                deadline(),
                LanSendGuard::new(),
            )
            .await
            .unwrap();
        let subscription = handle
            .subscribe(deadline(), LanSendGuard::new())
            .await
            .unwrap();
        assert_ne!(subscription.generation, 0);
        let event = events.recv_async().await.unwrap();
        assert_eq!(
            event,
            LanNotification::Event {
                did: 42,
                siid: 4,
                eiid: 5,
                arguments: LanEventArguments::Keyed(vec![super::LanEventArgument {
                    piid: 6,
                    value: WireValue::Integer(8),
                }]),
                epoch: NetworkEpoch::new(7),
                generation: subscription.generation,
                timestamp: 104,
            }
        );
        Timer::after(Duration::from_millis(20)).await;
        handle
            .unsubscribe(&subscription, deadline(), LanSendGuard::new())
            .await
            .unwrap();
        handle.stop();
    };
    let simulated_device = async {
        let (auth, source) = receive_request(&device, &token).await;
        assert_eq!(auth["method"], "get_properties");
        assert_eq!(auth["params"][0], json!({"did":"42","siid":2,"piid":1}));
        reply(
            &device,
            &token,
            source,
            100,
            json!({"id":auth["id"],"result":[{"did":"42","siid":2,"piid":1,"code":0,"value":true}]}),
        )
        .await;

        let (set, _) = receive_request(&device, &token).await;
        assert_eq!(set["method"], "set_properties");
        assert_eq!(set["params"][0]["value"], 7);
        reply(
            &device,
            &token,
            source,
            101,
            json!({"id":set["id"],"result":[{"did":"42","siid":2,"piid":2,"code":0}]}),
        )
        .await;

        let (action, _) = receive_request(&device, &token).await;
        assert_eq!(action["method"], "action");
        assert_eq!(
            action["params"],
            json!({"did":"42","siid":2,"aiid":3,"in":[9]})
        );
        reply(
            &device,
            &token,
            source,
            102,
            json!({"id":action["id"],"result":{"code":0}}),
        )
        .await;

        let (subscribe, _) = receive_request(&device, &token).await;
        assert_eq!(subscribe["method"], "miIO.sub");
        assert_eq!(subscribe["params"]["did"], "99");
        reply(
            &device,
            &token,
            source,
            103,
            json!({"id":subscribe["id"],"result":{"code":0}}),
        )
        .await;
        let push = json!({"id":900,"method":"event_occured","params":{"did":"42","siid":4,"eiid":5,"arguments":[{"piid":6,"value":8}]}});
        reply(&device, &token, source, 104, push.clone()).await;
        let (ack, _) = receive_request(&device, &token).await;
        assert_eq!(ack, json!({"id":900,"result":{"code":0}}));
        reply(&device, &token, source, 104, push).await;
        let (duplicate_ack, _) = receive_request(&device, &token).await;
        assert_eq!(duplicate_ack["id"], 900);

        let (unsubscribe, _) = receive_request(&device, &token).await;
        assert_eq!(unsubscribe["method"], "miIO.unsub");
        assert_eq!(
            unsubscribe["params"]["update_ts"],
            subscribe["params"]["update_ts"]
        );
        reply(
            &device,
            &token,
            source,
            105,
            json!({"id":unsubscribe["id"],"result":{"code":0}}),
        )
        .await;
    };
    future::block_on(future::zip(
        session.run(),
        future::zip(application, simulated_device),
    ))
    .0
    .unwrap();
}

#[test]
fn catalog_address_authentication_first_establishes_hello_clock_and_reports_msub_hint() {
    let (session, handle, events, device, token) = session_pair_requiring_hello();
    let application = async {
        let evidence = handle
            .authenticate(
                LanProperty { siid: 2, piid: 1 },
                Instant::now() + Duration::from_secs(1),
                LanSendGuard::new(),
            )
            .await
            .unwrap();
        assert_eq!(evidence.signed_timestamp, 501);
        let hint = events.recv_async().await.unwrap();
        assert_eq!(
            hint,
            LanNotification::SubscriptionHint {
                did: 42,
                hint: super::LanSubscriptionHint {
                    subscription_timestamp: 77,
                    subscription_type: 4,
                    wildcard_supported: true,
                },
                epoch: NetworkEpoch::new(7),
            }
        );
        handle.stop();
    };
    let simulated_device = async {
        let mut buffer = [0_u8; 1401];
        let (first_length, source) = device.recv_from(&mut buffer).await.unwrap();
        assert_eq!(first_length, 32);
        assert_eq!(&buffer[16..20], b"MDID");
        let (second_length, second_source) = device.recv_from(&mut buffer).await.unwrap();
        assert_eq!(second_length, 32);
        assert_eq!(source, second_source);
        assert_eq!(&buffer[..32], &LEGACY_PROBE);
        let mut hello = [0_u8; 32];
        hello[..4].copy_from_slice(&[0x21, 0x31, 0, 32]);
        hello[4..12].copy_from_slice(&42_u64.to_be_bytes());
        hello[12..16].copy_from_slice(&500_u32.to_be_bytes());
        device.send_to(&hello, source).await.unwrap();
        let (length, source) = device.recv_from(&mut buffer).await.unwrap();
        let source = match source {
            SocketAddr::V4(source) => source,
            SocketAddr::V6(_) => unreachable!(),
        };
        let signed = decode_packet(&buffer[..length], 42, &token).unwrap();
        assert_eq!(signed.timestamp, 500);
        let request = signed.message;
        reply(&device, &token, source, 501, json!({"id":request["id"],"result":[{"did":"42","siid":2,"piid":1,"code":0,"value":true}]})).await;
        let mut hint = hello;
        hint[12..16].copy_from_slice(&502_u32.to_be_bytes());
        hint[16..20].copy_from_slice(b"MSUB");
        hint[20..24].copy_from_slice(&77_u32.to_be_bytes());
        hint[24..27].copy_from_slice(b"PUB");
        hint[27] = 4;
        hint[28] = 1;
        device.send_to(&hint, source).await.unwrap();
    };
    future::block_on(future::zip(
        session.run(),
        future::zip(application, simulated_device),
    ))
    .0
    .unwrap();
}

#[test]
fn authentication_uses_one_total_deadline_for_hello_and_signed_request() {
    let (session, handle, _events, device, token) = session_pair_requiring_hello();
    let application = async {
        let started = Instant::now();
        let error = handle
            .authenticate_with_limit_for_test(
                LanProperty { siid: 2, piid: 1 },
                Instant::now() + Duration::from_secs(1),
                LanSendGuard::new(),
                Duration::from_millis(80),
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind(), &LanErrorKind::Timeout);
        assert!(error.may_have_been_sent());
        assert!(started.elapsed() < Duration::from_millis(200));
        handle.stop();
    };
    let simulated_device = async {
        let mut buffer = [0_u8; 1401];
        let (_, source) = device.recv_from(&mut buffer).await.unwrap();
        let (_, second_source) = device.recv_from(&mut buffer).await.unwrap();
        assert_eq!(source, second_source);
        Timer::after(Duration::from_millis(25)).await;
        let mut hello = [0_u8; 32];
        hello[..4].copy_from_slice(&[0x21, 0x31, 0, 32]);
        hello[4..12].copy_from_slice(&42_u64.to_be_bytes());
        hello[12..16].copy_from_slice(&500_u32.to_be_bytes());
        device.send_to(&hello, source).await.unwrap();

        let (length, source) = device.recv_from(&mut buffer).await.unwrap();
        let source = match source {
            SocketAddr::V4(source) => source,
            SocketAddr::V6(_) => unreachable!(),
        };
        let request = decode_packet(&buffer[..length], 42, &token)
            .unwrap()
            .message;
        Timer::after(Duration::from_millis(80)).await;
        reply(
            &device,
            &token,
            source,
            501,
            json!({"id":request["id"],"result":[{"did":"42","siid":2,"piid":1,"code":0,"value":true}]}),
        )
        .await;
        assert_eq!(
            device
                .get_ref()
                .recv_from(&mut [0_u8; 1401])
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::WouldBlock
        );
    };
    future::block_on(future::zip(
        session.run(),
        future::zip(application, simulated_device),
    ))
    .0
    .unwrap();
}

#[test]
fn stale_push_requires_reauthentication_and_delivered_subscribe_keeps_its_ack() {
    let (session, handle, events, device, token) = session_pair("test.device");
    let (received_sub, received_sub_rx) = flume::bounded(1);
    let guard = LanSendGuard::new();
    let application = async {
        handle
            .authenticate(
                LanProperty { siid: 2, piid: 1 },
                Instant::now() + Duration::from_secs(1),
                LanSendGuard::new(),
            )
            .await
            .unwrap();
        let cancel = guard.clone();
        let (subscription, ()) = future::zip(
            handle.subscribe(Instant::now() + Duration::from_secs(1), guard),
            async {
                received_sub_rx.recv_async().await.unwrap();
                cancel.revoke();
            },
        )
        .await;
        subscription.unwrap();
        Timer::after(Duration::from_millis(40)).await;
        assert!(events.try_recv().is_err());
        let read = handle
            .read_properties(
                &[LanProperty { siid: 2, piid: 1 }],
                Instant::now() + Duration::from_millis(100),
                LanSendGuard::new(),
            )
            .await;
        assert!(
            read.is_ok(),
            "cancelled subscribe must retain authenticated control"
        );
        handle.stop();
    };
    let simulated_device = async {
        let (auth, source) = receive_request(&device, &token).await;
        reply(&device, &token, source, 100, json!({"id":auth["id"],"result":[{"did":"42","siid":2,"piid":1,"code":0,"value":true}]})).await;
        let (subscribe, _) = receive_request(&device, &token).await;
        received_sub.send_async(()).await.unwrap();
        Timer::after(Duration::from_millis(20)).await;
        reply(
            &device,
            &token,
            source,
            101,
            json!({"id":subscribe["id"],"result":{"code":0}}),
        )
        .await;
        reply(&device, &token, source, 102, json!({"id":901,"method":"properties_changed","params":[{"did":"42","siid":2,"piid":1,"value":false}]})).await;
        let (read, _) = receive_request(&device, &token).await;
        reply(&device, &token, source, 103, json!({"id":read["id"],"result":[{"did":"42","siid":2,"piid":1,"code":0,"value":true}]})).await;
    };
    future::block_on(future::zip(
        session.run(),
        future::zip(application, simulated_device),
    ))
    .0
    .unwrap();

    let (session, handle, events, device, token) = session_pair("test.device");
    let application = async {
        handle
            .authenticate(
                LanProperty { siid: 2, piid: 1 },
                Instant::now() + Duration::from_secs(1),
                LanSendGuard::new(),
            )
            .await
            .unwrap();
        handle
            .subscribe(Instant::now() + Duration::from_secs(1), LanSendGuard::new())
            .await
            .unwrap();
        Timer::after(Duration::from_millis(30)).await;
        assert!(events.try_recv().is_err());
        let error = handle
            .read_properties(
                &[LanProperty { siid: 2, piid: 1 }],
                Instant::now() + Duration::from_millis(100),
                LanSendGuard::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind(), &LanErrorKind::NotAuthenticated);
        let evidence = handle
            .authenticate(
                LanProperty { siid: 2, piid: 1 },
                Instant::now() + Duration::from_secs(1),
                LanSendGuard::new(),
            )
            .await
            .unwrap();
        assert_eq!(evidence.signed_timestamp, 11);
        handle.stop();
    };
    let simulated_device = async {
        let (auth, source) = receive_request(&device, &token).await;
        reply(&device, &token, source, 100, json!({"id":auth["id"],"result":[{"did":"42","siid":2,"piid":1,"code":0,"value":true}]})).await;
        let (subscribe, _) = receive_request(&device, &token).await;
        reply(
            &device,
            &token,
            source,
            101,
            json!({"id":subscribe["id"],"result":{"code":0}}),
        )
        .await;
        reply(&device, &token, source, 50, json!({"id":902,"method":"properties_changed","params":[{"did":"42","siid":2,"piid":1,"value":false}]})).await;
        let mut buffer = [0_u8; 1401];
        let (_, hello_source) = device.recv_from(&mut buffer).await.unwrap();
        device.recv_from(&mut buffer).await.unwrap();
        let mut hello = [0_u8; 32];
        hello[..4].copy_from_slice(&[0x21, 0x31, 0, 32]);
        hello[4..12].copy_from_slice(&42_u64.to_be_bytes());
        hello[12..16].copy_from_slice(&10_u32.to_be_bytes());
        device.send_to(&hello, hello_source).await.unwrap();
        let (reauth, source) = receive_request(&device, &token).await;
        reply(&device, &token, source, 11, json!({"id":reauth["id"],"result":[{"did":"42","siid":2,"piid":1,"code":0,"value":true}]})).await;
    };
    future::block_on(future::zip(
        session.run(),
        future::zip(application, simulated_device),
    ))
    .0
    .unwrap();
}

#[test]
fn stale_unsubscribe_ack_cannot_clear_newer_subscription_generation() {
    let (session, handle, events, device, token) = session_pair("test.device");
    let application = async {
        handle
            .authenticate(
                LanProperty { siid: 2, piid: 1 },
                Instant::now() + Duration::from_secs(1),
                LanSendGuard::new(),
            )
            .await
            .unwrap();
        let first = handle
            .subscribe(Instant::now() + Duration::from_secs(1), LanSendGuard::new())
            .await
            .unwrap();
        let unsubscribe = handle.unsubscribe(
            &first,
            Instant::now() + Duration::from_secs(1),
            LanSendGuard::new(),
        );
        let subscribe =
            handle.subscribe(Instant::now() + Duration::from_secs(1), LanSendGuard::new());
        let (unsubscribed, second) = future::zip(unsubscribe, subscribe).await;
        unsubscribed.unwrap();
        let second = second.unwrap();
        assert_ne!(first.generation, second.generation);
        let event = events.recv_async().await.unwrap();
        assert!(
            matches!(event, LanNotification::Property { generation, .. } if generation == second.generation)
        );
        handle.stop();
    };
    let simulated_device = async {
        let (auth, source) = receive_request(&device, &token).await;
        reply(&device, &token, source, 100, json!({"id":auth["id"],"result":[{"did":"42","siid":2,"piid":1,"code":0,"value":true}]})).await;
        let (first, _) = receive_request(&device, &token).await;
        reply(
            &device,
            &token,
            source,
            101,
            json!({"id":first["id"],"result":{"code":0}}),
        )
        .await;
        let (old_unsubscribe, _) = receive_request(&device, &token).await;
        let (new_subscribe, _) = receive_request(&device, &token).await;
        assert_eq!(old_unsubscribe["method"], "miIO.unsub");
        assert_eq!(new_subscribe["method"], "miIO.sub");
        assert!(
            new_subscribe["params"]["update_ts"].as_u64().unwrap()
                > old_unsubscribe["params"]["update_ts"].as_u64().unwrap()
        );
        reply(
            &device,
            &token,
            source,
            102,
            json!({"id":new_subscribe["id"],"result":{"code":0}}),
        )
        .await;
        reply(
            &device,
            &token,
            source,
            103,
            json!({"id":old_unsubscribe["id"],"result":{"code":0}}),
        )
        .await;
        reply(&device, &token, source, 104, json!({"id":990,"method":"properties_changed","params":[{"did":"42","siid":2,"piid":1,"value":true}]})).await;
        let (ack, _) = receive_request(&device, &token).await;
        assert_eq!(ack["id"], 990);
    };
    future::block_on(future::zip(
        session.run(),
        future::zip(application, simulated_device),
    ))
    .0
    .unwrap();
}

#[test]
fn revoked_subscription_authority_blocks_delivery_and_ack() {
    let (session, handle, events, device, token) = session_pair("test.device");
    let authority = LanSendGuard::new();
    let (revoked, revoked_rx) = flume::bounded(1);
    let application = async {
        handle
            .authenticate(
                LanProperty { siid: 2, piid: 1 },
                Instant::now() + Duration::from_secs(1),
                LanSendGuard::new(),
            )
            .await
            .unwrap();
        handle
            .subscribe(Instant::now() + Duration::from_secs(1), authority.clone())
            .await
            .unwrap();
        authority.revoke();
        revoked.send_async(()).await.unwrap();
        Timer::after(Duration::from_millis(30)).await;
        assert!(events.try_recv().is_err());
        handle.stop();
    };
    let simulated_device = async {
        let (auth, source) = receive_request(&device, &token).await;
        reply(&device, &token, source, 100, json!({"id":auth["id"],"result":[{"did":"42","siid":2,"piid":1,"code":0,"value":true}]})).await;
        let (subscribe, _) = receive_request(&device, &token).await;
        reply(
            &device,
            &token,
            source,
            101,
            json!({"id":subscribe["id"],"result":{"code":0}}),
        )
        .await;
        revoked_rx.recv_async().await.unwrap();
        reply(&device, &token, source, 102, json!({"id":991,"method":"properties_changed","params":[{"did":"42","siid":2,"piid":1,"value":true}]})).await;
        Timer::after(Duration::from_millis(20)).await;
        assert_eq!(
            device
                .get_ref()
                .recv_from(&mut [0_u8; 1401])
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::WouldBlock
        );
    };
    future::block_on(future::zip(
        session.run(),
        future::zip(application, simulated_device),
    ))
    .0
    .unwrap();
}

#[test]
fn signed_error_authenticates_but_does_not_claim_native_support() {
    let (session, handle, _events, device, token) = session_pair("test.device");
    let application = async {
        let evidence = handle
            .authenticate(
                LanProperty { siid: 2, piid: 1 },
                Instant::now() + Duration::from_secs(1),
                LanSendGuard::new(),
            )
            .await
            .unwrap();
        assert!(!evidence.native_supported);
        handle.stop();
    };
    let simulated_device = async {
        let (request, source) = receive_request(&device, &token).await;
        reply(
            &device,
            &token,
            source,
            11,
            json!({"id":request["id"],"error":{"code":-1}}),
        )
        .await;
    };
    future::block_on(future::zip(
        session.run(),
        future::zip(application, simulated_device),
    ))
    .0
    .unwrap();
}

#[test]
fn legacy_mcn02_uses_only_verified_traditional_shape() {
    let (session, handle, _events, device, token) = session_pair("lumi.acpartner.mcn02");
    let application = async {
        handle
            .authenticate(
                LanProperty { siid: 2, piid: 1 },
                Instant::now() + Duration::from_secs(1),
                LanSendGuard::new(),
            )
            .await
            .unwrap();
        handle
            .execute_mcn02(
                &DeviceCommand::SetPower(true),
                Instant::now() + Duration::from_secs(1),
                LanSendGuard::new(),
            )
            .await
            .unwrap();
        let values = handle
            .read_mcn02(Instant::now() + Duration::from_secs(1), LanSendGuard::new())
            .await
            .unwrap();
        assert_eq!(values.len(), 5);
        assert_eq!(
            values[0],
            (
                crate::device::Property::Power,
                Some(crate::device::PropertyValue::Power(true))
            )
        );
        assert_eq!(
            values[1],
            (
                crate::device::Property::HvacMode,
                Some(crate::device::PropertyValue::HvacMode(
                    crate::device::HvacMode::Cool
                ))
            )
        );
        assert_eq!(
            values[2],
            (
                crate::device::Property::TargetTemperature,
                Some(crate::device::PropertyValue::Temperature(25.0))
            )
        );
        assert_eq!(
            values[3],
            (
                crate::device::Property::FanSpeed,
                Some(crate::device::PropertyValue::FanSpeed(1))
            )
        );
        assert_eq!(
            values[4],
            (
                crate::device::Property::SwingMode,
                Some(crate::device::PropertyValue::SwingMode(
                    crate::device::SwingMode::Off
                ))
            )
        );
        handle.stop();
    };
    let simulated_device = async {
        let (auth, source) = receive_request(&device, &token).await;
        reply(
            &device,
            &token,
            source,
            30,
            json!({"id":auth["id"],"error":{"code":-1}}),
        )
        .await;
        let (legacy, _) = receive_request(&device, &token).await;
        assert_eq!(legacy["method"], "set_power");
        assert_eq!(legacy["params"], json!(["on"]));
        reply(
            &device,
            &token,
            source,
            31,
            json!({"id":legacy["id"],"result":["ok"]}),
        )
        .await;
        let (read, _) = receive_request(&device, &token).await;
        assert_eq!(read["method"], "get_prop");
        assert_eq!(
            read["params"],
            json!(["power", "mode", "tar_temp", "fan_level", "ver_swing"])
        );
        reply(
            &device,
            &token,
            source,
            32,
            json!({"id":read["id"],"result":["on","cool",25,"small_fan","off"]}),
        )
        .await;
    };
    future::block_on(future::zip(
        session.run(),
        future::zip(application, simulated_device),
    ))
    .0
    .unwrap();
}

#[test]
fn mcn02_udp_runtime_transport_updates_state_and_sends_one_typed_command() {
    use crate::{
        device::{
            AccountId, CommandOutcome, DeviceDid, DeviceService, FeatureIdentity, FeatureRole,
            HomeId, HvacMode, PhysicalDeviceId, Property, PropertyState, PropertyValue, SwingMode,
        },
        storage::{Store, TokenSet, XiaomiRecord},
        xiaomi::{
            catalog::compile_spec,
            runtime::{
                AdmissionFeature, AdmissionSnapshot, AdmissionStatus, CommandLimits,
                CommandRuntime, CurrentSessionRegistry, OperationPaths, RouteFailure,
                RuntimeFeature, RuntimeTransports, SessionAuthority, StateRuntime,
            },
        },
    };

    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    store
        .xiaomi()
        .replace(&XiaomiRecord {
            uid: "10001".into(),
            region: "cn".into(),
            oauth_client_uuid: "123e4567-e89b-12d3-a456-426614174000".into(),
            redirect_uri: "http://127.0.0.1/callback".into(),
            tokens: TokenSet {
                access_token: "access".into(),
                refresh_token: "refresh".into(),
                expires_at: 2_000_000_000,
                refresh_at: 1_900_000_000,
            },
            virtual_did: "123456789012345".into(),
            private_key_pem: "key".into(),
            certificate_pem: "certificate".into(),
        })
        .unwrap();
    let auth = store.xiaomi().snapshot().unwrap();
    let descriptor = compile_spec(
        "lumi.acpartner.mcn02",
        include_str!("../../../tests/fixtures/miot_specs/lumi.acpartner.mcn02.json"),
    )
    .unwrap()
    .features
    .into_iter()
    .find(|feature| feature.role == FeatureRole::Climate)
    .unwrap();
    let identity = FeatureIdentity {
        physical: PhysicalDeviceId {
            account: AccountId::new("10001").unwrap(),
            home: HomeId::new("home-a").unwrap(),
            parent_did: DeviceDid::new("42").unwrap(),
        },
        service_instance: descriptor.service_instance,
        role: descriptor.role,
    };
    store.devices().allocate_feature(&identity).unwrap();
    let service = DeviceService::new();
    service.publish(
        identity.clone(),
        "Loopback air conditioner",
        descriptor.capabilities.clone(),
    );
    let registry = CurrentSessionRegistry::default();
    let transports = Rc::new(RuntimeTransports::new(registry.clone()));
    let commands = CommandRuntime::with_limits(
        service.clone(),
        transports.clone(),
        CommandLimits {
            total: Duration::from_millis(150),
            local_attempt: Duration::from_millis(100),
            ..CommandLimits::default()
        },
    );
    let state = StateRuntime::new(
        service.clone(),
        store.devices(),
        transports,
        commands.execution_gate(),
        commands.subscribe_completions(),
    );
    let runtime_feature = RuntimeFeature {
        identity: identity.clone(),
        descriptor: descriptor.clone(),
        authority_generation: 1,
        auth_session_generation: auth.session_generation,
    };
    commands.register(runtime_feature.clone());

    let (session, handle, _events, device, token) = session_pair("lumi.acpartner.mcn02");
    let application = async {
        let evidence = handle
            .authenticate(
                LanProperty { siid: 2, piid: 1 },
                Instant::now() + Duration::from_secs(1),
                LanSendGuard::new(),
            )
            .await
            .unwrap();
        registry.install_lan(
            identity.physical.clone(),
            handle.clone(),
            SessionAuthority::new(),
            Some(descriptor.clone()),
        );
        state.reconcile(&AdmissionSnapshot {
            binding: None,
            status: AdmissionStatus::Active,
            epoch: NetworkEpoch::new(7),
            features: vec![AdmissionFeature {
                identity: identity.clone(),
                runtime: runtime_feature,
                paths: OperationPaths {
                    lan: true,
                    ..OperationPaths::default()
                },
                gateways: Vec::new(),
                lan_evidence: Some(evidence),
            }],
        });
        state.run_until_idle().await;

        let snapshot = service.snapshot(&identity).unwrap();
        let current = |property| match snapshot.property(property) {
            Some(PropertyState::Current { value, .. }) => value.clone(),
            other => panic!("expected current {property:?}, got {other:?}"),
        };
        assert_eq!(current(Property::Power), PropertyValue::Power(true));
        assert_eq!(
            current(Property::HvacMode),
            PropertyValue::HvacMode(HvacMode::Cool)
        );
        assert_eq!(
            current(Property::TargetTemperature),
            PropertyValue::Temperature(25.0)
        );
        assert_eq!(current(Property::FanSpeed), PropertyValue::FanSpeed(1));
        assert_eq!(
            current(Property::SwingMode),
            PropertyValue::SwingMode(SwingMode::Off)
        );

        let command = service.command(&identity, DeviceCommand::SetPower(false));
        commands.run_until_idle().await;
        assert_eq!(command.await, CommandOutcome::Accepted);
        assert!(
            matches!(
                service
                    .snapshot(&identity)
                    .unwrap()
                    .property(Property::Power),
                Some(PropertyState::Current {
                    value: PropertyValue::Power(true),
                    ..
                })
            ),
            "accepted commands must not optimistically overwrite current state"
        );

        Timer::after(Duration::from_millis(50)).await;
        let failed = service.command(&identity, DeviceCommand::SetPower(true));
        commands.run_until_idle().await;
        let failed = failed.await;
        assert!(
            matches!(
                failed,
                CommandOutcome::Expired | CommandOutcome::Unavailable | CommandOutcome::Ambiguous
            ),
            "unexpected failed outcome: {failed:?}"
        );
        let failures = registry.drain_route_failures();
        assert!(matches!(
            failures.as_slice(),
            [RouteFailure::Lan { device, .. }] if device == &identity.physical
        ));
        handle.stop();
    };
    let simulated_device = async {
        let (auth_request, source) = receive_request(&device, &token).await;
        reply(
            &device,
            &token,
            source,
            40,
            json!({"id":auth_request["id"],"error":{"code":-1}}),
        )
        .await;

        let (read, _) = receive_request(&device, &token).await;
        assert_eq!(read["method"], "get_prop");
        assert_eq!(
            read["params"],
            json!(["power", "mode", "tar_temp", "fan_level", "ver_swing"])
        );
        reply(
            &device,
            &token,
            source,
            41,
            json!({"id":read["id"],"result":["on","cool",25,"small_fan","off"]}),
        )
        .await;

        let (command, _) = receive_request(&device, &token).await;
        assert_eq!(command["method"], "set_power");
        assert_eq!(command["params"], json!(["off"]));
        reply(
            &device,
            &token,
            source,
            42,
            json!({"id":command["id"],"result":["ok"]}),
        )
        .await;
        let mut extra = [0_u8; 1401];
        let received_extra = future::race(
            async {
                device.recv_from(&mut extra).await.unwrap();
                true
            },
            async {
                Timer::after(Duration::from_millis(20)).await;
                false
            },
        )
        .await;
        assert!(
            !received_extra,
            "typed command must send exactly one datagram"
        );
    };
    future::block_on(future::race(
        future::zip(session.run(), future::zip(application, simulated_device)),
        async {
            Timer::after(Duration::from_secs(2)).await;
            panic!("mcn02 runtime integration exceeded its bounded deadline");
        },
    ))
    .0
    .unwrap();
}

#[test]
fn multi_property_push_preserves_all_items_and_rejects_bad_event_arrays() {
    let notifications = super::protocol::parse_notifications(
        42,
        NetworkEpoch::new(9),
        3,
        100,
        &json!({"method":"properties_changed","params":[
            {"did":"42","siid":2,"piid":1,"value":true},
            {"did":42,"siid":3,"piid":2,"value":7}
        ]}),
    )
    .unwrap();
    assert_eq!(notifications.len(), 2);
    assert!(matches!(
        notifications[0],
        LanNotification::Property { siid: 2, .. }
    ));
    assert!(
        super::protocol::parse_notifications(
            42,
            NetworkEpoch::new(9),
            3,
            100,
            &json!({"method":"event_occured","params":{"siid":2,"eiid":3,"arguments":"bad"}}),
        )
        .is_err()
    );
}

#[test]
fn response_shapes_require_unique_correlation_and_explicit_success() {
    let requested = [
        LanProperty { siid: 2, piid: 1 },
        LanProperty { siid: 3, piid: 1 },
    ];
    assert!(
        super::protocol::parse_reads(
            42,
            &requested,
            &json!({"result":[
                {"did":"42","siid":2,"piid":1,"code":0,"value":1},
                {"did":"42","siid":2,"piid":1,"code":1,"value":2}
            ]})
        )
        .is_err()
    );
    assert!(
        super::protocol::parse_reads(
            42,
            &requested[..1],
            &json!({"result":[
                {"did":"42","siid":2,"piid":1,"code":"0","value":1}
            ]})
        )
        .is_err()
    );
    for invalid in [
        json!({"result":[]}),
        json!({"result":["error"]}),
        json!({"result":"ok"}),
    ] {
        assert!(super::protocol::accept_legacy(&invalid, "test legacy").is_err());
    }
    assert!(
        super::protocol::accept_action(
            &json!({"result":{"code":0,"did":"42","siid":2,"aiid":9}}),
            42,
            2,
            3,
        )
        .is_err()
    );
}

#[test]
fn sent_malformed_control_is_ambiguous_and_guard_children_do_not_share_attempt_state() {
    let (session, handle, _events, device, token) = session_pair("test.device");
    let parent = LanSendGuard::new();
    let application = async {
        handle
            .authenticate(
                LanProperty { siid: 2, piid: 1 },
                Instant::now() + Duration::from_secs(1),
                LanSendGuard::new(),
            )
            .await
            .unwrap();
        handle
            .set_properties(
                &[LanPropertyWrite {
                    property: LanProperty { siid: 2, piid: 2 },
                    value: WireValue::Integer(1),
                }],
                Instant::now() + Duration::from_secs(1),
                parent.clone(),
            )
            .await
            .unwrap();
        assert!(!parent.may_have_been_sent());
        let malformed = handle
            .set_properties(
                &[LanPropertyWrite {
                    property: LanProperty { siid: 2, piid: 2 },
                    value: WireValue::Integer(2),
                }],
                Instant::now() + Duration::from_secs(1),
                parent.clone(),
            )
            .await
            .unwrap_err();
        assert_eq!(malformed.kind(), &LanErrorKind::Protocol);
        assert!(malformed.may_have_been_sent());
        assert!(!parent.may_have_been_sent());
        parent.revoke();
        let unsent = handle
            .set_properties(
                &[LanPropertyWrite {
                    property: LanProperty { siid: 2, piid: 2 },
                    value: WireValue::Integer(3),
                }],
                Instant::now() + Duration::from_secs(1),
                parent,
            )
            .await
            .unwrap_err();
        assert_eq!(unsent.kind(), &LanErrorKind::Cancelled);
        assert!(!unsent.may_have_been_sent());
        handle.stop();
    };
    let simulated_device = async {
        let (auth, source) = receive_request(&device, &token).await;
        reply(&device, &token, source, 100, json!({"id":auth["id"],"result":[{"did":"42","siid":2,"piid":1,"code":0,"value":true}]})).await;
        let (first, _) = receive_request(&device, &token).await;
        reply(
            &device,
            &token,
            source,
            101,
            json!({"id":first["id"],"result":[{"did":"42","siid":2,"piid":2,"code":0}]}),
        )
        .await;
        let (second, _) = receive_request(&device, &token).await;
        reply(
            &device,
            &token,
            source,
            102,
            json!({"id":second["id"],"result":[]}),
        )
        .await;
    };
    future::block_on(future::zip(
        session.run(),
        future::zip(application, simulated_device),
    ))
    .0
    .unwrap();
}

#[test]
fn concurrent_native_reads_correlate_out_of_order_responses() {
    let (session, handle, _events, device, token) = session_pair("test.device");
    let application = async {
        handle
            .authenticate(
                LanProperty { siid: 2, piid: 1 },
                Instant::now() + Duration::from_secs(1),
                LanSendGuard::new(),
            )
            .await
            .unwrap();
        let first = handle.read_properties(
            &[LanProperty { siid: 3, piid: 1 }],
            Instant::now() + Duration::from_secs(1),
            LanSendGuard::new(),
        );
        let second = handle.read_properties(
            &[LanProperty { siid: 4, piid: 1 }],
            Instant::now() + Duration::from_secs(1),
            LanSendGuard::new(),
        );
        let (first, second) = future::zip(first, second).await;
        assert_eq!(
            first.unwrap()[0].outcome,
            LanReadOutcome::Value(WireValue::Integer(31))
        );
        assert_eq!(
            second.unwrap()[0].outcome,
            LanReadOutcome::Value(WireValue::Integer(41))
        );
        handle.stop();
    };
    let simulated_device = async {
        let (auth, source) = receive_request(&device, &token).await;
        reply(&device, &token, source, 20, json!({"id":auth["id"],"result":[{"did":"42","siid":2,"piid":1,"code":0,"value":true}]})).await;
        let (one, _) = receive_request(&device, &token).await;
        let (two, _) = receive_request(&device, &token).await;
        for request in [two, one] {
            let siid = request["params"][0]["siid"].as_u64().unwrap();
            reply(&device, &token, source, 21, json!({"id":request["id"],"result":[{"did":"42","siid":siid,"piid":1,"code":0,"value":siid * 10 + 1}]})).await;
        }
    };
    future::block_on(future::zip(
        session.run(),
        future::zip(application, simulated_device),
    ))
    .0
    .unwrap();
}

#[test]
fn revoking_after_udp_delivery_keeps_each_authenticated_reply() {
    let (session, handle, _events, device, token) = session_pair("test.device");
    let parent = LanSendGuard::new();
    let first_guard = parent.child();
    let second_guard = parent.child();
    let cancel_guard = first_guard.clone();
    let (requests_received, requests_received_rx) = flume::bounded(1);
    let application = async {
        handle
            .authenticate(
                LanProperty { siid: 2, piid: 1 },
                Instant::now() + Duration::from_secs(1),
                LanSendGuard::new(),
            )
            .await
            .unwrap();
        let first = handle.read_properties(
            &[LanProperty { siid: 3, piid: 1 }],
            Instant::now() + Duration::from_secs(1),
            first_guard,
        );
        let second = handle.read_properties(
            &[LanProperty { siid: 4, piid: 1 }],
            Instant::now() + Duration::from_secs(1),
            second_guard,
        );
        let ((first, second), ()) = future::zip(future::zip(first, second), async {
            requests_received_rx.recv_async().await.unwrap();
            cancel_guard.revoke();
        })
        .await;
        assert_eq!(
            first.unwrap()[0].outcome,
            LanReadOutcome::Value(WireValue::Integer(31))
        );
        assert_eq!(
            second.unwrap()[0].outcome,
            LanReadOutcome::Value(WireValue::Integer(41))
        );
        assert!(!parent.may_have_been_sent());
        handle.stop();
    };
    let simulated_device = async {
        let (auth, source) = receive_request(&device, &token).await;
        reply(&device, &token, source, 100, json!({"id":auth["id"],"result":[{"did":"42","siid":2,"piid":1,"code":0,"value":true}]})).await;
        let (first, _) = receive_request(&device, &token).await;
        let (second, _) = receive_request(&device, &token).await;
        assert_ne!(first["id"], second["id"]);
        requests_received.send_async(()).await.unwrap();
        Timer::after(Duration::from_millis(10)).await;
        reply(
            &device,
            &token,
            source,
            100,
            json!({"id":first["id"],"result":[{"did":"42","siid":3,"piid":1,"code":0,"value":31}]}),
        )
        .await;
        reply(&device, &token, source, 101, json!({"id":second["id"],"result":[{"did":"42","siid":4,"piid":1,"code":0,"value":41}]})).await;
    };
    future::block_on(future::zip(
        session.run(),
        future::zip(application, simulated_device),
    ))
    .0
    .unwrap();
}

#[test]
fn replay_spoof_oversize_and_push_id_collision_cannot_complete_request() {
    let (session, handle, _events, device, token) = session_pair("test.device");
    let attacker = socket();
    let application = async {
        handle
            .authenticate(
                LanProperty { siid: 2, piid: 1 },
                Instant::now() + Duration::from_secs(1),
                LanSendGuard::new(),
            )
            .await
            .unwrap();
        let duplicate = handle
            .read_properties(
                &[
                    LanProperty { siid: 3, piid: 1 },
                    LanProperty { siid: 3, piid: 1 },
                ],
                Instant::now() + Duration::from_secs(1),
                LanSendGuard::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(duplicate.kind(), &LanErrorKind::InvalidInput);
        let read = handle
            .read_properties(
                &[LanProperty { siid: 3, piid: 1 }],
                Instant::now() + Duration::from_secs(1),
                LanSendGuard::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            read[0].outcome,
            LanReadOutcome::Value(WireValue::Integer(9))
        );
        handle.stop();
    };
    let simulated_device = async {
        let (auth, source) = receive_request(&device, &token).await;
        let auth_response = json!({"id":auth["id"],"result":[{"did":"42","siid":2,"piid":1,"code":0,"value":true}]});
        reply(&device, &token, source, 20, auth_response.clone()).await;
        let (read, _) = receive_request(&device, &token).await;
        let valid =
            json!({"id":read["id"],"result":[{"did":"42","siid":3,"piid":1,"code":0,"value":9}]});
        let spoof = encode_packet(42, 21, &token, valid.to_string().as_bytes()).unwrap();
        attacker.send_to(&spoof, source).await.unwrap();
        reply(&device, &token, source, 20, auth_response).await;
        device.send_to(&[0_u8; 1401], source).await.unwrap();
        reply(&device, &token, source, 21, json!({"id":read["id"],"method":"properties_changed","params":[{"siid":3,"piid":1,"value":99}]})).await;
        reply(&device, &token, source, 22, valid).await;
    };
    future::block_on(future::zip(
        session.run(),
        future::zip(application, simulated_device),
    ))
    .0
    .unwrap();
}

#[test]
fn probe_receives_unauthenticated_hint_and_is_rate_limited() {
    let client = socket();
    let server = socket();
    let destination = match server.get_ref().local_addr().unwrap() {
        SocketAddr::V4(address) => address,
        SocketAddr::V6(_) => unreachable!(),
    };
    let mut discovery = LanDiscovery::new(99).unwrap();
    let probe = discovery.probe_socket(
        loopback_interface(),
        NetworkEpoch::new(3),
        client,
        destination,
        Instant::now() + Duration::from_millis(100),
        LanSendGuard::new(),
    );
    let responder = async {
        let mut buffer = [0_u8; 32];
        let (_, source) = server.recv_from(&mut buffer).await.unwrap();
        assert_eq!(&buffer[16..20], b"MDID");
        server.recv_from(&mut buffer).await.unwrap();
        let mut hello = [0_u8; 32];
        hello[..4].copy_from_slice(&[0x21, 0x31, 0, 32]);
        hello[4..12].copy_from_slice(&42_u64.to_be_bytes());
        hello[12..16].copy_from_slice(&123_u32.to_be_bytes());
        hello[16..20].copy_from_slice(b"MSUB");
        hello[20..24].copy_from_slice(&77_u32.to_be_bytes());
        hello[24..27].copy_from_slice(b"PUB");
        hello[27] = 4;
        hello[28] = 1;
        server.send_to(&hello, source).await.unwrap();
    };
    let (candidates, ()) = future::block_on(future::zip(probe, responder));
    let candidates = candidates.unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].timestamp_hint, 123);
    assert!(candidates[0].native_hint);
    assert_eq!(
        candidates[0]
            .subscription_hint
            .unwrap()
            .subscription_timestamp,
        77
    );
    assert_eq!(
        future::block_on(discovery.probe_socket(
            loopback_interface(),
            NetworkEpoch::new(3),
            socket(),
            destination,
            Instant::now() + Duration::from_millis(10),
            LanSendGuard::new(),
        ))
        .unwrap_err()
        .kind(),
        &LanErrorKind::RateLimited
    );
}

#[test]
fn cancelled_guard_sends_nothing_and_timeout_is_exactly_once() {
    let pending_client = socket();
    let pending_device = socket();
    let pending_destination = match pending_device.get_ref().local_addr().unwrap() {
        SocketAddr::V4(address) => address,
        SocketAddr::V6(_) => unreachable!(),
    };
    let pending_guard = LanSendGuard::new();
    let revoke = pending_guard.clone();
    let (pending_result, ()) = future::block_on(future::zip(
        super::session::force_pending_send_for_test(
            &pending_client,
            pending_destination,
            &pending_guard,
            Instant::now() + Duration::from_secs(1),
        ),
        async {
            Timer::after(Duration::from_millis(5)).await;
            revoke.revoke();
        },
    ));
    assert_eq!(pending_result.unwrap_err().kind(), &LanErrorKind::Cancelled);
    assert_eq!(
        pending_device
            .get_ref()
            .recv_from(&mut [0_u8; 64])
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::WouldBlock
    );
    let timeout = future::block_on(super::session::force_pending_send_for_test(
        &pending_client,
        pending_destination,
        &LanSendGuard::new(),
        Instant::now() + Duration::from_millis(5),
    ))
    .unwrap_err();
    assert_eq!(timeout.kind(), &LanErrorKind::Timeout);
    let stopped = future::block_on(super::session::force_pending_stop_for_test(
        &pending_client,
        pending_destination,
        &LanSendGuard::new(),
    ))
    .unwrap_err();
    assert_eq!(stopped.kind(), &LanErrorKind::Cancelled);

    let (session, handle, _events, device, _token) = session_pair("test.device");
    let local_authority = Rc::new(Cell::new(false));
    let checked = local_authority.clone();
    let cancelled = LanSendGuard::with_check(move || Ok(checked.get()));
    let application = async {
        let error = handle
            .authenticate(
                LanProperty { siid: 2, piid: 1 },
                Instant::now() + Duration::from_millis(100),
                cancelled,
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind(), &LanErrorKind::Cancelled);
        Timer::after(Duration::from_millis(20)).await;
        assert_eq!(
            device
                .get_ref()
                .recv_from(&mut [0_u8; 64])
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::WouldBlock
        );
        handle.stop();
    };
    future::block_on(future::zip(session.run(), application))
        .0
        .unwrap();

    let (session, handle, _events, device, _token) = session_pair("test.device");
    {
        let request = handle.authenticate(
            LanProperty { siid: 2, piid: 1 },
            Instant::now() + Duration::from_secs(1),
            LanSendGuard::new(),
        );
        futures_lite::pin!(request);
        assert!(future::block_on(future::poll_once(request.as_mut())).is_none());
    }
    let application = async {
        Timer::after(Duration::from_millis(20)).await;
        assert_eq!(
            device
                .get_ref()
                .recv_from(&mut [0_u8; 64])
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::WouldBlock
        );
        handle.stop();
    };
    future::block_on(future::zip(session.run(), application))
        .0
        .unwrap();

    let (session, handle, _events, device, token) = session_pair("test.device");
    let application = async {
        let error = handle
            .authenticate(
                LanProperty { siid: 2, piid: 1 },
                Instant::now() + Duration::from_millis(40),
                LanSendGuard::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind(), &LanErrorKind::Timeout);
        assert!(error.may_have_been_sent());
        Timer::after(Duration::from_millis(20)).await;
        assert_eq!(
            device
                .get_ref()
                .recv_from(&mut [0_u8; 1401])
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::WouldBlock
        );
        handle.stop();
    };
    let receiver = async {
        let (request, _) = receive_request(&device, &token).await;
        assert_eq!(request["method"], "get_properties");
    };
    future::block_on(future::zip(
        session.run(),
        future::zip(application, receiver),
    ))
    .0
    .unwrap();
}

#[test]
fn malformed_datagram_flood_yields_to_shutdown() {
    let (session, handle, _events, device, _token) = session_pair("test.device");
    let destination = session.local_address_for_test();
    let flooding = Arc::new(AtomicBool::new(true));
    let sender_flag = flooding.clone();
    let flood = async {
        let mut sent = 0_u32;
        while sender_flag.load(Ordering::Acquire) {
            device.send_to(b"bad", destination).await.unwrap();
            sent += 1;
            if sent.is_multiple_of(32) {
                future::yield_now().await;
            }
        }
        assert!(sent >= 32);
    };
    let application = async {
        Timer::after(Duration::from_millis(20)).await;
        flooding.store(false, Ordering::Release);
        handle.stop();
    };
    future::block_on(future::zip(session.run(), future::zip(application, flood)))
        .0
        .unwrap();
}

#[test]
fn request_deadline_and_cancellation_cover_an_unpolled_full_actor_queue() {
    let (_session, handle, _events, device, _token) = session_pair("test.device");
    let error = future::block_on(handle.authenticate(
        LanProperty { siid: 2, piid: 1 },
        Instant::now() + Duration::from_millis(10),
        LanSendGuard::new(),
    ))
    .unwrap_err();
    assert_eq!(error.kind(), &LanErrorKind::Timeout);
    assert!(!error.may_have_been_sent());
    assert_eq!(
        device
            .get_ref()
            .recv_from(&mut [0_u8; 64])
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::WouldBlock
    );

    let (_session, handle, _events, _device, _token) = session_pair("test.device");
    let mut queued = (0..33)
        .map(|_| {
            Box::pin(handle.authenticate(
                LanProperty { siid: 2, piid: 1 },
                Instant::now() + Duration::from_secs(1),
                LanSendGuard::new(),
            ))
        })
        .collect::<Vec<_>>();
    for request in &mut queued {
        assert!(future::block_on(future::poll_once(request.as_mut())).is_none());
    }
    let cancellation = LanSendGuard::new();
    let mut overflow = Box::pin(handle.authenticate(
        LanProperty { siid: 2, piid: 1 },
        Instant::now() + Duration::from_secs(1),
        cancellation.clone(),
    ));
    assert!(future::block_on(future::poll_once(overflow.as_mut())).is_none());
    cancellation.revoke();
    let error = future::block_on(overflow).unwrap_err();
    assert_eq!(error.kind(), &LanErrorKind::Cancelled);
    assert!(!error.may_have_been_sent());
}
