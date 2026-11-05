use super::*;
use futures_lite::future::poll_once;

#[test]
fn bridge_refresh_saves_tokens_using_the_network_completion_clock_without_other_requests() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let original = valid_record("uid", "old-access", NOW, NOW + 1_000_000);
    store.xiaomi().fixture_replace(&original).unwrap();
    store.xiaomi().snapshot().unwrap();
    let clock = Arc::new(AtomicI64::new(NOW));
    let completed = clock.clone();
    let (base, requests) = dynamic_mock_server(4, move |_| {
        completed.store(NOW + 50, Ordering::SeqCst);
        MockResponse::json(200, &token_body("fresh-access", "fresh-refresh"))
    });
    let auth = AuthService::new(
        store.xiaomi(),
        CloudClient::for_test(&base, Duration::from_secs(1)).unwrap(),
    )
    .unwrap();
    let refreshed =
        block_on(auth.refresh_bridge_tokens(&original, || clock.load(Ordering::SeqCst))).unwrap();
    assert_eq!(
        refreshed
            .ha
            .as_ref()
            .unwrap()
            .authorization
            .as_ref()
            .unwrap()
            .tokens
            .access_token,
        "fresh-access"
    );
    assert_eq!(
        refreshed
            .ha
            .as_ref()
            .unwrap()
            .authorization
            .as_ref()
            .unwrap()
            .tokens
            .refresh_token,
        "fresh-refresh"
    );
    assert_eq!(
        refreshed
            .ha
            .as_ref()
            .unwrap()
            .authorization
            .as_ref()
            .unwrap()
            .tokens
            .expires_at,
        NOW + 1050
    );
    assert_eq!(
        refreshed
            .ha
            .as_ref()
            .unwrap()
            .authorization
            .as_ref()
            .unwrap()
            .tokens
            .refresh_at,
        NOW + 750
    );
    assert_eq!(
        refreshed.hub.as_ref().unwrap().certificate_pem,
        original.hub.as_ref().unwrap().certificate_pem
    );
    assert_eq!(
        refreshed.hub.as_ref().unwrap().private_key_pem,
        original.hub.as_ref().unwrap().private_key_pem
    );
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
        store.xiaomi().fixture_replace(&original).unwrap();
        let generation = store.xiaomi().snapshot().unwrap().ha_generation().unwrap();
        let (base, requests) = dynamic_mock_server(4, |_| MockResponse::json(401, "denied"));
        let auth = AuthService::new(
            store.xiaomi(),
            CloudClient::for_test(&base, Duration::from_secs(1)).unwrap(),
        )
        .unwrap();
        let error = if certificate {
            block_on(auth.renew_bridge_certificate(&original, generation, || NOW)).unwrap_err()
        } else {
            block_on(auth.refresh_bridge_tokens(&original, || NOW)).unwrap_err()
        };
        assert!(matches!(error, AuthError::Cloud(error) if error.is_unauthorized()));
        let mut expected = original.clone();
        expected.ha.as_mut().unwrap().stopped = !certificate;
        assert_eq!(store.xiaomi().load().unwrap(), Some(expected));
        if !certificate {
            let stopped = store.xiaomi().current_snapshot().record.unwrap();
            assert!(matches!(
                block_on(auth.refresh_bridge_tokens(&stopped, || NOW)),
                Err(AuthError::Missing(_))
            ));
        }
        let seen = requests.try_iter().collect::<Vec<_>>();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].target.contains("get_token"), !certificate);
        assert_eq!(seen[0].target.ends_with("get_central_crt"), certificate);
    }
}

#[test]
fn a_late_bridge_certificate_cannot_replace_the_current_generation() {
    for same_values in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let original = valid_record("uid", "old-access", NOW, NOW + 60);
        store.xiaomi().fixture_replace(&original).unwrap();
        let generation = store.xiaomi().snapshot().unwrap().ha_generation().unwrap();
        let (release, gate) = flume::bounded(1);
        let (base, requests) = dynamic_mock_server(4, move |request| {
            certificate_response(request).gated(gate.clone())
        });
        let auth = AuthService::new(
            store.xiaomi(),
            CloudClient::for_test(&base, Duration::from_secs(1)).unwrap(),
        )
        .unwrap();
        let fresh = if same_values {
            original.ha_authorization().unwrap().tokens.clone()
        } else {
            TokenSet {
                access_token: "fresh-access".into(),
                refresh_token: "fresh-refresh".into(),
                expires_at: NOW + 2000,
                refresh_at: NOW + 1400,
            }
        };
        block_on(async {
            let mut renewal =
                Box::pin(auth.renew_bridge_certificate(&original, generation, || NOW));
            assert!(poll_once(renewal.as_mut()).await.is_none());
            loop {
                if requests.try_recv().is_ok() {
                    break;
                }
                async_io::Timer::after(Duration::from_millis(1)).await;
            }
            store.xiaomi().fixture_update_tokens(&fresh).unwrap();
            release.send(()).unwrap();
            assert!(matches!(renewal.await, Err(AuthError::Missing(_))));
            let saved = store.xiaomi().load().unwrap().unwrap();
            assert_eq!(saved.ha_authorization().unwrap().tokens, fresh);
            assert_eq!(saved.hub, original.hub);
        });
    }
}

#[test]
fn invalid_bridge_certificates_leave_saved_credentials_untouched() {
    for not_before in [None, Some(NOW + 1), Some(NOW - 60)] {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let original = valid_record("uid", "old-access", NOW, NOW + 60);
        store.xiaomi().fixture_replace(&original).unwrap();
        let generation = store.xiaomi().snapshot().unwrap().ha_generation().unwrap();
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
        )
        .unwrap();
        let renewed =
            block_on(auth.renew_bridge_certificate(&original, generation, || NOW)).unwrap();
        assert_eq!(
            renewed.update,
            CertificateUpdate::Failed(if not_before.is_none() {
                FailureReason::InvalidCertificate
            } else {
                FailureReason::CertificateNotCurrentlyValid
            })
        );
        assert_eq!(renewed.credentials, original);
        assert_eq!(renewed.validity.unwrap().not_after, NOW + 60);
        assert_eq!(store.xiaomi().load().unwrap(), Some(original));
        assert_eq!(requests.try_iter().count(), 1);
    }
}

#[test]
fn bridge_certificate_validity_is_checked_when_the_network_operation_completes() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let original = valid_record("uid", "old-access", NOW, NOW + 60);
    store.xiaomi().fixture_replace(&original).unwrap();
    let generation = store.xiaomi().snapshot().unwrap().ha_generation().unwrap();
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
    )
    .unwrap();
    let renewed = block_on(
        auth.renew_bridge_certificate(&original, generation, || clock.load(Ordering::SeqCst)),
    )
    .unwrap();
    assert_eq!(
        renewed.update,
        CertificateUpdate::Failed(FailureReason::CertificateNotCurrentlyValid)
    );
    assert_eq!(renewed.credentials, original);
    assert_eq!(store.xiaomi().load().unwrap(), Some(original));
    assert_eq!(requests.try_iter().count(), 1);
}

#[test]
fn missing_hub_identity_survives_cancel_and_reopen_before_certificate_retry() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let mut original = valid_record("uid", "old-access", NOW, NOW + 60);
    original.hub = None;
    store.xiaomi().fixture_replace(&original).unwrap();
    let generation = store.xiaomi().snapshot().unwrap().ha_generation().unwrap();
    let (release, gate) = flume::bounded(1);
    let mut count = 0;
    let (base, requests) = dynamic_mock_server(2, move |request| {
        count += 1;
        let response = certificate_response(request);
        if count == 1 {
            response.gated(gate.clone())
        } else {
            response
        }
    });
    let auth = AuthService::new(
        store.xiaomi(),
        CloudClient::for_test(&base, Duration::from_secs(1)).unwrap(),
    )
    .unwrap();
    block_on(async {
        let mut pending = Box::pin(auth.renew_bridge_certificate(&original, generation, || NOW));
        assert!(poll_once(pending.as_mut()).await.is_none());
        loop {
            if requests.try_recv().is_ok() {
                break;
            }
            async_io::Timer::after(Duration::from_millis(1)).await;
        }
        drop(pending);
    });
    let identity = store.xiaomi().load().unwrap().unwrap().hub.unwrap();
    assert!(identity.certificate_pem.is_none());
    drop(auth);
    drop(store);
    let reopened = Store::open(dir.path()).unwrap();
    let snapshot = reopened.xiaomi().snapshot().unwrap();
    let generation = snapshot.ha_generation().unwrap();
    let record = snapshot.record.unwrap();
    let auth = AuthService::new(
        reopened.xiaomi(),
        CloudClient::for_test(&base, Duration::from_secs(1)).unwrap(),
    )
    .unwrap();
    let renewed = block_on(auth.renew_bridge_certificate(&record, generation, || NOW)).unwrap();
    let hub = renewed.credentials.hub.unwrap();
    assert_eq!(hub.virtual_did, identity.virtual_did);
    assert_eq!(hub.private_key_pem, identity.private_key_pem);
    assert!(hub.certificate_pem.is_some());
    release.send(()).unwrap();
}

#[test]
fn certificate_storage_failures_do_not_publish_unsaved_identity_or_certificate() {
    for fail_identity in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let mut original = valid_record("uid", "old-access", NOW, NOW + 60);
        if fail_identity {
            original.hub = None;
        }
        store.xiaomi().fixture_replace(&original).unwrap();
        let generation = store.xiaomi().snapshot().unwrap().ha_generation().unwrap();
        let db = Connection::open(store.path()).unwrap();
        let operation = if fail_identity { "INSERT" } else { "UPDATE" };
        db.execute_batch(&format!("CREATE TRIGGER fail_hub BEFORE {operation} ON xiaomi_hub BEGIN SELECT RAISE(ABORT, 'injected'); END;")).unwrap();
        let (base, requests) = dynamic_mock_server(1, certificate_response);
        let auth = AuthService::new(
            store.xiaomi(),
            CloudClient::for_test(&base, Duration::from_secs(1)).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            block_on(auth.renew_bridge_certificate(&original, generation, || NOW)),
            Err(AuthError::Storage(_))
        ));
        assert_eq!(store.xiaomi().load().unwrap(), Some(original));
        assert_eq!(requests.try_iter().count(), usize::from(!fail_identity));
    }
}

#[test]
fn late_certificate_error_is_not_reported_after_identical_token_refresh() {
    for status in [401, 503] {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let original = valid_record("uid", "old-access", NOW, NOW + 60);
        store.xiaomi().fixture_replace(&original).unwrap();
        let authorization = original.ha_authorization().unwrap();
        let generation = store.xiaomi().snapshot().unwrap().ha_generation().unwrap();
        let (release, gate) = flume::bounded(1);
        let (base, requests) = dynamic_mock_server(1, move |_| {
            MockResponse::json(status, "{}").gated(gate.clone())
        });
        let auth = AuthService::new(
            store.xiaomi(),
            CloudClient::for_test(&base, Duration::from_secs(1)).unwrap(),
        )
        .unwrap();
        block_on(async {
            let mut pending =
                Box::pin(auth.renew_bridge_certificate(&original, generation, || NOW));
            assert!(poll_once(pending.as_mut()).await.is_none());
            loop {
                if requests.try_recv().is_ok() {
                    break;
                }
                async_io::Timer::after(Duration::from_millis(1)).await;
            }
            store
                .xiaomi()
                .fixture_update_tokens(&authorization.tokens)
                .unwrap();
            release.send(()).unwrap();
            assert!(matches!(pending.await, Err(AuthError::Missing(_))));
            assert!(!store.xiaomi().load().unwrap().unwrap().ha.unwrap().stopped);
        });
    }
}
