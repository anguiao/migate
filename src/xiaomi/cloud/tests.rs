use super::*;
use crate::xiaomi::test_support::{MockResponse, dynamic_mock_server, mock_server};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures_lite::future;
use serde_json::{Value, json};
use std::time::Duration;
use url::Url;

#[test]
fn authorization_state_matches_upstream_device_binding() {
    for (uuid, expected_state) in [
        (
            "550e8400-e29b-41d4-a716-446655440000",
            "139e3d81ebea72caaec5f4bb548d66e83ecce75c",
        ),
        (
            "550e8400-e29b-41d4-a716-446655440001",
            "510dfb7675e97409da88b563551ea8d59fdc43f6",
        ),
    ] {
        let attempt = AuthorizationAttempt::new(Some(uuid)).unwrap();
        let url = Url::parse(attempt.authorization_url()).unwrap();
        let query = url
            .query_pairs()
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(query["device_id"], format!("ha.{uuid}"));
        assert_eq!(query["state"], expected_state);
        assert_eq!(attempt.state(), expected_state);
    }
}

#[test]
fn authorization_callbacks_are_bound_to_unique_paths_and_expected_state() {
    let uuid = "550e8400-e29b-41d4-a716-446655440000";
    let first = AuthorizationAttempt::new(Some(uuid)).unwrap();
    let second = AuthorizationAttempt::new(Some(uuid)).unwrap();
    assert_eq!(first.oauth_client_uuid(), uuid);
    assert_eq!(first.state(), second.state());
    assert_ne!(first.redirect_uri(), second.redirect_uri());
    let url = Url::parse(first.authorization_url()).unwrap();
    let query = url
        .query_pairs()
        .collect::<std::collections::HashMap<_, _>>();
    assert_eq!(query["client_id"], CLIENT_ID.to_string());
    assert_eq!(query["response_type"], "code");
    assert_eq!(query["device_id"], format!("ha.{uuid}"));
    assert_eq!(query["skip_confirm"], "false");

    let callback = format!(
        "{}?code=secret-code&state={}",
        first.redirect_uri(),
        first.state()
    );
    assert_eq!(
        first.parse_callback(&format!(" \n{callback}\t")).unwrap(),
        "secret-code"
    );
    for invalid in [
        callback.replacen("http://", "https://", 1),
        callback.replacen("homeassistant.local", "example.invalid", 1),
        callback.replacen(":8123", ":8124", 1),
        callback.replacen("/api/webhook/", "/wrong/", 1),
        format!(
            "{}?code=x&state={}&state={}",
            first.redirect_uri(),
            first.state(),
            first.state()
        ),
        format!("{}?code=&state={}", first.redirect_uri(), first.state()),
        format!("{}?code=x&state=", first.redirect_uri()),
        format!(
            "{}?code=x&code=y&state={}",
            first.redirect_uri(),
            first.state()
        ),
        format!("{}?code=x&state=wrong", first.redirect_uri()),
        format!(
            "{}?error=denied&state={}",
            first.redirect_uri(),
            first.state()
        ),
        format!(
            "{}?code=x&state={}#fragment",
            first.redirect_uri(),
            first.state()
        ),
        callback.replacen("http://", "http://user@", 1),
        format!("{}?code=x&state={}", second.redirect_uri(), second.state()),
    ] {
        let error = first.parse_callback(&invalid).unwrap_err();
        let diagnostic = format!("{error:?} {error}");
        assert!(!diagnostic.contains(&invalid));
        assert!(!diagnostic.contains(first.state()));
    }
}

#[test]
fn token_requests_match_wire_protocol() {
    let body = r#"{"code":0,"result":{"access_token":"access-secret","refresh_token":"refresh-secret","expires_in":1000}}"#;
    let (base, requests) = mock_server(vec![
        MockResponse::json(200, body),
        MockResponse::json(200, body),
    ]);
    let client = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    let attempt = AuthorizationAttempt::new(Some("550e8400-e29b-41d4-a716-446655440000")).unwrap();
    let authorization_url = Url::parse(attempt.authorization_url()).unwrap();
    let authorization_query = authorization_url
        .query_pairs()
        .collect::<std::collections::HashMap<_, _>>();
    let exchanged = future::block_on(client.exchange_token(&attempt, "code-secret")).unwrap();
    let refreshed = future::block_on(client.refresh_token(
        attempt.oauth_client_uuid(),
        attempt.redirect_uri(),
        "refresh-input",
    ))
    .unwrap();
    assert_eq!(exchanged.access_token, "access-secret");
    assert_eq!(exchanged.refresh_token, "refresh-secret");
    assert_eq!(exchanged.expires_in, 1000);
    assert!(!format!("{exchanged:?}").contains("secret"));
    for (request, expected_key) in [
        (requests.recv().unwrap(), "code"),
        (requests.recv().unwrap(), "refresh_token"),
    ] {
        assert!(
            request
                .target
                .starts_with("/app/v2/ha/oauth/get_token?data=")
        );
        let url = Url::parse(&format!("http://local{}", request.target)).unwrap();
        let data = url.query_pairs().find(|(key, _)| key == "data").unwrap().1;
        let data: Value = serde_json::from_str(&data).unwrap();
        assert_eq!(data["client_id"], json!(2_882_303_761_520_251_711_u64));
        assert_eq!(
            data["redirect_uri"].as_str().unwrap(),
            authorization_query["redirect_uri"]
        );
        if expected_key == "code" {
            assert_eq!(
                data["device_id"].as_str().unwrap(),
                authorization_query["device_id"]
            );
        }
        assert!(data.get(expected_key).is_some());
        assert!(data.get("grant_type").is_none());
    }
    assert_eq!(refreshed.expires_in, 1000);
}

#[test]
fn invalid_token_responses_are_rejected_at_the_cloud_boundary() {
    for result in [
        json!({"access_token":"","refresh_token":"refresh","expires_in":1}),
        json!({"access_token":"access","refresh_token":"","expires_in":1}),
        json!({"access_token":" access","refresh_token":"refresh","expires_in":1}),
        json!({"access_token":"access","refresh_token":"refresh\nsecret","expires_in":1}),
        json!({"access_token":"access","refresh_token":"refresh","expires_in":0}),
        json!({"access_token":"access","refresh_token":"refresh","expires_in":-1}),
    ] {
        let body = json!({"code":0,"result":result}).to_string();
        let (base, _) = mock_server(vec![MockResponse::json(200, &body)]);
        let client = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
        let attempt = AuthorizationAttempt::new(None).unwrap();
        let error = future::block_on(client.exchange_token(&attempt, "code")).unwrap_err();
        assert_eq!(error.kind(), &CloudErrorKind::Protocol);
        assert!(!format!("{error:?} {error}").contains("secret"));
    }
}

#[test]
fn token_errors_preserve_oauth_codes_without_response_text() {
    for (oauth_code, expected) in [
        (
            96002,
            "cloud business code -6 (OAuth error 96002: missing or invalid request parameters)",
        ),
        (
            96013,
            "cloud business code -6 (OAuth error 96013: invalid authorization code)",
        ),
        (99999, "cloud business code -6 (OAuth error 99999)"),
    ] {
        let message = json!({
            "error": oauth_code,
            "error_description": "response-secret\nhttps://example.invalid/?code=secret",
            "traceId": "trace-secret",
            "access_token": "access-secret",
        })
        .to_string();
        let body = json!({"code": -6, "message": message}).to_string();
        let (base, _) = mock_server(vec![
            MockResponse::json(200, &body),
            MockResponse::json(200, &body),
        ]);
        let client = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
        let attempt = AuthorizationAttempt::new(None).unwrap();
        let errors = [
            future::block_on(client.exchange_token(&attempt, "code-secret")).unwrap_err(),
            future::block_on(client.refresh_token(
                attempt.oauth_client_uuid(),
                attempt.redirect_uri(),
                "refresh-secret",
            ))
            .unwrap_err(),
        ];
        for (error, operation) in errors
            .into_iter()
            .zip(["exchange authorization code", "refresh access token"])
        {
            assert_eq!(error.kind(), &CloudErrorKind::Business(-6));
            assert!(!error.is_unauthorized());
            assert_eq!(
                error.to_string(),
                format!("Failed to {operation}: {expected}")
            );
            assert!(!format!("{error:?} {error}").contains("secret"));
        }
    }
}

#[test]
fn unusable_oauth_details_preserve_the_outer_business_error() {
    for message in [
        Value::Null,
        json!("response-secret"),
        json!({"error": 96013, "error_description": "response-secret"}),
        json!(r#"{"error_description":"response-secret"}"#),
        json!(r#"{"error":"96013","error_description":"response-secret"}"#),
        json!(r#"{"error":96013.5,"error_description":"response-secret"}"#),
        json!(r#"{"error":true,"error_description":"response-secret"}"#),
    ] {
        let body = json!({"code": -6, "message": message}).to_string();
        let (base, _) = mock_server(vec![MockResponse::json(200, &body)]);
        let client = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
        let attempt = AuthorizationAttempt::new(None).unwrap();
        let error = future::block_on(client.exchange_token(&attempt, "code-secret")).unwrap_err();
        assert_eq!(error.kind(), &CloudErrorKind::Business(-6));
        assert_eq!(
            error.to_string(),
            "Failed to exchange authorization code: cloud business code -6"
        );
        assert!(!format!("{error:?} {error}").contains("secret"));
    }
}

#[test]
fn oauth_details_do_not_override_http_or_protocol_errors() {
    let message = json!({"error": 96013, "error_description": "response-secret"}).to_string();
    for (status, body, expected) in [
        (
            401,
            json!({"code": -6, "message": message}),
            CloudErrorKind::Unauthorized,
        ),
        (
            403,
            json!({"code": -6, "message": message}),
            CloudErrorKind::HttpStatus(403),
        ),
        (200, json!({"message": message}), CloudErrorKind::Protocol),
        (
            200,
            json!({"code": 0, "message": message}),
            CloudErrorKind::Protocol,
        ),
    ] {
        let (base, _) = mock_server(vec![MockResponse::json(status, &body.to_string())]);
        let client = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
        let attempt = AuthorizationAttempt::new(None).unwrap();
        let error = future::block_on(client.exchange_token(&attempt, "code-secret")).unwrap_err();
        assert_eq!(error.kind(), &expected);
        assert!(!error.to_string().contains("OAuth"));
        assert!(!format!("{error:?} {error}").contains("secret"));
    }
}

#[test]
fn protected_requests_do_not_interpret_oauth_details() {
    let body = json!({
        "code": -6,
        "message": json!({"error": 96013, "error_description": "response-secret"}).to_string(),
    })
    .to_string();
    let (base, _) = mock_server(vec![
        MockResponse::json(200, &body),
        MockResponse::json(200, &body),
    ]);
    let client = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    for error in [
        future::block_on(client.get_home("access-secret")).unwrap_err(),
        future::block_on(client.get_certificate("access-secret", "csr")).unwrap_err(),
    ] {
        assert_eq!(error.kind(), &CloudErrorKind::Business(-6));
        assert!(!error.is_unauthorized());
        assert!(!error.to_string().contains("OAuth"));
        assert!(!format!("{error:?} {error}").contains("secret"));
    }
}

#[test]
fn home_and_device_requests_are_minimal_and_bounded() {
    let dids = (0..160)
        .map(|index| format!("did-{index}"))
        .collect::<Vec<_>>();
    let home = json!({"code":0,"result":{"homelist":[{"uid":123,"dids":dids,"roomlist":[{"dids":["did-0","room-only"]}]}],"share_home_list":[{"uid":"shared"}],"has_more":true}});
    let (base, requests) = mock_server(vec![
        MockResponse::json(200, &home.to_string()),
        MockResponse::json(200, r#"{"code":0,"result":{"list":[],"has_more":true}}"#),
    ]);
    let client = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    let page = future::block_on(client.get_home("access-secret")).unwrap();
    assert_eq!(page.uid.as_deref(), Some("123"));
    assert_eq!(page.dids.len(), 150);
    future::block_on(client.get_devices("access-secret", &page.dids)).unwrap();
    let home_request = requests.recv().unwrap();
    assert!(home_request.target.ends_with("/app/v2/homeroom/gethome"));
    assert!(
        home_request
            .headers
            .to_ascii_lowercase()
            .contains("authorization: beareraccess-secret")
    );
    assert_eq!(
        serde_json::from_str::<Value>(&home_request.body).unwrap(),
        json!({"limit":150,"fetch_share":false,"fetch_share_dev":false,"plat_form":0,"app_ver":9})
    );
    let device_request = requests.recv().unwrap();
    let body: Value = serde_json::from_str(&device_request.body).unwrap();
    assert_eq!(body["dids"].as_array().unwrap().len(), 150);
    assert_eq!(body["limit"], 200);
}

#[test]
fn floating_point_uid_is_a_protocol_error() {
    let (base, _) = mock_server(vec![MockResponse::json(
        200,
        r#"{"code":0,"result":{"homelist":[{"uid":12.5}]}}"#,
    )]);
    let client = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    let error = future::block_on(client.get_home("access")).unwrap_err();
    assert_eq!(error.kind(), &CloudErrorKind::Protocol);
}

#[test]
fn owned_catalog_rejects_control_characters_at_the_cloud_boundary() {
    let bad_home = json!({"code":0,"result":{"homelist":[{
        "id":"1","uid":"42","name":"Bad\nHome","dids":[],"roomlist":[]
    }],"has_more":false}});
    let (base, _) = mock_server(vec![MockResponse::json(200, &bad_home.to_string())]);
    let client = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    assert_eq!(
        future::block_on(client.get_owned_catalog("access"))
            .unwrap_err()
            .kind(),
        &CloudErrorKind::Protocol
    );

    let home = r#"{"code":0,"result":{"homelist":[{"id":"1","uid":"42","name":"Home","dids":["a"],"roomlist":[]}],"has_more":false}}"#;
    let bad_device = json!({"code":0,"result":{"list":[{
        "did":"a","uid":"42","name":"Bad\nDevice","model":"vendor.light.x"
    }],"has_more":false}});
    let (base, _) = mock_server(vec![
        MockResponse::json(200, home),
        MockResponse::json(200, &bad_device.to_string()),
    ]);
    let client = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    assert_eq!(
        future::block_on(client.get_owned_catalog("access"))
            .unwrap_err()
            .kind(),
        &CloudErrorKind::Protocol
    );
}

#[test]
fn cloud_errors_are_classified_and_sanitized() {
    for (status, body, unauthorized) in [
        (401, "token-secret", true),
        (403, "forbidden-secret", false),
        (200, r#"{"code":-7,"message":"business-secret"}"#, false),
        (200, "protocol-secret", false),
    ] {
        let (base, _) = mock_server(vec![MockResponse::json(status, body)]);
        let client = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
        let error = future::block_on(client.get_home("access-secret")).unwrap_err();
        assert_eq!(error.is_unauthorized(), unauthorized);
        let diagnostic = format!("{error:?} {error}");
        assert!(!diagnostic.contains("secret"));
    }
}

#[test]
fn certificate_request_sends_only_base64_csr_and_empty_devices_skip_http() {
    let (base, requests) = mock_server(vec![MockResponse::json(
        200,
        r#"{"code":0,"result":{"cert":"certificate-pem"}}"#,
    )]);
    let client = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    future::block_on(client.get_devices("access", &[])).unwrap();
    assert_eq!(
        future::block_on(client.get_certificate("access", "csr-pem")).unwrap(),
        "certificate-pem"
    );
    let request = requests.recv().unwrap();
    assert!(request.target.ends_with("/app/v2/ha/oauth/get_central_crt"));
    let body: Value = serde_json::from_str(&request.body).unwrap();
    assert_eq!(
        STANDARD.decode(body["csr"].as_str().unwrap()).unwrap(),
        b"csr-pem"
    );
}

#[test]
fn requests_time_out_and_do_not_follow_redirects() {
    let (base, _) = mock_server(vec![
        MockResponse::json(200, r#"{"code":0,"result":{"homelist":[]}}"#)
            .delayed(Duration::from_millis(100)),
    ]);
    let client = CloudClient::for_test(&base, Duration::from_millis(10)).unwrap();
    assert!(
        future::block_on(client.get_home("access"))
            .unwrap_err()
            .is_timeout()
    );

    let (base, _) = mock_server(vec![MockResponse::delayed_body(
        200,
        r#"{"code":0,"result":{"homelist":[]}}"#,
        5,
        Duration::from_millis(100),
    )]);
    let client = CloudClient::for_test(&base, Duration::from_millis(10)).unwrap();
    assert!(
        future::block_on(client.get_home("access"))
            .unwrap_err()
            .is_timeout()
    );

    let redirect = "HTTP/1.1 302 Found\r\nLocation: /followed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    let (base, _) = mock_server(vec![MockResponse::raw(redirect)]);
    let client = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    assert_eq!(
        future::block_on(client.get_home("access"))
            .unwrap_err()
            .http_status(),
        Some(302)
    );
}

#[test]
fn owned_catalog_reads_all_home_and_device_pages_without_shared_flags() {
    let (base, requests) = mock_server(vec![
        MockResponse::json(
            200,
            r#"{"code":0,"result":{"homelist":[{"id":1,"uid":42,"name":"Home","dids":["a"],"roomlist":[{"id":10,"name":"Room","dids":["a"]}]}],"share_home_list":[{"id":9,"uid":99,"name":"Shared","dids":["x"],"roomlist":[]}],"has_more":true,"max_id":"h1"}}"#,
        ),
        MockResponse::json(
            200,
            r#"{"code":0,"result":{"info":[{"id":1,"dids":["b"],"roomlist":[{"id":11,"dids":["b"]}]}],"has_more":false}}"#,
        ),
        MockResponse::json(
            200,
            r#"{"code":0,"result":{"list":[{"did":"a","uid":42,"name":"Lamp","model":"vendor.light.x","spec_type":"urn:light","pid":0,"token":"00112233445566778899aabbccddeeff","local_ip":"192.168.1.2"}],"has_more":true,"next_start_did":"a"}}"#,
        ),
        MockResponse::json(
            200,
            r#"{"code":0,"result":{"list":[{"did":"b","uid":42,"name":"Switch","model":"vendor.switch.x","spec_type":"urn:switch","pid":8,"isOnline":false}],"has_more":false}}"#,
        ),
    ]);
    let client = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    let catalog = future::block_on(client.get_owned_catalog("access")).unwrap();
    assert_eq!(catalog.uid, "42");
    assert_eq!(catalog.homes.len(), 1);
    assert_eq!(catalog.homes[0].group_id, "c7fe21bb97b206e2");
    assert_eq!(catalog.homes[0].rooms[1].name, "");
    assert_eq!(catalog.devices.len(), 2);
    assert_eq!(
        catalog.devices[0].token.as_ref().unwrap().0,
        vec![
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff
        ]
    );
    assert_eq!(catalog.devices[1].online, Some(false));
    let first: Value = serde_json::from_str(&requests.recv().unwrap().body).unwrap();
    assert_eq!(first["fetch_share"], false);
    assert_eq!(first["fetch_share_dev"], false);
}

#[test]
fn owned_catalog_rejects_repeated_cursors_missing_details_and_shared_devices() {
    let home = r#"{"code":0,"result":{"homelist":[{"id":"1","uid":"42","name":"Home","dids":["a"],"roomlist":[]}],"has_more":true,"max_id":"same"}}"#;
    let repeated = r#"{"code":0,"result":{"info":[],"has_more":true,"max_id":"same"}}"#;
    let (base, _) = mock_server(vec![
        MockResponse::json(200, home),
        MockResponse::json(200, repeated),
    ]);
    let client = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    assert_eq!(
        future::block_on(client.get_owned_catalog("access"))
            .unwrap_err()
            .kind(),
        &CloudErrorKind::Protocol
    );

    let complete_home = r#"{"code":0,"result":{"homelist":[{"id":"1","uid":"42","name":"Home","dids":["a"],"roomlist":[]}],"has_more":false}}"#;
    let missing = r#"{"code":0,"result":{"list":[],"has_more":false}}"#;
    let (base, _) = mock_server(vec![
        MockResponse::json(200, complete_home),
        MockResponse::json(200, missing),
    ]);
    let client = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    assert_eq!(
        future::block_on(client.get_owned_catalog("access"))
            .unwrap_err()
            .kind(),
        &CloudErrorKind::Protocol
    );

    let shared = r#"{"code":0,"result":{"list":[{"did":"a","uid":"99","name":"Shared","model":"vendor.light.x","spec_type":"urn:light","owner":{"userid":"99","nickname":"Other"}}],"has_more":false}}"#;
    let (base, _) = mock_server(vec![
        MockResponse::json(200, complete_home),
        MockResponse::json(200, shared),
    ]);
    let client = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    assert_eq!(
        future::block_on(client.get_owned_catalog("access"))
            .unwrap_err()
            .kind(),
        &CloudErrorKind::Protocol
    );
}

#[test]
fn owned_catalog_rejects_non_ascii_token_without_panicking() {
    let home = r#"{"code":0,"result":{"homelist":[{"id":"1","uid":"42","name":"Home","dids":["a"],"roomlist":[]}],"has_more":false}}"#;
    let token = format!("{}aa", "aé".repeat(10));
    assert_eq!(token.len(), 32);
    let details = json!({
        "code": 0,
        "result": {
            "list": [{
                "did": "a",
                "uid": "42",
                "name": "Device",
                "model": "vendor.light.x",
                "token": token
            }],
            "has_more": false
        }
    })
    .to_string();
    let (base, _) = mock_server(vec![
        MockResponse::json(200, home),
        MockResponse::json(200, &details),
    ]);
    let client = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    assert_eq!(
        future::block_on(client.get_owned_catalog("access"))
            .unwrap_err()
            .kind(),
        &CloudErrorKind::Protocol
    );
}

#[test]
fn owned_catalog_rejects_repeated_device_cursor() {
    let home = r#"{"code":0,"result":{"homelist":[{"id":"1","uid":"42","name":"Home","dids":["a"],"roomlist":[]}],"has_more":false}}"#;
    let page = r#"{"code":0,"result":{"list":[{"did":"a","uid":"42","name":"Device","model":"vendor.light.x"}],"has_more":true,"next_start_did":"same"}}"#;
    let (base, _) = mock_server(vec![
        MockResponse::json(200, home),
        MockResponse::json(200, page),
        MockResponse::json(200, page),
    ]);
    let client = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    assert_eq!(
        future::block_on(client.get_owned_catalog("access"))
            .unwrap_err()
            .kind(),
        &CloudErrorKind::Protocol
    );
}

#[test]
fn owned_catalog_accepts_split_detail_as_conservative_parent_metadata() {
    let home = r#"{"code":0,"result":{"homelist":[{"id":"1","uid":"42","name":"Home","dids":["a","a.s2"],"roomlist":[]}],"has_more":false}}"#;
    let details = r#"{"code":0,"result":{"list":[{"did":"a.s2","uid":"42","name":"Channel","model":"vendor.switch.x","spec_type":"urn:miot-spec-v2:device:switch:0000:test:1","parent_id":"a"}],"has_more":false}}"#;
    let (base, _) = mock_server(vec![
        MockResponse::json(200, home),
        MockResponse::json(200, details),
    ]);
    let client = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    let catalog = future::block_on(client.get_owned_catalog("access")).unwrap();
    assert_eq!(catalog.devices.len(), 1);
    assert_eq!(catalog.devices[0].did, "a.s2");
}

#[test]
fn owned_catalog_rejects_missing_split_details_without_same_parent_evidence() {
    let home = r#"{"code":0,"result":{"homelist":[{"id":"1","uid":"42","name":"Home","dids":["a.s2"],"roomlist":[]}],"has_more":false}}"#;
    let details = r#"{"code":0,"result":{"list":[],"has_more":false}}"#;
    let (base, _) = mock_server(vec![
        MockResponse::json(200, home),
        MockResponse::json(200, details),
    ]);
    let client = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    assert_eq!(
        future::block_on(client.get_owned_catalog("access"))
            .unwrap_err()
            .kind(),
        &CloudErrorKind::Protocol
    );
}

#[test]
fn spec_instance_request_preserves_urn_and_validates_document() {
    let urn = "urn:miot-spec-v2:device:light:0000:test:1";
    let body = format!(r#"{{"type":"{urn}","description":"Light","services":[]}}"#);
    let (base, requests) = mock_server(vec![MockResponse::json(200, &body)]);
    let client = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&future::block_on(client.get_spec_instance(urn)).unwrap())
            .unwrap(),
        serde_json::from_str::<Value>(&body).unwrap()
    );
    let request = requests.recv().unwrap();
    let url = Url::parse(&format!("http://test{}", request.target)).unwrap();
    assert_eq!(url.path(), "/miot-spec-v2/instance");
    assert_eq!(
        url.query_pairs().find(|(key, _)| key == "type").unwrap().1,
        urn
    );
}

#[test]
fn authenticated_uid_allows_an_empty_owned_catalog_and_rejects_typed_paging_flags() {
    let (base, _) = mock_server(vec![MockResponse::json(
        200,
        r#"{"code":0,"result":{"homelist":null,"has_more":false}}"#,
    )]);
    let client = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    let catalog = future::block_on(client.get_owned_catalog_for_uid("access", "42")).unwrap();
    assert_eq!(catalog.uid, "42");
    assert!(catalog.homes.is_empty());
    assert!(catalog.devices.is_empty());

    let (base, _) = mock_server(vec![MockResponse::json(
        200,
        r#"{"code":0,"result":{"homelist":[],"has_more":"false"}}"#,
    )]);
    let client = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    assert_eq!(
        future::block_on(client.get_owned_catalog_for_uid("access", "42"))
            .unwrap_err()
            .kind(),
        &CloudErrorKind::Protocol
    );
}

#[test]
fn owned_catalog_batches_more_than_150_dids() {
    let dids = (0..151)
        .map(|index| format!("d{index}"))
        .collect::<Vec<_>>();
    let home_dids = dids.clone();
    let (base, requests) = dynamic_mock_server(3, move |request| {
        if request.target.contains("gethome") {
            MockResponse::json(200, &json!({"code":0,"result":{"homelist":[{"id":"1","uid":"42","name":"Home","dids":home_dids,"roomlist":[]}],"has_more":false}}).to_string())
        } else {
            let body: Value = serde_json::from_str(&request.body).unwrap();
            let list = body["dids"].as_array().unwrap().iter().map(|did| json!({"did":did,"uid":"42","name":"Device","model":"vendor.light.x","spec_type":"urn:miot-spec-v2:device:light:0000:test:1"})).collect::<Vec<_>>();
            MockResponse::json(
                200,
                &json!({"code":0,"result":{"list":list,"has_more":false}}).to_string(),
            )
        }
    });
    let client = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    let catalog = future::block_on(client.get_owned_catalog("access")).unwrap();
    assert_eq!(catalog.devices.len(), 151);
    let requests = requests.try_iter().collect::<Vec<_>>();
    assert_eq!(
        serde_json::from_str::<Value>(&requests[1].body).unwrap()["dids"]
            .as_array()
            .unwrap()
            .len(),
        150
    );
    assert_eq!(
        serde_json::from_str::<Value>(&requests[2].body).unwrap()["dids"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}
