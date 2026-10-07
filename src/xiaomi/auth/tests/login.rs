use super::*;

#[test]
fn login_succeeds_persists_and_restart_reuses_oauth_and_client_identity() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let (base, requests) = dynamic_mock_server(7, |request| {
        if request.target.contains("get_token") {
            MockResponse::json(200, &token_body("access-new", "refresh-new"))
        } else if request.target.ends_with("gethome") {
            MockResponse::json(200, &home_body(Some("uid-1"), &["did-1"]))
        } else if request.target.ends_with("device_list_page") {
            MockResponse::json(200, devices_body())
        } else {
            certificate_response(request)
        }
    });
    let auth = AuthService::with_clock(
        store.xiaomi(),
        CloudClient::for_test(&base, Duration::from_secs(1)).unwrap(),
        now,
    );
    let first = auth.begin_login().unwrap();
    let first_uuid = first.authorization().oauth_client_uuid().to_owned();
    let first_callback = callback(&first);
    let report = block_on(auth.complete_login(first, &first_callback)).unwrap();
    assert!(report.is_success(), "{report:?}");
    let saved = store.xiaomi().load().unwrap().unwrap();
    assert_eq!(saved.oauth_client_uuid, first_uuid);
    assert_eq!(saved.tokens.access_token, "access-new");
    let old_key = saved.private_key_pem.clone();
    let old_did = saved.virtual_did.clone();
    let old_cert = saved.certificate_pem.clone();

    let second = auth.begin_login().unwrap();
    assert_eq!(second.authorization().oauth_client_uuid(), first_uuid);
    let second_callback = callback(&second);
    assert!(
        block_on(auth.complete_login(second, &second_callback))
            .unwrap()
            .is_success()
    );
    let restarted = Store::open(dir.path())
        .unwrap()
        .xiaomi()
        .load()
        .unwrap()
        .unwrap();
    assert_eq!(restarted.private_key_pem, old_key);
    assert_eq!(restarted.virtual_did, old_did);
    assert_eq!(restarted.certificate_pem, old_cert);
    assert_eq!(requests.try_iter().count(), 7);
}

#[test]
fn same_uid_renews_with_same_identity_while_changed_uid_gets_new_identity() {
    for (cloud_uid, same) in [("old-uid", true), ("new-uid", false)] {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let old = valid_record("old-uid", "old-access", NOW + 500, NOW + 60);
        store.xiaomi().replace(&old).unwrap();
        let (base, _) = dynamic_mock_server(3, move |request| {
            if request.target.contains("get_token") {
                MockResponse::json(200, &token_body("new-access", "new-refresh"))
            } else if request.target.ends_with("gethome") {
                MockResponse::json(200, &home_body(Some(cloud_uid), &[]))
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
        let callback = callback(&attempt);
        assert!(
            block_on(auth.complete_login(attempt, &callback))
                .unwrap()
                .is_success()
        );
        let saved = store.xiaomi().load().unwrap().unwrap();
        assert_eq!(saved.oauth_client_uuid, old.oauth_client_uuid);
        assert_eq!(saved.private_key_pem == old.private_key_pem, same);
        assert_eq!(saved.virtual_did == old.virtual_did, same);
    }
}

#[test]
fn failed_login_inputs_cloud_identity_certificate_and_storage_preserve_old_record() {
    let cases = [
        vec![],
        vec![MockResponse::json(403, "forbidden")],
        vec![
            MockResponse::json(200, &token_body("new", "new-r")),
            MockResponse::json(200, &home_body(None, &[])),
        ],
        vec![
            MockResponse::json(200, &token_body("new", "new-r")),
            MockResponse::json(200, &home_body(Some("uid"), &["did"])),
            MockResponse::json(500, "device failed"),
        ],
        vec![
            MockResponse::json(200, &token_body("new", "new-r")),
            MockResponse::json(200, &home_body(Some("uid"), &[])),
            MockResponse::json(200, r#"{"code":0,"result":{"cert":"invalid"}}"#),
        ],
    ];
    for (index, responses) in cases.into_iter().enumerate() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let old = valid_record("old", "old-access", NOW + 500, NOW + 1_000_000);
        store.xiaomi().replace(&old).unwrap();
        let (auth, _) = service(&store, responses);
        let attempt = auth.begin_login().unwrap();
        let input = if index == 0 {
            "not a callback".into()
        } else {
            callback(&attempt)
        };
        let report = block_on(auth.complete_login(attempt, &input)).unwrap();
        assert!(!report.is_success());
        assert_eq!(store.xiaomi().load().unwrap(), Some(old));
    }

    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let old = valid_record("uid", "old", NOW + 500, NOW + 1_000_000);
    store.xiaomi().replace(&old).unwrap();
    let (base, _) = dynamic_mock_server(2, |request| {
        if request.target.contains("get_token") {
            MockResponse::json(200, &token_body("new", "new-r"))
        } else {
            MockResponse::json(200, &home_body(Some("uid"), &[]))
        }
    });
    Connection::open(store.path()).unwrap().execute_batch("CREATE TRIGGER reject_auth BEFORE UPDATE ON xiaomi_auth BEGIN SELECT RAISE(ABORT, 'reject'); END;").unwrap();
    let auth = AuthService::with_clock(
        store.xiaomi(),
        CloudClient::for_test(&base, Duration::from_secs(1)).unwrap(),
        now,
    );
    let attempt = auth.begin_login().unwrap();
    let input = callback(&attempt);
    assert!(matches!(
        block_on(auth.complete_login(attempt, &input)),
        Err(AuthError::Storage(_))
    ));
    assert_eq!(store.xiaomi().load().unwrap(), Some(old));
}

#[test]
fn login_http_401_is_sign_in_required() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let (auth, _) = service(&store, vec![MockResponse::json(401, "denied")]);
    let attempt = auth.begin_login().unwrap();
    let input = callback(&attempt);
    let report = block_on(auth.complete_login(attempt, &input)).unwrap();
    assert!(matches!(
        report.authentication,
        AuthenticationState::SignInRequired(_)
    ));
}

#[test]
fn login_rejects_previous_callback_when_oauth_identity_and_state_are_reused() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let previous = valid_record("uid", "old-access", NOW + 500, NOW + 1_000_000);
    store.xiaomi().replace(&previous).unwrap();
    let (auth, requests) = service(&store, vec![]);
    let first = auth.begin_login().unwrap();
    let current = auth.begin_login().unwrap();
    assert_eq!(
        first.authorization().state(),
        current.authorization().state()
    );
    let report = block_on(auth.complete_login(current, &callback(&first))).unwrap();
    assert!(!report.is_success());
    assert_eq!(store.xiaomi().load().unwrap(), Some(previous));
    assert!(requests.try_recv().is_err());
}

#[test]
fn oauth_diagnostics_preserve_credentials_and_authentication_classification() {
    for (oauth_code, login) in [(96002, true), (96002, false), (96013, true), (96013, false)] {
        let body = json!({
            "code": -6,
            "message": json!({"error": oauth_code, "error_description": "response-secret"}).to_string(),
        })
        .to_string();
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let previous = valid_record("uid", "old-access", NOW, NOW + 1_000_000);
        store.xiaomi().replace(&previous).unwrap();
        let (auth, requests) = service(&store, vec![MockResponse::json(200, &body)]);
        let report = if login {
            let attempt = auth.begin_login().unwrap();
            let input = callback(&attempt);
            block_on(auth.complete_login(attempt, &input)).unwrap()
        } else {
            block_on(auth.check()).unwrap()
        };
        let AuthenticationState::Unavailable(FailureReason::Cloud(error)) = &report.authentication
        else {
            panic!("unexpected authentication state: {report:?}");
        };
        assert!(
            error
                .to_string()
                .contains(&format!("OAuth error {oauth_code}: "))
        );
        assert!(!format!("{report:?}").contains("secret"));
        assert_eq!(
            report.certificate,
            Some(CertificateValidity {
                not_before: NOW - 60,
                not_after: NOW + 1_000_000,
            })
        );
        assert!(!report.is_success());
        assert_eq!(store.xiaomi().load().unwrap(), Some(previous));
        assert_eq!(requests.try_iter().count(), 1);
    }
}

#[test]
fn login_certificate_rejection_keeps_credentials_and_classifies_only_401_as_invalid_auth() {
    for (response, sign_in_required) in [
        (MockResponse::json(401, "denied"), true),
        (MockResponse::json(403, "forbidden"), false),
        (MockResponse::json(200, r#"{"code":-9}"#), false),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let previous = valid_record("old-uid", "old", NOW + 500, NOW + 1_000_000);
        store.xiaomi().replace(&previous).unwrap();
        let (auth, _) = service(
            &store,
            vec![
                MockResponse::json(200, &token_body("candidate", "candidate-refresh")),
                MockResponse::json(200, &home_body(Some("new-uid"), &[])),
                response,
            ],
        );
        let attempt = auth.begin_login().unwrap();
        let input = callback(&attempt);
        let report = block_on(auth.complete_login(attempt, &input)).unwrap();
        assert_eq!(
            matches!(
                report.authentication,
                AuthenticationState::SignInRequired(_)
            ),
            sign_in_required,
            "{report:?}"
        );
        assert_eq!(
            matches!(report.authentication, AuthenticationState::Authenticated),
            !sign_in_required,
            "{report:?}"
        );
        assert!(matches!(
            report.certificate_update,
            CertificateUpdate::Failed(_)
        ));
        assert!(!report.is_success());
        assert_eq!(store.xiaomi().load().unwrap(), Some(previous));
    }
}
