use super::*;

#[test]
fn missing_mijia_session_does_not_make_ha_authorization_sufficient() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let mut record = valid_record("uid", "access", NOW + 500, NOW + 1_000_000);
    record.mijia = None;
    store.xiaomi().fixture_replace(&record).unwrap();
    let (auth, requests) = service(
        &store,
        vec![MockResponse::json(200, &home_body(Some("uid"), &[]))],
    );

    let report = block_on(auth.check()).unwrap();

    assert!(
        !report.is_success(),
        "missing Mijia session must fail check"
    );
    assert_eq!(requests.try_iter().count(), 1, "HA is still checked");
}

#[test]
fn successful_certificate_renewal_does_not_hide_failed_due_refresh() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let original = valid_record("uid", "old", NOW, NOW + 60);
    store.xiaomi().fixture_replace(&original).unwrap();
    let (base, requests) = dynamic_mock_server(2, |request| {
        if request.target.contains("get_token") {
            MockResponse::json(503, "temporary failure")
        } else {
            certificate_response(request)
        }
    });
    let auth = AuthService::with_clock(
        store.xiaomi(),
        CloudClient::for_test(&base, Duration::from_secs(1)).unwrap(),
        now,
    );
    let report = block_on(auth.check()).unwrap();
    assert!(!report.is_success());
    assert!(matches!(report.mijia, AuthenticationState::Authenticated));
    assert!(matches!(
        report.ha,
        AuthenticationState::Unavailable(FailureReason::Cloud(_))
    ));
    assert_eq!(report.certificate_update, CertificateUpdate::Updated);
    let saved = store.xiaomi().load().unwrap().unwrap();
    assert!(!saved.ha.unwrap().stopped);
    assert_ne!(
        saved.hub.unwrap().certificate_pem,
        original.hub.unwrap().certificate_pem
    );
    let requests: Vec<_> = requests.try_iter().collect();
    assert_eq!(requests.len(), 2);
    assert!(
        requests
            .iter()
            .all(|request| !request.target.ends_with("gethome"))
    );
}

#[test]
fn failed_refresh_followed_by_old_certificate_token_rejection_does_not_stop_ha() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let original = valid_record("uid", "old", NOW, NOW + 60);
    store.xiaomi().fixture_replace(&original).unwrap();
    let (auth, requests) = service(
        &store,
        vec![
            MockResponse::json(503, "refresh unavailable"),
            MockResponse::json(401, "old token rejected"),
        ],
    );
    let report = block_on(auth.check()).unwrap();
    assert!(!report.is_success());
    assert!(matches!(report.mijia, AuthenticationState::Authenticated));
    assert!(matches!(
        report.ha,
        AuthenticationState::Unavailable(FailureReason::Cloud(_))
    ));
    assert!(matches!(
        report.certificate_update,
        CertificateUpdate::Failed(_)
    ));
    assert!(!store.xiaomi().load().unwrap().unwrap().ha.unwrap().stopped);
    assert_eq!(requests.try_iter().count(), 2);
}

#[test]
fn check_reports_certificate_request_errors_without_hiding_other_ha_results() {
    for (certificate, expected) in [
        (None, FailureReason::MissingCertificate),
        (
            Some("malformed certificate".to_owned()),
            FailureReason::InvalidCertificate,
        ),
    ] {
        for ha_state in ["ok", "missing", "stopped", "network", "mismatch"] {
            let dir = tempfile::tempdir().unwrap();
            let store = Store::open(dir.path()).unwrap();
            let mut record = valid_record("uid", "access", NOW + 500, NOW + 1_000_000);
            record.hub.as_mut().unwrap().certificate_pem = certificate.clone();
            if ha_state == "missing" {
                record.ha.as_mut().unwrap().authorization = None;
            }
            if ha_state == "stopped" {
                record.ha.as_mut().unwrap().stopped = true;
            }
            store.xiaomi().fixture_replace(&record).unwrap();
            let home = match ha_state {
                "network" => MockResponse::json(503, "unavailable"),
                "mismatch" => MockResponse::json(200, &home_body(Some("other"), &[])),
                _ => MockResponse::json(200, &home_body(Some("uid"), &[])),
            };
            let (auth, requests) = service(
                &store,
                vec![home, MockResponse::json(503, "certificate unavailable")],
            );
            let report = block_on(auth.check()).unwrap();
            assert_eq!(report.certificate, None);
            if matches!(ha_state, "ok" | "network") {
                assert!(matches!(
                    report.certificate_update,
                    CertificateUpdate::Failed(FailureReason::Cloud(_))
                ));
            } else {
                assert_eq!(
                    report.certificate_update,
                    CertificateUpdate::Failed(expected.clone())
                );
            }
            match ha_state {
                "ok" => assert!(matches!(report.ha, AuthenticationState::Authenticated)),
                "missing" => assert!(matches!(
                    report.ha,
                    AuthenticationState::SignInRequired(FailureReason::MissingHaAuthorization)
                )),
                "stopped" => assert!(matches!(
                    report.ha,
                    AuthenticationState::SignInRequired(FailureReason::MissingHaAuthorization)
                )),
                "network" => assert!(matches!(
                    report.ha,
                    AuthenticationState::Unavailable(FailureReason::Cloud(_))
                )),
                "mismatch" => assert!(matches!(
                    report.ha,
                    AuthenticationState::Unavailable(FailureReason::AccountIdentityMismatch)
                )),
                _ => unreachable!(),
            }
            let count = match ha_state {
                "missing" | "stopped" => 0,
                "mismatch" => 1,
                _ => 2,
            };
            assert_eq!(requests.try_iter().count(), count);
        }
    }
}

#[test]
fn independent_check_uses_ha_home_only_and_not_ha_device_directory() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store
        .xiaomi()
        .fixture_replace(&valid_record("uid", "access", NOW + 500, NOW + 1_000_000))
        .unwrap();
    let (auth, requests) = service(
        &store,
        vec![MockResponse::json(
            200,
            &home_body(Some("uid"), &["target"]),
        )],
    );
    assert!(block_on(auth.check()).unwrap().is_success());
    let requests: Vec<_> = requests.try_iter().collect();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].target.ends_with("gethome"));
}

#[test]
fn check_without_credentials_is_not_signed_in_and_makes_no_request() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let (auth, requests) = service(&store, vec![]);
    let report = block_on(auth.check()).unwrap();
    assert!(matches!(report.ha, AuthenticationState::NotSignedIn));
    assert_eq!(report.certificate, None);
    assert!(!report.is_success());
    assert!(requests.try_recv().is_err());
}

#[test]
fn stopped_refresh_survives_reopening_and_check_does_not_retry() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let record = valid_record("uid", "access", NOW, NOW + 1_000_000);
    store.xiaomi().fixture_replace(&record).unwrap();
    let (auth, requests) = service(&store, vec![MockResponse::json(401, "denied")]);
    assert!(matches!(
        block_on(auth.check()).unwrap().ha,
        AuthenticationState::SignInRequired(_)
    ));
    assert_eq!(requests.try_iter().count(), 1);
    drop(auth);
    drop(store);

    let reopened = Store::open(dir.path()).unwrap();
    let (auth, requests) = service(&reopened, vec![]);
    for _ in 0..2 {
        let report = block_on(auth.check()).unwrap();
        assert!(matches!(report.ha, AuthenticationState::SignInRequired(_)));
        assert_eq!(report.certificate.unwrap().not_after, NOW + 1_000_000);
    }
    assert!(requests.try_recv().is_err());
    assert!(
        reopened
            .xiaomi()
            .load()
            .unwrap()
            .unwrap()
            .ha
            .unwrap()
            .stopped
    );
}

#[test]
fn local_status_reports_checking_with_real_certificate_dates_without_http() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let (auth, requests) = service(&store, vec![]);
    let empty = auth.local_status().unwrap();
    assert!(matches!(empty.ha, AuthenticationState::NotSignedIn));
    assert_eq!(empty.certificate, None);

    let record = valid_record("uid", "access", NOW + 500, NOW + 1_000_000);
    store.xiaomi().fixture_replace(&record).unwrap();
    let status = auth.local_status().unwrap();
    assert!(matches!(status.ha, AuthenticationState::Checking));
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
fn proactive_refresh_followed_by_rejected_home_stops_the_new_generation() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store
        .xiaomi()
        .fixture_replace(&valid_record("uid", "old", NOW, NOW + 1_000_000))
        .unwrap();
    let (auth, requests) = service(
        &store,
        vec![
            MockResponse::json(200, &token_body("fresh", "rotated")),
            MockResponse::json(401, "denied"),
        ],
    );
    let report = block_on(auth.check()).unwrap();
    assert!(matches!(report.ha, AuthenticationState::SignInRequired(_)));
    let saved = store.xiaomi().load().unwrap().unwrap();
    assert!(saved.ha.as_ref().unwrap().stopped);
    assert_eq!(
        saved.ha.unwrap().authorization.unwrap().tokens.access_token,
        "fresh"
    );
    let requests: Vec<_> = requests.try_iter().collect();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.target.ends_with("gethome"))
            .count(),
        1
    );
}

#[test]
fn home_401_refreshes_once_and_retries_home_with_new_token() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store
        .xiaomi()
        .fixture_replace(&valid_record("uid", "old", NOW + 500, NOW + 1_000_000))
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
fn home_refresh_exhausts_the_certificate_refresh_allowance() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let original = valid_record("uid", "old", NOW + 500, NOW + 60);
    store.xiaomi().fixture_replace(&original).unwrap();
    let (auth, requests) = service(
        &store,
        vec![
            MockResponse::json(401, "home denied"),
            MockResponse::json(200, &token_body("fresh", "rotated")),
            MockResponse::json(200, &home_body(Some("uid"), &[])),
            MockResponse::json(401, "certificate denied"),
        ],
    );
    let report = block_on(auth.check()).unwrap();
    assert!(matches!(report.ha, AuthenticationState::SignInRequired(_)));
    assert!(matches!(
        report.certificate_update,
        CertificateUpdate::Failed(_)
    ));
    let saved = store.xiaomi().load().unwrap().unwrap();
    assert!(saved.ha.as_ref().unwrap().stopped);
    assert_eq!(
        saved
            .ha
            .unwrap()
            .authorization
            .unwrap()
            .tokens
            .refresh_token,
        "rotated"
    );
    assert_eq!(
        saved.hub.unwrap().certificate_pem,
        original.hub.unwrap().certificate_pem
    );
    assert_eq!(
        requests
            .try_iter()
            .filter(|r| r.target.contains("get_token"))
            .count(),
        1
    );
}

#[test]
fn refresh_is_saved_before_later_certificate_failure() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store
        .xiaomi()
        .fixture_replace(&valid_record("uid", "old", NOW, NOW + 60))
        .unwrap();
    let (auth, _) = service(
        &store,
        vec![
            MockResponse::json(200, &token_body("fresh", "rotated")),
            MockResponse::json(200, &home_body(Some("uid"), &[])),
            MockResponse::json(403, "forbidden"),
        ],
    );
    let report = block_on(auth.check()).unwrap();
    assert!(!report.is_success());
    assert!(matches!(
        report.certificate_update,
        CertificateUpdate::Failed(_)
    ));
    assert_eq!(
        store
            .xiaomi()
            .load()
            .unwrap()
            .unwrap()
            .ha
            .unwrap()
            .authorization
            .unwrap()
            .tokens
            .refresh_token,
        "rotated"
    );
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
            .fixture_replace(&valid_record("uid", "access", NOW, NOW + 1_000_000))
            .unwrap();
        let (auth, _) = service(&store, vec![response]);
        let report = block_on(auth.check()).unwrap();
        assert!(matches!(report.ha, AuthenticationState::Unavailable(_)));
        assert!(!matches!(report.ha, AuthenticationState::SignInRequired(_)));
    }
}
