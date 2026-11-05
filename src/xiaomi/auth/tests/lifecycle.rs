use super::*;

#[test]
fn local_status_distinguishes_missing_invalid_and_repaired_certificates() {
    let mismatch = valid_record("other-uid", "access", NOW + 500, NOW + 1_000_000)
        .hub
        .unwrap()
        .certificate_pem;
    for (certificate, expected) in [
        (None, FailureReason::MissingCertificate),
        (
            Some("malformed certificate".to_owned()),
            FailureReason::InvalidCertificate,
        ),
        (mismatch, FailureReason::InvalidCertificate),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let mut record = valid_record("uid", "access", NOW + 500, NOW + 1_000_000);
        let original = record.hub.as_ref().unwrap().certificate_pem.clone();
        record.hub.as_mut().unwrap().certificate_pem = certificate;
        store.xiaomi().replace(&record).unwrap();
        let (auth, requests) = service(&store, vec![]);
        let report = auth.local_status().unwrap();
        assert!(matches!(
            report.authentication,
            AuthenticationState::Checking
        ));
        assert_eq!(report.certificate, None);
        assert_eq!(
            report.certificate_update,
            CertificateUpdate::Failed(expected)
        );
        record.hub.as_mut().unwrap().certificate_pem = original;
        store.xiaomi().replace(&record).unwrap();
        let repaired = auth.local_status().unwrap();
        assert!(repaired.certificate.is_some());
        assert_eq!(repaired.certificate_update, CertificateUpdate::NotNeeded);
        assert!(requests.try_recv().is_err());
    }
}

#[test]
fn invalid_certificate_does_not_hide_account_identity() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let mut record = valid_record("uid", "access", NOW + 500, NOW + 1_000_000);
    record.hub.as_mut().unwrap().certificate_pem = Some("invalid certificate".into());
    store.xiaomi().replace(&record).unwrap();
    let (auth, _) = service(&store, vec![]);

    let report = auth.local_status().unwrap();
    assert!(!matches!(
        report.authentication,
        AuthenticationState::NotSignedIn
    ));
    assert_eq!(report.certificate, None);
}

#[test]
fn corrupt_credentials_block_login_and_check_but_logout_does_not_load() {
    for corruption in [
        "UPDATE xiaomi_ha_oauth SET redirect_uri = 'http://evil.invalid/api/webhook/x'",
        "UPDATE xiaomi_ha_oauth SET redirect_uri = 'http://user@homeassistant.local:8123/api/webhook/0123456789abcdef0123456789abcdef'",
        "UPDATE xiaomi_ha_oauth SET redirect_uri = 'http://homeassistant.local:8123/api/webhook/0123456789abcdef0123456789abcdef?query=x'",
        "UPDATE xiaomi_ha_oauth SET redirect_uri = 'http://homeassistant.local:8123/api/webhook/0123456789abcdef0123456789abcdef#fragment'",
        "UPDATE xiaomi_hub SET private_key_pem = 'not a key'",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store
            .xiaomi()
            .replace(&valid_record("uid", "access", NOW + 500, NOW + 1_000_000))
            .unwrap();
        Connection::open(store.path())
            .unwrap()
            .execute(corruption, [])
            .unwrap();
        let (auth, _) = service(&store, vec![]);
        assert!(matches!(auth.begin_login(), Err(AuthError::Storage(_))));
        assert!(matches!(block_on(auth.check()), Err(AuthError::Storage(_))));
        store.xiaomi().logout().unwrap();
        assert!(store.xiaomi().load().unwrap().is_none());
    }
}

#[test]
fn certificate_binding_failures_do_not_remove_the_account() {
    for corruption in [
        "UPDATE xiaomi_hub SET certificate_pem = 'not a certificate'",
        "UPDATE xiaomi_hub SET virtual_did = '1'",
        "UPDATE xiaomi_account SET uid = 'other-account'",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store
            .xiaomi()
            .replace(&valid_record("uid", "access", NOW + 500, NOW + 1_000_000))
            .unwrap();
        Connection::open(store.path())
            .unwrap()
            .execute(corruption, [])
            .unwrap();
        let (auth, _) = service(&store, vec![]);
        let report = auth.local_status().unwrap();
        assert_eq!(report.certificate, None);
        assert!(!matches!(
            report.authentication,
            AuthenticationState::NotSignedIn
        ));
        assert!(auth.begin_login().is_ok());
        store.xiaomi().logout().unwrap();
        assert!(store.xiaomi().load().unwrap().is_none());
    }
}

#[test]
fn dropping_pending_login_or_check_stops_later_storage_writes_but_keeps_prior_refresh() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let old = valid_record("uid", "old", NOW + 500, NOW + 1_000_000);
    store.xiaomi().replace(&old).unwrap();
    let login_pending = Arc::new(AtomicBool::new(false));
    let login_signal = login_pending.clone();
    let (base, _) = dynamic_mock_server(1, move |_| {
        login_signal.store(true, Ordering::SeqCst);
        MockResponse::json(200, &token_body("new", "rotated")).delayed(Duration::from_millis(100))
    });
    let auth = AuthService::with_clock(
        store.xiaomi(),
        CloudClient::for_test(&base, Duration::from_secs(1)).unwrap(),
        now,
    );
    let attempt = auth.begin_login().unwrap();
    let input = callback(&attempt);
    block_on(future::or(
        async {
            let _ = auth.complete_login(attempt, &input).await;
        },
        wait_for_signal(login_pending),
    ));
    thread::sleep(Duration::from_millis(150));
    assert_eq!(store.xiaomi().load().unwrap(), Some(old));

    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let old = valid_record("uid", "old", NOW, NOW + 1_000_000);
    let old_certificate = old.hub.as_ref().unwrap().certificate_pem.clone();
    store.xiaomi().replace(&old).unwrap();
    let check_pending = Arc::new(AtomicBool::new(false));
    let check_signal = check_pending.clone();
    let (base, _) = dynamic_mock_server(2, move |request| {
        if request.target.contains("get_token") {
            MockResponse::json(200, &token_body("fresh", "rotated"))
        } else {
            check_signal.store(true, Ordering::SeqCst);
            MockResponse::json(200, &home_body(Some("uid"), &["did"]))
                .delayed(Duration::from_millis(100))
        }
    });
    let auth = AuthService::with_clock(
        store.xiaomi(),
        CloudClient::for_test(&base, Duration::from_secs(1)).unwrap(),
        now,
    );
    block_on(future::or(
        async {
            let _ = auth.check().await;
        },
        wait_for_signal(check_pending),
    ));
    thread::sleep(Duration::from_millis(120));
    let saved = store.xiaomi().load().unwrap().unwrap();
    assert_eq!(
        saved
            .ha
            .as_ref()
            .unwrap()
            .authorization
            .as_ref()
            .unwrap()
            .tokens
            .access_token,
        "fresh"
    );
    assert_eq!(saved.hub.as_ref().unwrap().certificate_pem, old_certificate);
}

#[test]
fn completion_clock_is_read_after_network_operations() {
    static CLOCK: AtomicI64 = AtomicI64::new(NOW);
    fn controlled_now() -> i64 {
        CLOCK.load(Ordering::SeqCst)
    }
    CLOCK.store(NOW, Ordering::SeqCst);
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let (base, _) = dynamic_mock_server(3, |request| {
        if request.target.contains("get_token") {
            CLOCK.store(NOW + 50, Ordering::SeqCst);
            MockResponse::json(200, &token_body("a", "r"))
        } else if request.target.ends_with("gethome") {
            MockResponse::json(200, &home_body(Some("uid"), &[]))
        } else {
            CLOCK.store(NOW + 100, Ordering::SeqCst);
            certificate_response(request)
        }
    });
    let auth = AuthService::with_clock(
        store.xiaomi(),
        CloudClient::for_test(&base, Duration::from_secs(1)).unwrap(),
        controlled_now,
    );
    let attempt = auth.begin_login().unwrap();
    let input = callback(&attempt);
    let report = block_on(auth.complete_login(attempt, &input)).unwrap();
    assert_eq!(report.completed_at(), NOW + 100);
    let tokens = store
        .xiaomi()
        .load()
        .unwrap()
        .unwrap()
        .ha
        .as_ref()
        .unwrap()
        .authorization
        .as_ref()
        .unwrap()
        .tokens
        .clone();
    assert_eq!(tokens.expires_at, NOW + 1050);
    assert_eq!(tokens.refresh_at, NOW + 750);
}
