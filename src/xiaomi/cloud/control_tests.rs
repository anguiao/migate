use super::*;
use crate::xiaomi::{
    catalog::WireValue,
    test_support::{MockResponse, dynamic_mock_server, mock_server},
};
use futures_lite::future::{self, block_on};
use futures_util::FutureExt;
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
