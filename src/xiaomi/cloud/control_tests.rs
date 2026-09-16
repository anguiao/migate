use super::*;
use crate::{
    xiaomi::catalog::WireValue,
    xiaomi::gateway::EventArguments,
    xiaomi::mqtt::MqttErrorKind,
    xiaomi::test_support::{MockResponse, dynamic_mock_server, mock_server},
};
use bytes::BytesMut;
use futures_lite::future::{self, block_on};
use futures_util::FutureExt;
use mqttbytes::{QoS, v5};
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    thread,
    time::{Duration, Instant},
};

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(1)
}

fn property(did: &str, siid: u32, piid: u32) -> CloudProperty {
    CloudProperty::new(did, siid, piid)
}

fn notification_packet(stream: &mut TcpStream, buffer: &mut BytesMut) -> v5::Packet {
    loop {
        match v5::read(buffer, 256 * 1024) {
            Ok(packet) => return packet,
            Err(mqttbytes::Error::InsufficientBytes(_)) => {
                let mut bytes = [0; 4096];
                let count = stream.read(&mut bytes).unwrap();
                assert!(count > 0, "cloud notification connection closed early");
                buffer.extend_from_slice(&bytes[..count]);
            }
            Err(mqttbytes::Error::PayloadRequired) => {
                return v5::Packet::Disconnect(v5::Disconnect::new());
            }
            Err(error) => panic!("invalid cloud notification packet: {error:?}"),
        }
    }
}

fn write_notification_packet(stream: &mut TcpStream, packet: &v5::Packet) {
    let mut bytes = BytesMut::new();
    match packet {
        v5::Packet::ConnAck(value) => {
            bytes.extend_from_slice(&[0x20, 0x03, value.session_present as u8, value.code as u8, 0])
        }
        v5::Packet::SubAck(value) => {
            value.write(&mut bytes).unwrap();
        }
        v5::Packet::UnsubAck(value) => {
            value.write(&mut bytes).unwrap();
        }
        v5::Packet::Publish(value) => {
            value.write(&mut bytes).unwrap();
        }
        _ => panic!("unsupported cloud notification test packet"),
    }
    stream.write_all(&bytes).unwrap();
}

#[test]
fn reads_properties_with_exact_headers_body_and_partial_results() {
    let body = json!({"code":0,"result":[
        {"did":"air","siid":2,"piid":3,"code":0,"value":24},
        {"did":"air","siid":2,"piid":4,"code":-4004},
        {"did":"air","siid":2,"piid":5,"code":0,"value":null},
        {"did":"air","siid":2,"piid":6,"code":0,"value":{"unsupported":true}}
    ]});
    let (base, requests) = mock_server(vec![MockResponse::json(200, &body.to_string())]);
    let client = CloudClient::for_test_with_control_timeout(
        &base,
        Duration::from_secs(1),
        Duration::from_millis(200),
    )
    .unwrap();
    let properties = [
        property("air", 2, 3),
        property("air", 2, 4),
        property("air", 2, 5),
        property("air", 2, 6),
    ];
    let result =
        block_on(client.read_properties("access-secret", &properties, deadline())).unwrap();
    assert_eq!(
        result,
        vec![
            PropertyRead::new(
                properties[0].clone(),
                PropertyReadOutcome::Value(WireValue::Integer(24))
            ),
            PropertyRead::new(properties[1].clone(), PropertyReadOutcome::Error(-4004)),
            PropertyRead::new(properties[2].clone(), PropertyReadOutcome::Unknown),
            PropertyRead::new(properties[3].clone(), PropertyReadOutcome::Unknown),
        ]
    );
    let request = requests.recv().unwrap();
    assert_eq!(request.target, "/app/v2/miotspec/prop/get");
    let headers = request.headers.to_ascii_lowercase();
    assert!(headers.contains("content-type: application/json"));
    assert!(headers.contains("x-client-bizid: haapi"));
    assert!(headers.contains("x-client-appid: 2882303761520251711"));
    assert!(headers.contains("authorization: beareraccess-secret"));
    assert_eq!(
        serde_json::from_str::<Value>(&request.body).unwrap(),
        json!({"datasource":1,"params":[
            {"did":"air","siid":2,"piid":3},
            {"did":"air","siid":2,"piid":4},
            {"did":"air","siid":2,"piid":5},
            {"did":"air","siid":2,"piid":6}
        ]})
    );
}

#[test]
fn writes_preserve_integer_values_and_return_each_explicit_outcome() {
    let response = json!({"code":0,"result":[
        {"did":"air","siid":2,"piid":3,"code":0},
        {"did":"air","siid":2,"piid":4,"code":-4004}
    ]});
    let (base, requests) = mock_server(vec![MockResponse::json(200, &response.to_string())]);
    let client = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    let writes = [
        PropertyWrite::new(property("air", 2, 3), WireValue::Integer(24)),
        PropertyWrite::new(property("air", 2, 4), WireValue::Boolean(true)),
    ];
    let result = block_on(client.set_properties("access", &writes, deadline())).unwrap();
    assert_eq!(
        result,
        vec![
            PropertyWriteResult::new(writes[0].property().clone(), PropertyWriteOutcome::Accepted),
            PropertyWriteResult::new(
                writes[1].property().clone(),
                PropertyWriteOutcome::Error(-4004)
            ),
        ]
    );
    let request = requests.recv().unwrap();
    assert_eq!(request.target, "/app/v2/miotspec/prop/set");
    let body: Value = serde_json::from_str(&request.body).unwrap();
    assert_eq!(body["params"][0]["value"], json!(24));
    assert!(body["params"][0]["value"].as_i64().is_some());
    assert_eq!(body["params"][1]["value"], json!(true));
}

#[test]
fn action_sends_plain_arguments_and_requires_matching_success() {
    let response = json!({"code":0,"result":{"did":"vacuum","siid":2,"aiid":1,"code":0}});
    let (base, requests) = mock_server(vec![MockResponse::json(200, &response.to_string())]);
    let client = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    let action = CloudAction::new(
        "vacuum",
        2,
        1,
        vec![WireValue::Integer(2), WireValue::String("room".into())],
    );
    block_on(client.invoke_action("access", &action, deadline())).unwrap();
    let request = requests.recv().unwrap();
    assert_eq!(request.target, "/app/v2/miotspec/action");
    assert_eq!(
        serde_json::from_str::<Value>(&request.body).unwrap(),
        json!({"params":{"did":"vacuum","siid":2,"aiid":1,"in":[2,"room"]}})
    );

    for result in [
        json!({"did":"other","siid":2,"aiid":1,"code":0}),
        json!({"did":"vacuum","siid":9,"aiid":1,"code":0}),
        json!({"did":"vacuum","siid":2,"aiid":9,"code":0}),
    ] {
        let body = json!({"code":0,"result":result}).to_string();
        let (base, _) = mock_server(vec![MockResponse::json(200, &body)]);
        let client = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
        assert_eq!(
            block_on(client.invoke_action("access", &action, deadline()))
                .unwrap_err()
                .kind(),
            &CloudErrorKind::Protocol
        );
    }
    let body = json!({"code":0,"result":{"did":"vacuum","siid":2,"aiid":1,"code":-1}}).to_string();
    let (base, _) = mock_server(vec![MockResponse::json(200, &body)]);
    let client = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    assert_eq!(
        block_on(client.invoke_action("access", &action, deadline()))
            .unwrap_err()
            .kind(),
        &CloudErrorKind::Business(-1)
    );
}

#[test]
fn property_replies_reject_duplicates_mismatches_and_missing_items() {
    let requested = [property("did", 2, 1), property("did", 2, 2)];
    for result in [
        json!([
            {"did":"did","siid":2,"piid":1,"code":0,"value":true},
            {"did":"did","siid":2,"piid":1,"code":0,"value":false}
        ]),
        json!([
            {"did":"did","siid":2,"piid":1,"code":0,"value":true},
            {"did":"other","siid":2,"piid":2,"code":0,"value":false}
        ]),
        json!([{"did":"did","siid":2,"piid":1,"code":0,"value":true}]),
    ] {
        let body = json!({"code":0,"result":result}).to_string();
        let (base, _) = mock_server(vec![MockResponse::json(200, &body)]);
        let client = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
        assert_eq!(
            block_on(client.read_properties("access", &requested, deadline()))
                .unwrap_err()
                .kind(),
            &CloudErrorKind::Protocol
        );
    }
}

#[test]
fn property_write_replies_reject_wrong_did_and_piid() {
    let write = PropertyWrite::new(property("did", 2, 1), WireValue::Boolean(true));
    for result in [
        json!([{"did":"other","siid":2,"piid":1,"code":0}]),
        json!([{"did":"did","siid":2,"piid":9,"code":0}]),
    ] {
        let body = json!({"code":0,"result":result}).to_string();
        let (base, _) = mock_server(vec![MockResponse::json(200, &body)]);
        let client = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
        assert_eq!(
            block_on(client.set_properties("access", std::slice::from_ref(&write), deadline()))
                .unwrap_err()
                .kind(),
            &CloudErrorKind::Protocol
        );
    }
}

#[test]
fn control_http_and_envelope_failures_are_safe_and_sent_once() {
    let action = CloudAction::new("did", 2, 1, vec![]);
    for (response, expected) in [
        (
            MockResponse::json(401, "token-secret"),
            CloudErrorKind::Unauthorized,
        ),
        (
            MockResponse::json(503, "server-secret"),
            CloudErrorKind::HttpStatus(503),
        ),
        (
            MockResponse::json(200, r#"{"code":-7,"message":"response-secret"}"#),
            CloudErrorKind::Business(-7),
        ),
        (MockResponse::raw(""), CloudErrorKind::Network),
    ] {
        let (base, requests) = mock_server(vec![response]);
        let client = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
        let error =
            block_on(client.invoke_action("access-secret", &action, deadline())).unwrap_err();
        assert_eq!(error.kind(), &expected);
        assert!(!format!("{error:?} {error}").contains("secret"));
        requests.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(requests.recv_timeout(Duration::from_millis(80)).is_err());
    }
}

#[test]
fn control_timeout_covers_headers_and_body_without_retrying() {
    let action = CloudAction::new("did", 2, 1, vec![]);
    for response in [
        MockResponse::json(
            200,
            r#"{"code":0,"result":{"did":"did","siid":2,"aiid":1,"code":0}}"#,
        )
        .delayed(Duration::from_millis(100)),
        MockResponse::delayed_body(
            200,
            r#"{"code":0,"result":{"did":"did","siid":2,"aiid":1,"code":0}}"#,
            5,
            Duration::from_millis(100),
        ),
    ] {
        let (base, requests) = mock_server(vec![response]);
        let client = CloudClient::for_test_with_control_timeout(
            &base,
            Duration::from_secs(1),
            Duration::from_millis(20),
        )
        .unwrap();
        assert!(
            block_on(client.invoke_action("access", &action, deadline()))
                .unwrap_err()
                .is_timeout()
        );
        requests.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(requests.recv_timeout(Duration::from_millis(120)).is_err());
    }
}

#[test]
fn remaining_deadline_shortens_the_control_timeout() {
    let response = MockResponse::json(
        200,
        r#"{"code":0,"result":{"did":"did","siid":2,"aiid":1,"code":0}}"#,
    )
    .delayed(Duration::from_millis(100));
    let (base, requests) = mock_server(vec![response]);
    let client = CloudClient::for_test_with_control_timeout(
        &base,
        Duration::from_secs(1),
        Duration::from_millis(200),
    )
    .unwrap();
    let action = CloudAction::new("did", 2, 1, vec![]);
    assert!(
        block_on(client.invoke_action(
            "access",
            &action,
            Instant::now() + Duration::from_millis(20),
        ))
        .unwrap_err()
        .is_timeout()
    );
    requests.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(requests.recv_timeout(Duration::from_millis(120)).is_err());
}

#[test]
fn deadline_invalid_input_and_cancellation_never_add_control_posts() {
    let action = CloudAction::new("did", 2, 1, vec![]);
    let (base, requests) = dynamic_mock_server(2, |_| {
        MockResponse::json(
            200,
            r#"{"code":0,"result":{"did":"did","siid":2,"aiid":1,"code":0}}"#,
        )
        .delayed(Duration::from_millis(200))
    });
    let client = CloudClient::for_test_with_control_timeout(
        &base,
        Duration::from_secs(1),
        Duration::from_secs(1),
    )
    .unwrap();
    assert!(
        block_on(client.invoke_action("access", &action, Instant::now()))
            .unwrap_err()
            .is_timeout()
    );
    assert_eq!(
        block_on(client.read_properties("access", &[], deadline()))
            .unwrap_err()
            .kind(),
        &CloudErrorKind::InvalidInput
    );
    assert_eq!(
        block_on(client.set_properties("access", &[], deadline()))
            .unwrap_err()
            .kind(),
        &CloudErrorKind::InvalidInput
    );
    assert_eq!(
        block_on(client.invoke_action("access", &CloudAction::new("", 2, 1, vec![]), deadline()))
            .unwrap_err()
            .kind(),
        &CloudErrorKind::InvalidInput
    );
    assert_eq!(
        block_on(client.set_properties(
            "access",
            &[PropertyWrite::new(
                property("did", 2, 1),
                WireValue::Number(f64::NAN),
            )],
            deadline(),
        ))
        .unwrap_err()
        .kind(),
        &CloudErrorKind::InvalidInput
    );
    assert_eq!(
        block_on(client.read_properties("access", &[property("did", 0, 1)], deadline()))
            .unwrap_err()
            .kind(),
        &CloudErrorKind::InvalidInput
    );
    assert!(requests.recv_timeout(Duration::from_millis(80)).is_err());

    block_on(async {
        let request = client.invoke_action("access", &action, deadline());
        futures_lite::pin!(request);
        let completed = future::or(async { Some(request.await) }, async {
            async_io::Timer::after(Duration::from_millis(30)).await;
            None
        })
        .await;
        assert!(completed.is_none());
    });
    requests.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(requests.recv_timeout(Duration::from_millis(250)).is_err());
}

#[test]
fn control_timeout_does_not_replace_the_general_http_timeout() {
    let home = r#"{"code":0,"result":{"homelist":[]}}"#;
    let action_reply = r#"{"code":0,"result":{"did":"did","siid":2,"aiid":1,"code":0}}"#;
    let (base, _) = mock_server(vec![
        MockResponse::json(200, home).delayed(Duration::from_millis(60)),
        MockResponse::json(200, action_reply).delayed(Duration::from_millis(60)),
    ]);
    let client = CloudClient::for_test_with_control_timeout(
        &base,
        Duration::from_millis(20),
        Duration::from_millis(300),
    )
    .unwrap();
    assert!(
        block_on(client.get_home("access"))
            .unwrap_err()
            .is_timeout()
    );
    block_on(client.invoke_action("access", &CloudAction::new("did", 2, 1, vec![]), deadline()))
        .unwrap();
}

#[test]
fn cloud_notifications_use_selected_topics_and_preserve_keyed_or_positional_events() {
    let selected = vec!["did".to_owned()];
    let property = CloudNotificationSession::parse(
        "device/did/up/properties_changed/2/3",
        br#"{"params":{"siid":2,"piid":3,"value":24}}"#,
        false,
        &selected,
        4,
    )
    .unwrap();
    assert!(matches!(
        property,
        CloudNotification::Property {
            value: Some(WireValue::Integer(24)),
            generation: 4,
            ..
        }
    ));
    let keyed = CloudNotificationSession::parse(
        "device/did/up/event_occured/2/4",
        br#"{"params":{"did":"did","siid":2,"eiid":4,"arguments":[{"piid":7,"value":true}]}}"#,
        false,
        &selected,
        5,
    )
    .unwrap();
    assert!(
        matches!(keyed, CloudNotification::Event { arguments: EventArguments::Keyed(values), .. } if values == vec![(7, WireValue::Boolean(true))])
    );
    let positional = CloudNotificationSession::parse(
        "device/did/up/event_occured/2/4",
        br#"{"params":{"siid":2,"eiid":4,"arguments":[{"value":[1,false]}]}}"#,
        false,
        &selected,
        6,
    )
    .unwrap();
    assert!(
        matches!(positional, CloudNotification::Event { arguments: EventArguments::Positional(values), .. } if values == vec![WireValue::Integer(1), WireValue::Boolean(false)])
    );
    for (topic, payload, retained) in [
        (
            "device/foreign/up/properties_changed/2/3",
            br#"{"params":{"siid":2,"piid":3,"value":1}}"#.as_slice(),
            false,
        ),
        (
            "device/did/up/properties_changed/2/9",
            br#"{"params":{"siid":2,"piid":3,"value":1}}"#.as_slice(),
            false,
        ),
        (
            "device/did/up/event_occurred/2/4",
            br#"{"params":{"siid":2,"eiid":4}}"#.as_slice(),
            false,
        ),
        (
            "device/did/up/event_occured/2/4",
            br#"{"params":{"siid":2,"eiid":4}}"#.as_slice(),
            false,
        ),
        (
            "device/did/up/event_occured/2/4",
            br#"{"params":{"siid":2,"eiid":4,"arguments":[]}}"#.as_slice(),
            true,
        ),
        (
            "device/did/up/properties_changed/2/3",
            br#"{"params":{"did":"foreign","siid":2,"piid":3,"value":1}}"#.as_slice(),
            false,
        ),
    ] {
        assert!(CloudNotificationSession::parse(topic, payload, retained, &selected, 7).is_err());
    }
    let state = CloudNotificationSession::parse(
        "device/did/state/change",
        br#"{"device_id":"did","event":"offline"}"#,
        false,
        &selected,
        8,
    )
    .unwrap();
    assert!(matches!(
        state,
        CloudNotification::State {
            online: false,
            generation: 8,
            ..
        }
    ));
    assert!(
        CloudNotificationSession::parse(
            "device/did/state/change",
            br#"{"device_id":"did","event":"unknown"}"#,
            false,
            &selected,
            8,
        )
        .is_err()
    );
}

#[test]
fn cloud_selection_coalesces_while_suback_is_held_and_filters_removed_dids() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (first_seen, first_ready) = flume::bounded(1);
    let (release_first, release) = flume::bounded(1);
    let (abandoned_seen, abandoned_ready) = flume::bounded(1);
    let (release_abandoned, abandoned_release) = flume::bounded(1);
    let (done, broker_done) = flume::bounded(1);
    let broker = thread::spawn(move || {
        let mut stream = listener.accept().unwrap().0;
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut buffer = BytesMut::new();
        assert!(matches!(
            notification_packet(&mut stream, &mut buffer),
            v5::Packet::Connect(_)
        ));
        write_notification_packet(
            &mut stream,
            &v5::Packet::ConnAck(v5::ConnAck::new(v5::ConnectReturnCode::Success, false)),
        );
        let v5::Packet::Subscribe(first) = notification_packet(&mut stream, &mut buffer) else {
            panic!("expected first cloud subscription")
        };
        assert!(
            first
                .filters
                .iter()
                .all(|filter| filter.path.contains("old.did"))
        );
        first_seen.send(()).unwrap();
        release.recv_timeout(Duration::from_secs(3)).unwrap();
        write_notification_packet(
            &mut stream,
            &v5::Packet::SubAck(v5::SubAck::new(
                first.pkid,
                vec![v5::SubscribeReasonCode::QoS2; first.filters.len()],
            )),
        );
        let latest = loop {
            match notification_packet(&mut stream, &mut buffer) {
                v5::Packet::Unsubscribe(remove) => {
                    assert!(remove.filters.iter().all(|topic| topic.contains("old.did")));
                    let mut unsuback = v5::UnsubAck::new(remove.pkid);
                    unsuback.reasons = vec![v5::UnsubAckReason::Success; remove.filters.len()];
                    write_notification_packet(&mut stream, &v5::Packet::UnsubAck(unsuback));
                }
                v5::Packet::Subscribe(latest) => break latest,
                packet => panic!("unexpected cloud selection packet: {packet:?}"),
            }
        };
        assert!(
            latest
                .filters
                .iter()
                .all(|filter| filter.path.contains("new.did"))
        );
        write_notification_packet(
            &mut stream,
            &v5::Packet::SubAck(v5::SubAck::new(
                latest.pkid,
                vec![v5::SubscribeReasonCode::QoS2; latest.filters.len()],
            )),
        );
        for did in ["old.did", "new.did"] {
            write_notification_packet(
                &mut stream,
                &v5::Packet::Publish(v5::Publish::new(
                    format!("device/{did}/up/properties_changed/2/3"),
                    QoS::AtMostOnce,
                    format!(r#"{{"params":{{"did":"{did}","siid":2,"piid":3,"value":7}}}}"#),
                )),
            );
        }
        let third = loop {
            match notification_packet(&mut stream, &mut buffer) {
                v5::Packet::Unsubscribe(remove) => {
                    assert!(remove.filters.iter().all(|topic| topic.contains("new.did")));
                    let mut ack = v5::UnsubAck::new(remove.pkid);
                    ack.reasons = vec![v5::UnsubAckReason::Success; remove.filters.len()];
                    write_notification_packet(&mut stream, &v5::Packet::UnsubAck(ack));
                }
                v5::Packet::Subscribe(third) => break third,
                packet => panic!("unexpected abandoned cloud selection packet: {packet:?}"),
            }
        };
        assert!(
            third
                .filters
                .iter()
                .all(|filter| filter.path.contains("abandoned.did"))
        );
        abandoned_seen.send(()).unwrap();
        abandoned_release
            .recv_timeout(Duration::from_secs(3))
            .unwrap();
        write_notification_packet(
            &mut stream,
            &v5::Packet::SubAck(v5::SubAck::new(
                third.pkid,
                vec![v5::SubscribeReasonCode::QoS2; third.filters.len()],
            )),
        );
        let final_selection = loop {
            match notification_packet(&mut stream, &mut buffer) {
                v5::Packet::Unsubscribe(remove) => {
                    assert!(
                        remove
                            .filters
                            .iter()
                            .all(|topic| topic.contains("abandoned.did"))
                    );
                    let mut ack = v5::UnsubAck::new(remove.pkid);
                    ack.reasons = vec![v5::UnsubAckReason::Success; remove.filters.len()];
                    write_notification_packet(&mut stream, &v5::Packet::UnsubAck(ack));
                }
                v5::Packet::Subscribe(final_selection) => break final_selection,
                packet => panic!("unexpected final cloud selection packet: {packet:?}"),
            }
        };
        assert!(
            final_selection
                .filters
                .iter()
                .all(|filter| filter.path.contains("final.did"))
        );
        write_notification_packet(
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
                let (connection, session, handle, notifications) = CloudNotificationSession::new(
                    "test-client",
                    "token",
                    Duration::from_secs(5),
                    &address.ip().to_string(),
                    address.port(),
                    None,
                )
                .unwrap();
                let mut driver = Box::pin(connection.run());
                let mut session = Box::pin(session.run());
                let app = async {
                    let mut first = Box::pin(handle.select_dids(
                        vec!["old.did".into()],
                        Instant::now() + Duration::from_secs(2),
                    ));
                    assert!(future::poll_once(first.as_mut()).await.is_none());
                    first_ready.recv_async().await.unwrap();
                    let mut latest = Box::pin(handle.select_dids(
                        vec!["new.did".into()],
                        Instant::now() + Duration::from_secs(2),
                    ));
                    assert!(future::poll_once(latest.as_mut()).await.is_none());
                    release_first.send_async(()).await.unwrap();
                    assert_eq!(first.await.unwrap_err().kind(), &MqttErrorKind::Superseded);
                    let latest_generation = latest.await.unwrap();
                    let notification = notifications.recv_async().await.unwrap();
                    assert!(matches!(
                        notification,
                        CloudNotification::Property { did, generation, .. }
                            if did == "new.did" && generation == latest_generation
                    ));
                    let mut abandoned = Box::pin(handle.select_dids(
                        vec!["abandoned.did".into()],
                        Instant::now() + Duration::from_secs(2),
                    ));
                    assert!(future::poll_once(abandoned.as_mut()).await.is_none());
                    abandoned_ready.recv_async().await.unwrap();
                    drop(abandoned);
                    let mut final_selection = Box::pin(handle.select_dids(
                        vec!["final.did".into()],
                        Instant::now() + Duration::from_secs(2),
                    ));
                    assert!(future::poll_once(final_selection.as_mut()).await.is_none());
                    release_abandoned.send_async(()).await.unwrap();
                    final_selection.await.unwrap();
                    done.send_async(()).await.unwrap();
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
                    Ready::Driver => panic!("MQTT driver exited before cloud assertions"),
                    Ready::Session => panic!("cloud session exited before assertions"),
                }
                true
            },
            async {
                async_io::Timer::after(Duration::from_secs(5)).await;
                false
            },
        )
        .await;
        assert!(completed, "cloud selection scenario exceeded its deadline");
    });
    broker.join().unwrap();
}
