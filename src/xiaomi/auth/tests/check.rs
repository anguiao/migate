use super::*;

#[test]
fn independent_check_keeps_its_cloud_device_probe_without_a_local_gateway() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store
        .xiaomi()
        .replace(&valid_record(
            "uid",
            "access",
            NOW + 500,
            NOW + 10 * 24 * 60 * 60,
        ))
        .unwrap();
    let (auth, requests) = service(
        &store,
        vec![
            MockResponse::json(200, &home_body(Some("uid"), &["target"])),
            MockResponse::json(503, "device read unavailable"),
        ],
    );
    assert!(!block_on(auth.check()).unwrap().is_success());
    let requests = requests.try_iter().collect::<Vec<_>>();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].target.ends_with("device_list_page"));
}

#[test]
fn check_without_credentials_is_not_signed_in_and_makes_no_request() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let (auth, requests) = service(&store, vec![]);
    let report = block_on(auth.check()).unwrap();
    assert!(matches!(
        report.authentication,
        AuthenticationState::NotSignedIn
    ));
    assert_eq!(report.certificate, None);
    assert!(!report.is_success());
    assert!(requests.try_recv().is_err());
}

#[test]
fn local_status_reports_checking_with_real_certificate_dates_without_http() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let (auth, requests) = service(&store, vec![]);
    let empty = auth.local_status().unwrap();
    assert!(matches!(
        empty.authentication,
        AuthenticationState::NotSignedIn
    ));
    assert_eq!(empty.certificate, None);

    let record = valid_record("uid", "access", NOW + 500, NOW + 1_000_000);
    store.xiaomi().replace(&record).unwrap();
    let status = auth.local_status().unwrap();
    assert!(matches!(
        status.authentication,
        AuthenticationState::Checking
    ));
    assert_eq!(
        status.certificate,
        Some(CertificateValidity {
            not_before: NOW - 60,
            not_after: NOW + 1_000_000,
        })
    );
    assert!(requests.try_recv().is_err());
}

#[test]
fn empty_devices_skip_request_but_missing_uid_fails_with_specific_reason() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let (base, requests) = dynamic_mock_server(3, |request| {
        if request.target.contains("get_token") {
            MockResponse::json(200, &token_body("a", "r"))
        } else if request.target.ends_with("gethome") {
            MockResponse::json(200, &home_body(Some("uid"), &[]))
        } else {
            certificate_response(request)
        }
    });
    let auth = AuthService::with_clock(
        store.xiaomi(),
        CloudClient::for_test(&base, Duration::from_secs(1)).unwrap(),
        now,
    );
    let attempt = auth.begin_login().unwrap();
    let input = callback(&attempt);
    assert!(
        block_on(auth.complete_login(attempt, &input))
            .unwrap()
            .is_success()
    );
    assert!(
        requests
            .try_iter()
            .all(|request| !request.target.ends_with("device_list_page"))
    );

    let (auth, _) = service(
        &store,
        vec![
            MockResponse::json(200, &token_body("a", "r")),
            MockResponse::json(200, &home_body(None, &[])),
        ],
    );
    let attempt = auth.begin_login().unwrap();
    let input = callback(&attempt);
    let report = block_on(auth.complete_login(attempt, &input)).unwrap();
    assert!(matches!(
        report.certificate_update,
        CertificateUpdate::Failed(FailureReason::CannotDetermineAccountIdentity)
    ));
}

#[test]
fn check_refreshes_at_deadline_and_retries_only_failed_device_after_401() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store
        .xiaomi()
        .replace(&valid_record("uid", "old", NOW, NOW + 1_000_000))
        .unwrap();
    let (auth, requests) = service(
        &store,
        vec![
            MockResponse::json(200, &token_body("fresh", "rotated")),
            MockResponse::json(200, &home_body(Some("uid"), &["did"])),
            MockResponse::json(401, "denied"),
        ],
    );
    let report = block_on(auth.check()).unwrap();
    assert!(matches!(
        report.authentication,
        AuthenticationState::SignInRequired(_)
    ));
    assert_eq!(
        store.xiaomi().load().unwrap().unwrap().tokens.access_token,
        "fresh"
    );
    let paths = requests.try_iter().map(|r| r.target).collect::<Vec<_>>();
    assert_eq!(paths.iter().filter(|p| p.ends_with("gethome")).count(), 1);

    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store
        .xiaomi()
        .replace(&valid_record("uid", "old", NOW + 500, NOW + 1_000_000))
        .unwrap();
    let (auth, requests) = service(
        &store,
        vec![
            MockResponse::json(200, &home_body(Some("uid"), &["did"])),
            MockResponse::json(401, "denied"),
            MockResponse::json(200, &token_body("fresh", "rotated")),
            MockResponse::json(200, devices_body()),
        ],
    );
    assert!(block_on(auth.check()).unwrap().is_success());
    let paths = requests.try_iter().map(|r| r.target).collect::<Vec<_>>();
    assert_eq!(paths.iter().filter(|p| p.ends_with("gethome")).count(), 1);
    assert_eq!(
        paths
            .iter()
            .filter(|p| p.ends_with("device_list_page"))
            .count(),
        2
    );
}

#[test]
fn home_401_refreshes_once_and_retries_home_with_new_token() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store
        .xiaomi()
        .replace(&valid_record("uid", "old", NOW + 500, NOW + 1_000_000))
        .unwrap();
    let (auth, requests) = service(
        &store,
        vec![
            MockResponse::json(401, "denied"),
            MockResponse::json(200, &token_body("fresh", "rotated")),
            MockResponse::json(200, &home_body(Some("uid"), &[])),
        ],
    );
    assert!(block_on(auth.check()).unwrap().is_success());
    let requests = requests.try_iter().collect::<Vec<_>>();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.target.ends_with("gethome"))
            .count(),
        2
    );
    assert!(
        requests[2]
            .headers
            .to_ascii_lowercase()
            .contains("authorization: bearerfresh")
    );
}

#[test]
fn home_or_device_refresh_exhausts_the_certificate_refresh_allowance() {
    for rejected_home in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let original = valid_record("uid", "old", NOW + 500, NOW + 60);
        store.xiaomi().replace(&original).unwrap();
        let mut responses = Vec::new();
        if !rejected_home {
            responses.push(MockResponse::json(200, &home_body(Some("uid"), &["did"])));
        }
        responses.extend([
            MockResponse::json(401, "access denied"),
            MockResponse::json(200, &token_body("fresh", "rotated")),
        ]);
        if rejected_home {
            responses.push(MockResponse::json(200, &home_body(Some("uid"), &["did"])));
        }
        responses.extend([
            MockResponse::json(200, devices_body()),
            MockResponse::json(401, "certificate denied"),
        ]);
        let expected_requests = responses.len();
        let (auth, requests) = service(&store, responses);
        let report = block_on(auth.check()).unwrap();
        assert!(matches!(
            report.authentication,
            AuthenticationState::SignInRequired(_)
        ));
        assert!(matches!(
            report.certificate_update,
            CertificateUpdate::Failed(_)
        ));
        let saved = store.xiaomi().load().unwrap().unwrap();
        assert_eq!(saved.tokens.refresh_token, "rotated");
        assert_eq!(saved.certificate_pem, original.certificate_pem);
        let requests = requests.try_iter().collect::<Vec<_>>();
        assert_eq!(requests.len(), expected_requests);
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.target.contains("get_token"))
                .count(),
            1
        );
        assert!(
            requests
                .last()
                .unwrap()
                .headers
                .to_ascii_lowercase()
                .contains("authorization: bearerfresh")
        );
    }
}

#[test]
fn refresh_is_saved_before_later_device_or_certificate_failure() {
    for renewal in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let expiry = if renewal { NOW + 60 } else { NOW + 1_000_000 };
        store
            .xiaomi()
            .replace(&valid_record("uid", "old", NOW, expiry))
            .unwrap();
        let mut responses = vec![
            MockResponse::json(200, &token_body("fresh", "rotated")),
            MockResponse::json(
                200,
                &home_body(Some("uid"), if renewal { &[] } else { &["did"] }),
            ),
        ];
        responses.push(if renewal {
            MockResponse::json(403, "forbidden")
        } else {
            MockResponse::json(500, "failed")
        });
        let (auth, _) = service(&store, responses);
        let report = block_on(auth.check()).unwrap();
        assert!(!report.is_success());
        assert_eq!(
            store.xiaomi().load().unwrap().unwrap().tokens.refresh_token,
            "rotated"
        );
    }
}

#[test]
fn refresh_403_and_business_failures_are_unavailable_not_sign_in_required() {
    for response in [
        MockResponse::json(403, "forbidden"),
        MockResponse::json(200, r#"{"code":-9}"#),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store
            .xiaomi()
            .replace(&valid_record("uid", "access", NOW, NOW + 1_000_000))
            .unwrap();
        let (auth, _) = service(&store, vec![response]);
        let report = block_on(auth.check()).unwrap();
        assert!(matches!(
            report.authentication,
            AuthenticationState::Unavailable(_)
        ));
        assert!(!matches!(
            report.authentication,
            AuthenticationState::SignInRequired(_)
        ));
    }
}
