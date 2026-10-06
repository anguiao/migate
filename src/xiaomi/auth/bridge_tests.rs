use super::*;
use futures_lite::future::poll_once;

#[test]
fn bridge_refresh_saves_tokens_using_the_network_completion_clock_without_other_requests() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let original = valid_record("uid", "old-access", NOW, NOW + 1_000_000);
    store.xiaomi().replace(&original).unwrap();
    let clock = Arc::new(AtomicI64::new(NOW));
    let completed = clock.clone();
    let (base, requests) = dynamic_mock_server(4, move |_| {
        completed.store(NOW + 50, Ordering::SeqCst);
        MockResponse::json(200, &token_body("fresh-access", "fresh-refresh"))
    });
    let auth = AuthService::new(
        store.xiaomi(),
        CloudClient::for_test(&base, Duration::from_secs(1)).unwrap(),
    );
    let refreshed =
        block_on(auth.refresh_bridge_tokens(&original, || clock.load(Ordering::SeqCst))).unwrap();
    assert_eq!(refreshed.tokens.access_token, "fresh-access");
    assert_eq!(refreshed.tokens.refresh_token, "fresh-refresh");
    assert_eq!(refreshed.tokens.expires_at, NOW + 1050);
    assert_eq!(refreshed.tokens.refresh_at, NOW + 750);
    assert_eq!(refreshed.certificate_pem, original.certificate_pem);
    assert_eq!(refreshed.private_key_pem, original.private_key_pem);
    assert_eq!(store.xiaomi().load().unwrap(), Some(refreshed));
    let seen = requests.try_iter().collect::<Vec<_>>();
    assert_eq!(seen.len(), 1);
    assert!(seen[0].target.contains("get_token"));
}

#[test]
fn bridge_helpers_return_a_single_unauthorized_response_without_refresh_or_retry() {
    for certificate in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let original = valid_record("uid", "old-access", NOW, NOW + 60);
        store.xiaomi().replace(&original).unwrap();
        let (base, requests) = dynamic_mock_server(4, |_| MockResponse::json(401, "denied"));
        let auth = AuthService::new(
            store.xiaomi(),
            CloudClient::for_test(&base, Duration::from_secs(1)).unwrap(),
        );
        let error = if certificate {
            block_on(auth.renew_bridge_certificate(&original, || NOW)).unwrap_err()
        } else {
            block_on(auth.refresh_bridge_tokens(&original, || NOW)).unwrap_err()
        };
        assert!(matches!(error, AuthError::Cloud(error) if error.is_unauthorized()));
        assert_eq!(store.xiaomi().load().unwrap(), Some(original));
        let seen = requests.try_iter().collect::<Vec<_>>();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].target.contains("get_token"), !certificate);
        assert_eq!(seen[0].target.ends_with("get_central_crt"), certificate);
    }
}

#[test]
fn a_late_bridge_certificate_preserves_tokens_saved_while_its_response_was_held() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let original = valid_record("uid", "old-access", NOW, NOW + 60);
    store.xiaomi().replace(&original).unwrap();
    let (release, gate) = flume::bounded(1);
    let (base, requests) = dynamic_mock_server(4, move |request| {
        certificate_response(request).gated(gate.clone())
    });
    let auth = AuthService::new(
        store.xiaomi(),
        CloudClient::for_test(&base, Duration::from_secs(1)).unwrap(),
    );
    let fresh = TokenSet {
        access_token: "fresh-access".into(),
        refresh_token: "fresh-refresh".into(),
        expires_at: NOW + 2000,
        refresh_at: NOW + 1400,
    };
    block_on(async {
        let mut renewal = Box::pin(auth.renew_bridge_certificate(&original, || NOW));
        assert!(poll_once(renewal.as_mut()).await.is_none());
        let pending = loop {
            if let Ok(request) = requests.try_recv() {
                break request;
            }
            async_io::Timer::after(Duration::from_millis(1)).await;
        };
        assert!(pending.target.ends_with("get_central_crt"));
        store.xiaomi().update_tokens(&fresh).unwrap();
        release.send(()).unwrap();
        let renewed = renewal.await.unwrap();
        assert_eq!(renewed.update, CertificateUpdate::Updated);
        assert_eq!(renewed.credentials.tokens, fresh);
        assert_eq!(
            renewed.credentials.private_key_pem,
            original.private_key_pem
        );
        assert_eq!(renewed.credentials.virtual_did, original.virtual_did);
        assert_ne!(
            renewed.credentials.certificate_pem,
            original.certificate_pem
        );
        assert_eq!(renewed.validity.not_after, NOW + 10 * 24 * 60 * 60);
        assert_eq!(store.xiaomi().load().unwrap(), Some(renewed.credentials));
        assert_eq!(requests.try_iter().count(), 0);
    });
}

#[test]
fn invalid_bridge_certificates_leave_saved_credentials_untouched() {
    for not_before in [None, Some(NOW + 1), Some(NOW - 60)] {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let original = valid_record("uid", "old-access", NOW, NOW + 60);
        store.xiaomi().replace(&original).unwrap();
        let (base, requests) = dynamic_mock_server(4, move |request| {
            let certificate = match not_before {
                None => "not a certificate".into(),
                Some(not_before) => {
                    let body: Value = serde_json::from_str(&request.body).unwrap();
                    let csr =
                        String::from_utf8(STANDARD.decode(body["csr"].as_str().unwrap()).unwrap())
                            .unwrap();
                    let not_after = if not_before > NOW { NOW + 1000 } else { NOW };
                    sign_csr(&csr, not_before, not_after).unwrap()
                }
            };
            MockResponse::json(
                200,
                &json!({"code":0,"result":{"cert":certificate}}).to_string(),
            )
        });
        let auth = AuthService::new(
            store.xiaomi(),
            CloudClient::for_test(&base, Duration::from_secs(1)).unwrap(),
        );
        let renewed = block_on(auth.renew_bridge_certificate(&original, || NOW)).unwrap();
        assert_eq!(
            renewed.update,
            CertificateUpdate::Failed(if not_before.is_none() {
                FailureReason::InvalidCertificate
            } else {
                FailureReason::CertificateNotCurrentlyValid
            })
        );
        assert_eq!(renewed.credentials, original);
        assert_eq!(renewed.validity.not_after, NOW + 60);
        assert_eq!(store.xiaomi().load().unwrap(), Some(original));
        assert_eq!(requests.try_iter().count(), 1);
    }
}

#[test]
fn bridge_certificate_validity_is_checked_when_the_network_operation_completes() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let original = valid_record("uid", "old-access", NOW, NOW + 60);
    store.xiaomi().replace(&original).unwrap();
    let clock = Arc::new(AtomicI64::new(NOW));
    let completed = clock.clone();
    let (base, requests) = dynamic_mock_server(4, move |request| {
        let body: Value = serde_json::from_str(&request.body).unwrap();
        let csr =
            String::from_utf8(STANDARD.decode(body["csr"].as_str().unwrap()).unwrap()).unwrap();
        let certificate = sign_csr(&csr, NOW - 60, NOW + 10).unwrap();
        completed.store(NOW + 20, Ordering::SeqCst);
        MockResponse::json(
            200,
            &json!({"code":0,"result":{"cert":certificate}}).to_string(),
        )
    });
    let auth = AuthService::new(
        store.xiaomi(),
        CloudClient::for_test(&base, Duration::from_secs(1)).unwrap(),
    );
    let renewed =
        block_on(auth.renew_bridge_certificate(&original, || clock.load(Ordering::SeqCst)))
            .unwrap();
    assert_eq!(
        renewed.update,
        CertificateUpdate::Failed(FailureReason::CertificateNotCurrentlyValid)
    );
    assert_eq!(renewed.credentials, original);
    assert_eq!(store.xiaomi().load().unwrap(), Some(original));
    assert_eq!(requests.try_iter().count(), 1);
}
