use super::*;
use crate::xiaomi::{
    capability::PropertyBinding,
    test_support::{MockResponse, mock_server},
};
use futures_lite::future;
use serde_json::{Value, json};
use std::time::Duration;

const POWER: PropertyBinding = PropertyBinding { siid: 2, piid: 1 };

#[test]
fn numeric_property_reads_and_batch_writes_preserve_partial_acceptance() {
    let brightness = PropertyBinding { siid: 2, piid: 2 };
    let mode = PropertyBinding { siid: 4, piid: 2 };
    let (base, requests) = mock_server(vec![
        MockResponse::json(
            200,
            r#"{"code":0,"result":[{"did":"target","siid":2,"piid":2,"code":0,"value":65535}]}"#,
        ),
        MockResponse::json(
            200,
            r#"{"code":0,"result":[{"did":"target","siid":4,"piid":2,"code":-704},{"did":"target","siid":2,"piid":2,"code":1}]}"#,
        ),
    ]);
    let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    assert_eq!(
        future::block_on(cloud.read_property_value("token", "target", brightness)).unwrap(),
        json!(65535)
    );
    let result = future::block_on(cloud.set_properties(
        "token",
        "target",
        &[(brightness, json!(50)), (mode, json!(1))],
    ))
    .unwrap();
    assert!(!result.protocol_error);
    assert_eq!(result.items[0], Ok(()));
    assert_eq!(
        result.items[1].as_ref().unwrap_err().kind(),
        &CloudErrorKind::PropertyRejected(-704)
    );
    requests.recv().unwrap();
    let request = requests.recv().unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&request.body).unwrap(),
        json!({"params":[{"did":"target","siid":2,"piid":2,"value":50},{"did":"target","siid":4,"piid":2,"value":1}]})
    );
    assert!(requests.try_recv().is_err());
}

#[test]
fn batch_property_identity_errors_keep_confirmed_items() {
    let brightness = PropertyBinding { siid: 2, piid: 2 };
    for invalid in [
        json!([]),
        json!([{"did":"target","siid":2,"piid":2,"code":0},{"did":"target","siid":2,"piid":2,"code":0}]),
        json!([{"did":"foreign","siid":2,"piid":2,"code":0}]),
        json!([{"did":"target","siid":2,"code":0}]),
        json!([{"did":"target","siid":9,"piid":9,"code":0}]),
    ] {
        let mut items = vec![json!({"did":"target","siid":2,"piid":1,"code":0})];
        items.extend(invalid.as_array().unwrap().iter().cloned());
        let body = json!({"code":0,"result":items}).to_string();
        let (base, _) = mock_server(vec![MockResponse::json(200, &body)]);
        let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
        let result = future::block_on(cloud.set_properties(
            "token",
            "target",
            &[(POWER, json!(true)), (brightness, json!(50))],
        ))
        .unwrap();
        assert!(result.protocol_error, "{body}");
        assert_eq!(result.items[0], Ok(()), "{body}");
        assert_eq!(
            result.items[1].as_ref().unwrap_err().kind(),
            &CloudErrorKind::Protocol,
            "{body}"
        );
    }
}

#[test]
fn batch_reads_correlate_numeric_and_boolean_values_in_request_order() {
    let brightness = PropertyBinding { siid: 2, piid: 2 };
    let (base, _) = mock_server(vec![MockResponse::json(
        200,
        r#"{"code":0,"result":[{"did":"target","siid":2,"piid":2,"code":0,"value":35.5},{"did":"target","siid":2,"piid":1,"code":0,"value":true}]}"#,
    )]);
    let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    let result =
        future::block_on(cloud.read_properties("token", "target", &[POWER, brightness])).unwrap();
    assert!(!result.protocol_error);
    assert_eq!(
        result
            .items
            .into_iter()
            .map(|item| item.map(|reading| reading.value))
            .collect::<Vec<_>>(),
        vec![Ok(json!(true)), Ok(json!(35.5))]
    );
}

#[test]
fn general_property_writes_reject_invalid_inputs_without_sending() {
    let (base, requests) = mock_server(vec![MockResponse::json(200, "{}")]);
    let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    for items in [
        vec![],
        vec![(POWER, json!(true)), (POWER, json!(false))],
        vec![(POWER, Value::Null)],
        vec![(POWER, json!("1"))],
        vec![(POWER, json!([]))],
    ] {
        let error = future::block_on(cloud.set_properties("token", "target", &items)).unwrap_err();
        assert_eq!(error.kind(), &CloudErrorKind::InvalidInput);
    }
    assert!(requests.try_recv().is_err());
}

#[test]
fn property_requests_use_official_paths_and_correlated_boolean_results() {
    let (base, requests) = mock_server(vec![
        MockResponse::json(
            200,
            r#"{"code":0,"result":[{"did":"target","siid":2,"piid":1,"code":0,"value":false}]}"#,
        ),
        MockResponse::json(
            200,
            r#"{"code":0,"result":[{"did":"target","siid":2,"piid":1,"code":0}]}"#,
        ),
        MockResponse::json(
            200,
            r#"{"code":0,"result":[{"did":"target","siid":2,"piid":1,"code":1}]}"#,
        ),
    ]);
    let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    assert_eq!(
        future::block_on(cloud.read_property_value("access-secret", "target", POWER)).unwrap(),
        json!(false)
    );
    future::block_on(cloud.set_property("access-secret", "target", POWER, true)).unwrap();
    future::block_on(cloud.set_property("access-secret", "target", POWER, false)).unwrap();
    for (path, body) in [
        (
            "/app/v2/miotspec/prop/get",
            json!({"datasource":1,"params":[{"did":"target","siid":2,"piid":1}]}),
        ),
        (
            "/app/v2/miotspec/prop/set",
            json!({"params":[{"did":"target","siid":2,"piid":1,"value":true}]}),
        ),
        (
            "/app/v2/miotspec/prop/set",
            json!({"params":[{"did":"target","siid":2,"piid":1,"value":false}]}),
        ),
    ] {
        let request = requests.recv().unwrap();
        assert_eq!(request.target, path);
        assert_eq!(serde_json::from_str::<Value>(&request.body).unwrap(), body);
        let headers = request.headers.to_ascii_lowercase();
        assert!(headers.starts_with("post "));
        assert!(headers.contains("authorization: beareraccess-secret"));
        assert!(headers.contains("x-client-bizid: haapi"));
        assert!(headers.contains(&format!("x-client-appid: {CLIENT_ID}")));
    }
}

#[test]
fn property_reads_accept_boolean_and_numeric_values_with_successful_item_codes() {
    for value in [json!(true), json!(false), json!(0), json!(1), json!(2.5)] {
        let body =
            json!({"code":0,"result":[{"did":"target","siid":2,"piid":1,"code":0,"value":value}]})
                .to_string();
        let (base, _) = mock_server(vec![MockResponse::json(200, &body)]);
        let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
        assert_eq!(
            future::block_on(cloud.read_property_value("token", "target", POWER)).unwrap(),
            value
        );
    }
    for value in [Value::Null, json!("true"), json!([]), json!({})] {
        let body =
            json!({"code":0,"result":[{"did":"target","siid":2,"piid":1,"code":0,"value":value}]})
                .to_string();
        let (base, _) = mock_server(vec![MockResponse::json(200, &body)]);
        let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
        assert_eq!(
            future::block_on(cloud.read_property_value("token", "target", POWER))
                .unwrap_err()
                .kind(),
            &CloudErrorKind::Protocol
        );
    }
    let (base, _) = mock_server(vec![MockResponse::json(
        200,
        r#"{"code":0,"result":[{"did":"target","siid":2,"piid":1,"code":0}]}"#,
    )]);
    let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    assert_eq!(
        future::block_on(cloud.read_property_value("token", "target", POWER))
            .unwrap_err()
            .kind(),
        &CloudErrorKind::Protocol
    );
    let (base, _) = mock_server(vec![MockResponse::json(
        200,
        r#"{"code":0,"result":[{"did":"target","siid":2,"piid":1,"code":1,"value":true}]}"#,
    )]);
    let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    assert_eq!(
        future::block_on(cloud.read_property_value("token", "target", POWER))
            .unwrap_err()
            .kind(),
        &CloudErrorKind::PropertyRejected(1)
    );
}

#[test]
fn property_operations_reject_invalid_envelopes_and_unassociated_results() {
    let item = json!({"did":"target","siid":2,"piid":1,"code":0,"value":true});
    let mut invalid_items = vec![
        json!([]),
        json!([item.clone(), item.clone()]),
        json!([item.clone(), {"did":"other","siid":2,"piid":1,"code":0,"value":true}]),
    ];
    for (field, value) in [
        ("did", json!("other")),
        ("did", json!(123)),
        ("siid", json!(3)),
        ("piid", json!(2)),
        ("siid", json!("2")),
        ("piid", json!(1.0)),
        ("code", json!(true)),
        ("code", json!("0")),
    ] {
        let mut invalid = item.clone();
        invalid[field] = value;
        invalid_items.push(json!([invalid]));
    }
    for field in ["did", "siid", "piid", "code"] {
        let mut invalid = item.clone();
        invalid.as_object_mut().unwrap().remove(field);
        invalid_items.push(json!([invalid]));
    }
    let mut bodies = vec![
        json!({}),
        json!({"code":0}),
        json!({"code":0,"result":null}),
        json!({"code":0,"result":{}}),
        json!({"code":"0","result":[item.clone()]}),
    ];
    bodies.extend(
        invalid_items
            .into_iter()
            .map(|result| json!({"code":0,"result":result})),
    );
    for body in bodies {
        let (base, requests) = mock_server(vec![
            MockResponse::json(200, &body.to_string()),
            MockResponse::json(200, &body.to_string()),
        ]);
        let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
        let errors = [
            future::block_on(cloud.read_property_value("token", "target", POWER)).unwrap_err(),
            future::block_on(cloud.set_property("token", "target", POWER, true)).unwrap_err(),
        ];
        for error in errors {
            assert_eq!(error.kind(), &CloudErrorKind::Protocol, "{body}");
            assert!(!error.is_temporary());
            assert!(!error.is_device_offline());
            assert!(!error.is_device_forbidden());
        }
        assert_eq!(requests.try_iter().count(), 2);
    }
}

#[test]
fn property_failures_preserve_official_business_codes_and_failure_scope() {
    for (code, unauthorized, offline, forbidden) in [
        (-704_042_011, false, true, false),
        (-704_010_000, false, false, true),
        (-704_012_904, false, false, true),
        (-704_012_905, false, false, true),
        (-704_012_901, true, false, false),
        (-704_012_902, true, false, false),
        (-704_012_903, true, false, false),
        (-704_030_023, false, false, false),
        (-704_220_043, false, false, false),
        (-704_083_036, false, false, false),
        (17, false, false, false),
    ] {
        let body = json!({"code":0,"result":[{"did":"target","siid":2,"piid":1,"code":code,"message":"response-secret"}]}).to_string();
        let (base, requests) = mock_server(vec![
            MockResponse::json(200, &body),
            MockResponse::json(200, &body),
        ]);
        let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
        for error in [
            future::block_on(cloud.read_property_value("access-secret", "target", POWER))
                .unwrap_err(),
            future::block_on(cloud.set_property("access-secret", "target", POWER, true))
                .unwrap_err(),
        ] {
            assert_eq!(error.kind(), &CloudErrorKind::PropertyRejected(code));
            assert_eq!(error.is_unauthorized(), unauthorized);
            assert_eq!(error.is_device_offline(), offline);
            assert_eq!(error.is_device_forbidden(), forbidden);
            assert!(!error.is_temporary());
            assert!(!format!("{error:?} {error}").contains("secret"));
        }
        assert_eq!(requests.try_iter().count(), 2);
    }
}

#[test]
fn property_http_failures_are_classified_without_automatic_retries() {
    for (status, temporary, unauthorized) in [
        (401, false, true),
        (403, false, false),
        (408, true, false),
        (429, true, false),
        (500, true, false),
        (503, true, false),
        (400, false, false),
    ] {
        let (base, requests) = mock_server(vec![
            MockResponse::json(status, "response-secret"),
            MockResponse::json(200, r#"{"code":0,"result":[]}"#),
        ]);
        let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
        let error = future::block_on(cloud.set_property("access-secret", "target", POWER, true))
            .unwrap_err();
        assert_eq!(error.is_temporary(), temporary);
        assert_eq!(error.is_unauthorized(), unauthorized);
        assert!(!error.is_device_offline());
        assert!(!error.is_device_forbidden());
        assert!(!format!("{error:?} {error}").contains("secret"));
        requests.recv().unwrap();
        assert!(requests.recv_timeout(Duration::from_millis(20)).is_err());
    }
}

#[test]
fn property_outer_rejections_remain_distinct_from_correlated_device_rejections() {
    for code in [-704_012_901, -704_042_011, -704_010_000, -704_012_904, 1] {
        let (base, _) = mock_server(vec![MockResponse::json(
            200,
            &json!({"code":code,"message":"secret"}).to_string(),
        )]);
        let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
        let error =
            future::block_on(cloud.set_property("token", "target", POWER, true)).unwrap_err();
        assert_eq!(error.kind(), &CloudErrorKind::Business(code));
        assert_eq!(error.is_unauthorized(), code == -704_012_901);
        assert!(!error.is_device_offline());
        assert!(!error.is_device_forbidden());
    }
}

#[test]
fn property_timeouts_cover_headers_and_body_and_do_not_replay_writes() {
    for response in [
        MockResponse::json(
            200,
            r#"{"code":0,"result":[{"did":"target","siid":2,"piid":1,"code":0}]}"#,
        )
        .delayed(Duration::from_millis(100)),
        MockResponse::delayed_body(
            200,
            r#"{"code":0,"result":[{"did":"target","siid":2,"piid":1,"code":0}]}"#,
            5,
            Duration::from_millis(100),
        ),
    ] {
        let (base, requests) = mock_server(vec![response, MockResponse::json(200, "{}")]);
        let cloud = CloudClient::for_test(&base, Duration::from_millis(10)).unwrap();
        let error =
            future::block_on(cloud.set_property("token", "target", POWER, true)).unwrap_err();
        assert!(error.is_timeout());
        assert!(error.is_temporary());
        requests.recv().unwrap();
        assert!(requests.recv_timeout(Duration::from_millis(150)).is_err());
    }
    let (base, _) = mock_server(vec![
        MockResponse::json(200, "{}").delayed(Duration::from_millis(100)),
    ]);
    let cloud = CloudClient::for_test(&base, Duration::from_millis(10)).unwrap();
    assert!(
        future::block_on(cloud.read_property_value("token", "target", POWER))
            .unwrap_err()
            .is_timeout()
    );
}

#[test]
fn invalid_property_targets_fail_before_sending_http() {
    let (base, requests) = mock_server(vec![MockResponse::json(200, "{}")]);
    let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    for (token, did, binding) in [
        ("", "target", POWER),
        ("token", "", POWER),
        ("token", "bad\n", POWER),
        ("token", "target", PropertyBinding { siid: 0, piid: 1 }),
        ("token", "target", PropertyBinding { siid: 2, piid: 0 }),
    ] {
        for error in [
            future::block_on(cloud.read_property_value(token, did, binding)).unwrap_err(),
            future::block_on(cloud.set_property(token, did, binding, true)).unwrap_err(),
        ] {
            assert_eq!(error.kind(), &CloudErrorKind::InvalidInput);
        }
    }
    assert!(requests.try_recv().is_err());
}

#[test]
fn property_reads_preserve_a_supplied_upstream_update_time() {
    let (base, _) = mock_server(vec![
        MockResponse::json(
            200,
            r#"{"code":0,"result":[{"did":"target","siid":2,"piid":1,"code":0,"value":21.5,"updateTime":1700000100}]}"#,
        ),
        MockResponse::json(
            200,
            r#"{"code":0,"result":[{"did":"target","siid":2,"piid":1,"code":0,"value":22.5}]}"#,
        ),
    ]);
    let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    let reading =
        future::block_on(cloud.read_property_observation("token", "target", POWER)).unwrap();
    assert_eq!(reading.value, json!(21.5));
    assert_eq!(reading.update_time, Some(1_700_000_100));
    let reading =
        future::block_on(cloud.read_property_observation("token", "target", POWER)).unwrap();
    assert_eq!(reading.value, json!(22.5));
    assert_eq!(reading.update_time, None);
}
