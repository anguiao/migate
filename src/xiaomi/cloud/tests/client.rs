use super::*;

#[test]
fn miot_instance_endpoint_is_injected_only_for_tests() {
    let instance =
        json!({"type":"urn:miot-spec-v2:device:outlet:0000A002:vendor-v1:1","services":[]});
    let (base, requests) = mock_server(vec![MockResponse::json(200, &instance.to_string())]);
    let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    assert_eq!(
        future::block_on(cloud.get_instance(instance["type"].as_str().unwrap())).unwrap(),
        instance
    );
    let request = requests.recv().unwrap();
    let url = Url::parse(&format!("http://test{}", request.target)).unwrap();
    assert_eq!(url.path(), "/miot-spec-v2/instance");
    assert_eq!(
        url.query_pairs().find(|(key, _)| key == "type").unwrap().1,
        instance["type"].as_str().unwrap()
    );
    assert!(!request.headers.to_lowercase().contains("authorization"));
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
