use super::*;
use crate::{
    storage::{Store, TokenSet, XiaomiRecord},
    xiaomi::{
        certificate::{CertificateValidity, ClientIdentity},
        cloud::CloudClient,
        test_support::{MockResponse, dynamic_mock_server, mock_server, sign_csr},
    },
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures_lite::future::{self, block_on};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicI64, Ordering},
    },
    thread,
    time::Duration,
};

const NOW: i64 = 2_000_000_000;
const UUID: &str = "550e8400-e29b-41d4-a716-446655440000";

fn now() -> i64 {
    NOW
}

fn token_body(access: &str, refresh: &str) -> String {
    json!({"code":0,"result":{"access_token":access,"refresh_token":refresh,"expires_in":1000}})
        .to_string()
}

fn home_body(uid: Option<&str>, dids: &[&str]) -> String {
    let homes = uid
        .map(|uid| vec![json!({"uid":uid,"dids":dids})])
        .unwrap_or_default();
    json!({"code":0,"result":{"homelist":homes}}).to_string()
}

fn devices_body() -> &'static str {
    r#"{"code":0,"result":{"list":[]}}"#
}

fn certificate_response(request: &crate::xiaomi::test_support::ReceivedRequest) -> MockResponse {
    let body: Value = serde_json::from_str(&request.body).unwrap();
    let csr = STANDARD.decode(body["csr"].as_str().unwrap()).unwrap();
    let csr = String::from_utf8(csr).unwrap();
    let certificate = sign_csr(&csr, NOW - 60, NOW + 10 * 24 * 60 * 60).unwrap();
    MockResponse::json(
        200,
        &json!({"code":0,"result":{"cert":certificate}}).to_string(),
    )
}

fn valid_record(uid: &str, access: &str, refresh_at: i64, not_after: i64) -> XiaomiRecord {
    let identity = ClientIdentity::generate(uid).unwrap();
    let certificate = sign_csr(&identity.csr_pem, NOW - 60, not_after).unwrap();
    XiaomiRecord {
        uid: uid.into(),
        region: "cn".into(),
        oauth_client_uuid: UUID.into(),
        redirect_uri:
            "http://homeassistant.local:8123/api/webhook/0123456789abcdef0123456789abcdef".into(),
        tokens: TokenSet {
            access_token: access.into(),
            refresh_token: "refresh-old".into(),
            expires_at: NOW + 1000,
            refresh_at,
        },
        virtual_did: identity.virtual_did,
        private_key_pem: identity.private_key_pem,
        certificate_pem: certificate,
    }
}

fn service(
    store: &Store,
    responses: Vec<MockResponse>,
) -> (
    AuthService,
    std::sync::mpsc::Receiver<crate::xiaomi::test_support::ReceivedRequest>,
) {
    let (base, requests) = mock_server(responses);
    let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    (
        AuthService::with_clock(store.xiaomi(), cloud, now),
        requests,
    )
}

fn callback(attempt: &LoginAttempt) -> String {
    format!(
        "{}?code=code&state={}",
        attempt.authorization().redirect_uri(),
        attempt.authorization().state()
    )
}

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
    assert!(report.is_success());
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
    assert!(report.is_success());
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
            sign_in_required
        );
        assert_eq!(
            matches!(report.authentication, AuthenticationState::Unavailable(_)),
            !sign_in_required
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
        auth.logout().unwrap();
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

async fn wait_for_signal(signal: Arc<AtomicBool>) {
    while !signal.load(Ordering::SeqCst) {
        async_io::Timer::after(Duration::from_millis(1)).await;
    }
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
