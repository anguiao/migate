use super::*;
use crate::xiaomi::test_support::{MockResponse, mock_server};
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
