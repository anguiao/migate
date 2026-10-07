use super::*;

#[test]
fn certificate_renewal_reuses_key_and_failure_is_independent_from_authentication() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let old = valid_record("uid", "access", NOW + 500, NOW + 60);
    store.xiaomi().replace(&old).unwrap();
    let (base, _) = dynamic_mock_server(2, |request| {
        if request.target.ends_with("gethome") {
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
    let report = block_on(auth.check()).unwrap();
    assert!(report.is_success(), "{report:?}");
    assert!(matches!(
        report.certificate_update,
        CertificateUpdate::Updated
    ));
    let saved = store.xiaomi().load().unwrap().unwrap();
    assert_eq!(saved.private_key_pem, old.private_key_pem);
    assert_eq!(saved.virtual_did, old.virtual_did);
    assert_ne!(saved.certificate_pem, old.certificate_pem);

    let mut expiring = saved;
    let identity = ClientIdentity::from_private_key(
        &expiring.uid,
        &expiring.virtual_did,
        &expiring.private_key_pem,
    )
    .unwrap();
    expiring.certificate_pem = sign_csr(&identity.csr_pem, NOW - 60, NOW + 60).unwrap();
    let old_cert = expiring.certificate_pem.clone();
    store.xiaomi().replace(&expiring).unwrap();
    let (auth, _) = service(
        &store,
        vec![
            MockResponse::json(200, &home_body(Some("uid"), &[])),
            MockResponse::json(403, "forbidden"),
        ],
    );
    let report = block_on(auth.check()).unwrap();
    assert!(matches!(
        report.authentication,
        AuthenticationState::Authenticated
    ));
    assert!(matches!(
        report.certificate_update,
        CertificateUpdate::Failed(_)
    ));
    assert_eq!(
        store.xiaomi().load().unwrap().unwrap().certificate_pem,
        old_cert
    );
}

#[test]
fn future_certificate_with_three_days_remaining_is_renewed() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let mut record = valid_record("uid", "access", NOW + 500, NOW + 1_000);
    let identity =
        ClientIdentity::from_private_key(&record.uid, &record.virtual_did, &record.private_key_pem)
            .unwrap();
    record.certificate_pem = sign_csr(&identity.csr_pem, NOW + 100, NOW + 1_000).unwrap();
    let old_certificate = record.certificate_pem.clone();
    store.xiaomi().replace(&record).unwrap();
    let (base, requests) = dynamic_mock_server(2, |request| {
        if request.target.ends_with("gethome") {
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
    let report = block_on(auth.check()).unwrap();
    assert!(matches!(
        report.certificate_update,
        CertificateUpdate::Updated
    ));
    assert_ne!(
        store.xiaomi().load().unwrap().unwrap().certificate_pem,
        old_certificate
    );
    assert_eq!(requests.try_iter().count(), 2);
}

#[test]
fn certificate_401_refresh_failures_report_failed_renewal_and_cloud_state() {
    let cases = [
        (
            vec![
                MockResponse::json(200, &home_body(Some("uid"), &[])),
                MockResponse::json(401, "certificate denied"),
                MockResponse::json(401, "refresh denied"),
            ],
            true,
        ),
        (
            vec![
                MockResponse::json(200, &home_body(Some("uid"), &[])),
                MockResponse::json(401, "certificate denied"),
                MockResponse::json(500, "refresh unavailable"),
            ],
            false,
        ),
        (
            vec![
                MockResponse::json(200, &home_body(Some("uid"), &[])),
                MockResponse::json(401, "certificate denied"),
                MockResponse::json(200, &token_body("fresh", "rotated")),
                MockResponse::json(401, "certificate denied again"),
            ],
            true,
        ),
    ];
    for (responses, sign_in_required) in cases {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let record = valid_record("uid", "access", NOW + 500, NOW + 60);
        let old_certificate = record.certificate_pem.clone();
        store.xiaomi().replace(&record).unwrap();
        let (auth, _) = service(&store, responses);
        let report = block_on(auth.check()).unwrap();
        assert_eq!(
            matches!(
                report.authentication,
                AuthenticationState::SignInRequired(_)
            ),
            sign_in_required,
            "{report:?}"
        );
        assert_eq!(
            matches!(report.authentication, AuthenticationState::Unavailable(_)),
            !sign_in_required,
            "{report:?}"
        );
        assert!(matches!(
            report.certificate_update,
            CertificateUpdate::Failed(_)
        ));
        assert_eq!(
            report.certificate,
            Some(CertificateValidity {
                not_before: NOW - 60,
                not_after: NOW + 60
            })
        );
        assert_eq!(
            store.xiaomi().load().unwrap().unwrap().certificate_pem,
            old_certificate
        );
    }
}

#[test]
fn shared_refresh_budget_covers_certificate_and_classifies_401_403_and_business_errors() {
    for (responses, expected_sign_in) in [
        (
            vec![
                MockResponse::json(200, &home_body(Some("uid"), &[])),
                MockResponse::json(401, "denied"),
                MockResponse::json(401, "refresh denied"),
            ],
            true,
        ),
        (
            vec![
                MockResponse::json(200, &home_body(Some("uid"), &[])),
                MockResponse::json(401, "denied"),
                MockResponse::json(200, &token_body("fresh", "rotated")),
                MockResponse::json(401, "denied again"),
            ],
            true,
        ),
        (vec![MockResponse::json(403, "forbidden")], false),
        (vec![MockResponse::json(200, r#"{"code":-9}"#)], false),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store
            .xiaomi()
            .replace(&valid_record("uid", "access", NOW + 500, NOW + 60))
            .unwrap();
        let (auth, _) = service(&store, responses);
        let report = block_on(auth.check()).unwrap();
        assert_eq!(
            matches!(
                report.authentication,
                AuthenticationState::SignInRequired(_)
            ),
            expected_sign_in
        );
    }
}

#[test]
fn offline_keeps_real_certificate_validity_and_future_or_expired_is_unsuccessful() {
    for (not_before, not_after) in [
        (NOW - 10, NOW + 1_000_000),
        (NOW + 10, NOW + 1_000_000),
        (NOW - 100, NOW),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let mut record = valid_record("uid", "access", NOW + 500, not_after.max(NOW + 1));
        let identity =
            ClientIdentity::from_private_key("uid", &record.virtual_did, &record.private_key_pem)
                .unwrap();
        record.certificate_pem = sign_csr(&identity.csr_pem, not_before, not_after).unwrap();
        store.xiaomi().replace(&record).unwrap();
        let (auth, _) = service(&store, vec![MockResponse::json(500, "offline")]);
        let report = block_on(auth.check()).unwrap();
        assert_eq!(
            report.certificate,
            Some(CertificateValidity {
                not_before,
                not_after
            })
        );
        assert!(!report.is_success());
    }
}
