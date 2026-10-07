use super::*;

#[test]
fn authorization_state_matches_upstream_device_binding() {
    for (uuid, expected_device_id, expected_state) in [
        (
            "550e8400-e29b-41d4-a716-446655440000",
            "ha.550e8400e29b41d4a716446655440000",
            "004a628e02325dad7fb5f8db58b6da38cfaa8418",
        ),
        (
            "550e8400-e29b-41d4-a716-446655440001",
            "ha.550e8400e29b41d4a716446655440001",
            "2353e712f3c5dfdf92234484bea15ef42312c769",
        ),
    ] {
        let attempt = AuthorizationAttempt::new(Some(uuid)).unwrap();
        let url = Url::parse(attempt.authorization_url()).unwrap();
        let query = url
            .query_pairs()
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(attempt.oauth_client_uuid(), uuid);
        assert_eq!(attempt.device_id(), expected_device_id);
        assert_eq!(query["device_id"], expected_device_id);
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
    assert_eq!(query["device_id"], "ha.550e8400e29b41d4a716446655440000");
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
    let refreshed =
        future::block_on(client.refresh_token(attempt.redirect_uri(), "refresh-input")).unwrap();
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
            assert_eq!(data["device_id"], "ha.550e8400e29b41d4a716446655440000");
        } else {
            assert!(data.get("device_id").is_none());
        }
        assert!(data.get(expected_key).is_some());
        assert!(data.get("grant_type").is_none());
    }
    assert_eq!(refreshed.expires_in, 1000);
}

#[test]
fn new_authorization_uses_compact_device_id_through_token_exchange() {
    use sha1::{Digest as _, Sha1};

    let attempt = AuthorizationAttempt::new(None).unwrap();
    let uuid = uuid::Uuid::parse_str(attempt.oauth_client_uuid()).unwrap();
    assert_eq!(attempt.oauth_client_uuid(), uuid.hyphenated().to_string());
    let expected_device_id = format!("ha.{}", uuid.simple());
    let expected_state = Sha1::digest(format!("d={expected_device_id}"))
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let authorization_url = Url::parse(attempt.authorization_url()).unwrap();
    let query = authorization_url
        .query_pairs()
        .collect::<std::collections::HashMap<_, _>>();
    assert_eq!(attempt.device_id(), expected_device_id);
    assert_eq!(query["device_id"], expected_device_id);
    assert_eq!(query["state"], expected_state);

    let (base, requests) = mock_server(vec![MockResponse::json(
        200,
        r#"{"code":0,"result":{"access_token":"access","refresh_token":"refresh","expires_in":1000}}"#,
    )]);
    let client = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    future::block_on(client.exchange_token(&attempt, "code")).unwrap();
    let request = requests.recv().unwrap();
    let url = Url::parse(&format!("http://local{}", request.target)).unwrap();
    let data = url.query_pairs().find(|(key, _)| key == "data").unwrap().1;
    let data: Value = serde_json::from_str(&data).unwrap();
    assert_eq!(data["device_id"], expected_device_id);
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
            future::block_on(client.refresh_token(attempt.redirect_uri(), "refresh-secret"))
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
