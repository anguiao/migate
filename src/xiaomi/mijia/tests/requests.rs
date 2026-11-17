use super::super::rpc::RpcOutcome;
use super::*;

const POWER: PropertyBinding = PropertyBinding { siid: 2, piid: 1 };
const LEVEL: PropertyBinding = PropertyBinding { siid: 2, piid: 2 };

#[test]
fn realtime_reads_and_writes_keep_correlated_partial_results_without_replays() {
    let (base, requests) = mock_server(vec![
        response(json!([
            {"did":"device","siid":2,"piid":2,"code":-704042011},
            {"did":"device","siid":2,"piid":1,"code":0,"value":true,"updateTime":1700000000}
        ])),
        response(json!([
            {"did":"device","siid":2,"piid":2,"code":-704},
            {"did":"device","siid":2,"piid":1,"code":1}
        ])),
    ]);
    let client = client(&base, Duration::from_secs(1));
    let session = session();
    let reads = block_on(client.read_properties(&session, "device", &[POWER, LEVEL])).unwrap();
    assert!(!reads.protocol_error);
    let value = reads.items[0].as_ref().unwrap();
    assert_eq!(value.value, json!(true));
    assert_eq!(value.update_time, Some(1700000000));
    assert_eq!(
        reads.items[1].as_ref().unwrap_err().kind(),
        &CloudErrorKind::PropertyRejected(-704042011)
    );
    let writes = block_on(client.set_properties(
        &session,
        "device",
        &[(POWER, json!(true)), (LEVEL, json!(50))],
    ))
    .unwrap();
    assert_eq!(writes.items[0], Ok(()));
    assert_eq!(
        writes.items[1].as_ref().unwrap_err().kind(),
        &CloudErrorKind::PropertyRejected(-704)
    );
    let read = requests.recv().unwrap();
    let write = requests.recv().unwrap();
    assert_signed(&read);
    assert_signed(&write);
    assert_eq!(read.target, "/app/miotspec/prop/get");
    assert_eq!(
        data(&read),
        json!({"datasource":2,"params":[{"did":"device","siid":2,"piid":1},{"did":"device","siid":2,"piid":2}]})
    );
    assert_eq!(write.target, "/app/miotspec/prop/set");
    assert_eq!(
        data(&write),
        json!({"params":[{"did":"device","siid":2,"piid":1,"value":true},{"did":"device","siid":2,"piid":2,"value":50}]})
    );
    assert!(requests.recv_timeout(Duration::from_millis(30)).is_err());
}

#[test]
fn authentication_protocol_and_http_errors_remain_distinct() {
    for (status, body, expected) in [
        (401, "{}", CloudErrorKind::Unauthorized),
        (
            426,
            r#"{"code":0,"message":"SERVICETOKEN_EXPIRED"}"#,
            CloudErrorKind::Unauthorized,
        ),
        (
            426,
            r#"{"code":0,"message":"upgrade required"}"#,
            CloudErrorKind::HttpStatus(426),
        ),
        (426, "Upgrade Required", CloudErrorKind::HttpStatus(426)),
        (
            200,
            r#"{"code":3,"message":"SERVICETOKEN_EXPIRED"}"#,
            CloudErrorKind::Unauthorized,
        ),
        (
            200,
            r#"{"code":2,"message":"auth err"}"#,
            CloudErrorKind::Unauthorized,
        ),
        (
            200,
            r#"{"code":2,"message":"invalid signature"}"#,
            CloudErrorKind::Protocol,
        ),
        (
            200,
            r#"{"code":-6,"message":"service+token/secret"}"#,
            CloudErrorKind::Business(-6),
        ),
        (200, r#"{"code":0,"result":{}}"#, CloudErrorKind::Protocol),
        (429, "{}", CloudErrorKind::HttpStatus(429)),
        (302, "{}", CloudErrorKind::HttpStatus(302)),
    ] {
        let (base, requests) =
            mock_server(vec![MockResponse::json(status, body), response(json!([]))]);
        let client = client(&base, Duration::from_secs(1));
        let error = block_on(client.read_properties(&session(), "device", &[POWER])).unwrap_err();
        assert_eq!(error.kind(), &expected, "{body}");
        assert_eq!(
            error.is_unauthorized(),
            expected == CloudErrorKind::Unauthorized
        );
        assert_eq!(
            error.is_temporary(),
            expected == CloudErrorKind::HttpStatus(429)
        );
        if body.contains("invalid signature") {
            assert!(error.to_string().contains("signature"));
        }
        let diagnostic = format!("{error:?} {error}");
        assert!(!diagnostic.contains("service+token/secret"));
        assert!(!diagnostic.contains("MDEyMzQ1Njc4OWFiY2RlZg=="));
        requests.recv().unwrap();
        assert!(requests.recv_timeout(Duration::from_millis(30)).is_err());
    }
}

#[test]
fn action_encodes_ordered_native_inputs_and_preserves_correlated_code_and_output() {
    let (base, requests) = mock_server(vec![
        response(
            json!({"did":"device","siid":3,"aiid":4,"code":0,"out":[{"piid":8,"value":"A&B + /中文"}]}),
        ),
        response(json!({"did":"device","siid":3,"aiid":4,"code":-704,"out":[]})),
        response(json!({"did":"wrong","siid":3,"aiid":4,"code":0,"out":[]})),
    ]);
    let client = client(&base, Duration::from_secs(1));
    let session = session();
    let input = vec![
        json!({"piid":2,"value":"A&B + /中文"}),
        json!({"piid":1,"value":42}),
    ];
    let result = block_on(client.action(&session, "device", 3, 4, input.clone())).unwrap();
    assert_eq!(result.code, 0);
    assert_eq!(
        result.result["out"],
        json!([{"piid":8,"value":"A&B + /中文"}])
    );
    let rejection = block_on(client.action(&session, "device", 3, 4, input.clone())).unwrap();
    assert_eq!(rejection.code, -704);
    let error = block_on(client.action(&session, "device", 3, 4, input.clone())).unwrap_err();
    assert_eq!(error.kind(), &CloudErrorKind::Protocol);
    let first = requests.recv().unwrap();
    assert_signed(&first);
    assert_eq!(first.target, "/app/miotspec/action");
    assert_eq!(
        data(&first),
        json!({"params":{"did":"device","siid":3,"aiid":4,"in":input}})
    );
    assert!(first.body.contains("%26"));
    assert!(first.body.contains("%2B"));
    assert!(first.body.contains("%2F"));
    assert!(first.body.contains("%E4%B8%AD"));
    requests.recv().unwrap();
    requests.recv().unwrap();
    assert!(requests.recv_timeout(Duration::from_millis(30)).is_err());
}

#[test]
fn rpc_encodes_single_request_and_correlates_native_result_and_error() {
    let (base, requests) = dynamic_mock_server(3, |request| {
        let payload = data(request);
        let id = payload["id"].as_u64().unwrap();
        match payload["method"].as_str().unwrap() {
            "get_prop" => MockResponse::json(200, &json!({"code":0,"id":id,"result":["on",23]}).to_string()),
            "set_power" => MockResponse::json(200, &json!({"code":0,"id":id,"error":{"code":-123,"message":"service+token/secret","extra":{"detail":7}}}).to_string()),
            _ => MockResponse::json(200, &json!({"code":0,"id":id+1,"result":["bad"]}).to_string()),
        }
    });
    let client = client(&base, Duration::from_secs(1));
    let session = session();
    assert_eq!(
        block_on(client.rpc(&session, "device/one", "get_prop", json!(["power", "temp"]))).unwrap(),
        RpcOutcome::Result(json!(["on", 23]))
    );
    let rejected =
        block_on(client.rpc(&session, "device/one", "set_power", json!(["off"]))).unwrap();
    assert_eq!(
        rejected,
        RpcOutcome::Error(
            json!({"code":-123,"message":"service+token/secret","extra":{"detail":7}})
        )
    );
    assert!(!format!("{rejected:?}").contains("service+token/secret"));
    let error = block_on(client.rpc(&session, "device/one", "invalid", json!([]))).unwrap_err();
    assert_eq!(error.kind(), &CloudErrorKind::Protocol);
    let first = requests.recv().unwrap();
    assert_signed(&first);
    assert_eq!(first.target, "/app/home/rpc/device%2Fone");
    assert_eq!(data(&first)["method"], "get_prop");
    assert_eq!(data(&first)["params"], json!(["power", "temp"]));
    assert!(data(&first)["id"].as_u64().is_some());
    let second = requests.recv().unwrap();
    assert_ne!(data(&first)["id"], data(&second)["id"]);
    requests.recv().unwrap();
    assert!(requests.recv_timeout(Duration::from_millis(30)).is_err());
}

#[test]
fn action_and_rpc_failures_are_never_replayed() {
    for action in [true, false] {
        let (base, requests) = mock_server(vec![
            MockResponse::json(200, r#"{"code":3,"message":"SERVICETOKEN_EXPIRED"}"#),
            response(json!({"code":0})),
        ]);
        let client = client(&base, Duration::from_secs(1));
        let error = if action {
            block_on(client.action(&session(), "device", 1, 1, vec![])).unwrap_err()
        } else {
            block_on(client.rpc(&session(), "device", "set_power", json!(["on"]))).unwrap_err()
        };
        assert!(error.is_unauthorized());
        let request = requests.recv().unwrap();
        assert_signed(&request);
        assert!(requests.recv_timeout(Duration::from_millis(50)).is_err());
    }
}

#[test]
fn redirect_with_location_is_not_followed() {
    let redirect = MockResponse::raw(
        "HTTP/1.1 302 Found\r\nLocation: /followed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    );
    let (base, requests) = mock_server(vec![redirect, response(json!([]))]);
    let client = client(&base, Duration::from_secs(1));
    let error = block_on(client.read_properties(&session(), "device", &[POWER])).unwrap_err();
    assert_eq!(error.kind(), &CloudErrorKind::HttpStatus(302));
    requests.recv().unwrap();
    assert!(requests.recv_timeout(Duration::from_millis(50)).is_err());
}
