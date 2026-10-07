use super::*;

#[test]
fn corrupt_credentials_block_login_and_check_but_logout_does_not_load() {
    for corruption in [
        "UPDATE xiaomi_auth SET redirect_uri = 'http://evil.invalid/api/webhook/x'",
        "UPDATE xiaomi_auth SET redirect_uri = 'http://user@homeassistant.local:8123/api/webhook/0123456789abcdef0123456789abcdef'",
        "UPDATE xiaomi_auth SET redirect_uri = 'http://homeassistant.local:8123/api/webhook/0123456789abcdef0123456789abcdef?query=x'",
        "UPDATE xiaomi_auth SET redirect_uri = 'http://homeassistant.local:8123/api/webhook/0123456789abcdef0123456789abcdef#fragment'",
        "UPDATE xiaomi_auth SET private_key_pem = 'not a key'",
        "UPDATE xiaomi_auth SET certificate_pem = 'not a certificate'",
        "UPDATE xiaomi_auth SET virtual_did = '1'",
        "UPDATE xiaomi_auth SET uid = 'other-account'",
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
    let old_certificate = old.certificate_pem.clone();
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
    assert_eq!(saved.tokens.access_token, "fresh");
    assert_eq!(saved.certificate_pem, old_certificate);
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
    let tokens = store.xiaomi().load().unwrap().unwrap().tokens;
    assert_eq!(tokens.expires_at, NOW + 1050);
    assert_eq!(tokens.refresh_at, NOW + 750);
}
