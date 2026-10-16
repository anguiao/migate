use super::*;
use futures_lite::future::poll_once;

fn save_cache(store: &Store, cached: crate::storage::CachedPhysicalDevice) {
    let scope = crate::storage::CachedScope {
        uid: cached.uid.clone(),
        home_id: cached.home_id.clone(),
        group_id: cached.group_id.clone(),
    };
    store
        .devices()
        .apply_publication(&crate::storage::tests::fixtures::commit(
            scope,
            vec![cached],
            Vec::new(),
        ))
        .unwrap();
}

#[test]
fn bridge_preparation_reads_homes_once_and_skips_the_independent_device_probe() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store
        .xiaomi()
        .replace(&valid_record(
            "uid",
            "old-access",
            NOW + 500,
            NOW + 10 * 24 * 60 * 60,
        ))
        .unwrap();
    let homes = json!({"code":0,"result":{"homelist":[{"uid":"uid","id":"local-home","roomlist":[],"dids":["target"]}]}});
    let (auth, requests) = service(&store, vec![MockResponse::json(200, &homes.to_string())]);
    let prepared = block_on(auth.prepare_bridge()).unwrap();
    assert!(prepared.report.is_success());
    assert_eq!(
        prepared.credentials.unwrap().tokens.access_token,
        "old-access"
    );
    assert_eq!(prepared.homes[0].home_id, "local-home");
    assert_eq!(requests.try_iter().count(), 1);
}

#[test]
fn bridge_cloud_or_refresh_failure_preserves_same_account_cache_and_valid_certificate() {
    for (refresh_at, status) in [(NOW + 500, 503), (NOW - 1, 401)] {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let original = valid_record("uid", "old-access", refresh_at, NOW + 10 * 24 * 60 * 60);
        store.xiaomi().replace(&original).unwrap();
        let cached = cached_bridge_device("uid");
        save_cache(&store, cached.clone());
        save_cache(&store, cached_bridge_device("other-account"));
        let (auth, requests) = service(&store, vec![MockResponse::json(status, "cloud failure")]);
        let prepared = block_on(auth.prepare_bridge()).unwrap();
        assert!(!prepared.report.is_success());
        assert!(prepared.report.certificate.unwrap().currently_valid(NOW));
        assert_eq!(prepared.credentials, Some(original));
        assert!(prepared.homes.is_empty());
        assert_eq!(
            store
                .devices()
                .load_parsed(
                    &prepared.credentials.as_ref().unwrap().uid,
                    "local-home",
                    &cached.device.key
                )
                .unwrap(),
            Some(cached)
        );
        assert_eq!(requests.try_iter().count(), 1);
        if status == 401 {
            assert!(matches!(
                prepared.report.authentication,
                AuthenticationState::SignInRequired(_)
            ));
        }
    }
}

#[test]
fn bridge_account_mismatch_does_not_expose_another_accounts_directory_or_cache() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store
        .xiaomi()
        .replace(&valid_record(
            "old-account",
            "access",
            NOW + 500,
            NOW + 10 * 24 * 60 * 60,
        ))
        .unwrap();
    let other = cached_bridge_device("new-account");
    save_cache(&store, other.clone());
    let homes = json!({"code":0,"result":{"homelist":[{"uid":"new-account","id":"local-home","roomlist":[],"dids":["target"]}]}});
    let (auth, _) = service(&store, vec![MockResponse::json(200, &homes.to_string())]);
    let prepared = block_on(auth.prepare_bridge()).unwrap();
    assert!(matches!(
        prepared.report.authentication,
        AuthenticationState::Unavailable(FailureReason::AccountIdentityMismatch)
    ));
    assert!(prepared.homes.is_empty());
    assert!(
        store
            .devices()
            .load_parsed("old-account", "local-home", &other.device.key)
            .unwrap()
            .is_none()
    );
    assert_eq!(prepared.credentials.unwrap().uid, "old-account");
}

#[test]
fn bridge_preparation_renews_with_processed_credentials_without_reading_device_cache() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store
        .xiaomi()
        .replace(&valid_record("uid", "old-access", NOW - 1, NOW + 60))
        .unwrap();
    let homes = json!({"code":0,"result":{"homelist":[{"uid":"uid","id":"local-home","roomlist":[],"dids":["target"]}]}});
    let (base, requests) = dynamic_mock_server(3, move |request| {
        if request.target.contains("get_token") {
            MockResponse::json(200, &token_body("new-access", "new-refresh"))
        } else if request.target.ends_with("gethome") {
            MockResponse::json(200, &homes.to_string())
        } else {
            certificate_response(request)
        }
    });
    let auth = AuthService::with_clock(
        store.xiaomi(),
        CloudClient::for_test(&base, Duration::from_secs(1)).unwrap(),
        now,
    );
    let prepared = block_on(auth.prepare_bridge()).unwrap();
    assert!(prepared.report.is_success());
    assert_eq!(
        prepared.report.certificate_update,
        CertificateUpdate::Updated
    );
    assert_eq!(
        prepared.credentials.unwrap().tokens.access_token,
        "new-access"
    );
    assert_eq!(requests.try_iter().count(), 3);

    let cached = cached_bridge_device("uid");
    save_cache(&store, cached.clone());
    Connection::open(store.path())
        .unwrap()
        .execute("UPDATE device_cache SET capabilities = 'broken'", [])
        .unwrap();
    let (auth, _) = service(&store, vec![MockResponse::json(503, "cloud unavailable")]);
    let prepared = block_on(auth.prepare_bridge()).unwrap();
    assert!(prepared.credentials.is_some());
    assert!(
        store
            .devices()
            .load_parsed("uid", "local-home", &cached.device.key)
            .is_err()
    );
}

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
