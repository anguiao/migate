use super::*;
use crate::storage::{AccountRecovery, RecoveryStop};
use crate::xiaomi::{mijia::MijiaClient, tests::support::ReceivedRequest};
use std::sync::mpsc::Receiver;

#[derive(Clone, Copy)]
enum RecoveryResponse {
    Success,
    Rejected,
    Verification,
    Network,
    Protocol,
    BadSignature,
    StsRejected,
    WrongAccount,
    WrongSession,
}

fn credentials() -> AccountCredentials {
    AccountCredentials::new(
        "uid".into(),
        "old-cuser".into(),
        "old-pass".into(),
        AccountClientIdentity::new("stable-device".into(), "agent".into()).unwrap(),
    )
    .unwrap()
}
fn recovery_record() -> XiaomiRecord {
    let mut record = valid_record("uid", "access", NOW + 500, NOW + 1_000_000);
    record.recovery = AccountRecovery {
        credentials: credentials(),
        stop: None,
    };
    record
}
fn recovery_service(
    store: &Store,
    scenario: RecoveryResponse,
) -> (
    AuthService,
    Receiver<ReceivedRequest>,
    Receiver<ReceivedRequest>,
    Receiver<ReceivedRequest>,
) {
    let (account_base, account) = dynamic_mock_server(20, move |request| {
        if request.target.starts_with("/sts?") {
            if matches!(scenario, RecoveryResponse::StsRejected) {
                return MockResponse::json(401, "service ticket rejected");
            }
            return MockResponse::raw(
                "HTTP/1.1 200 Test\r\nSet-Cookie: serviceToken=restored-service; Path=/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
        }
        match scenario {
            RecoveryResponse::Rejected => MockResponse::json(200, r#"{"code":70002}"#),
            RecoveryResponse::Verification => MockResponse::json(200, r#"{"notificationUrl":"https://account.xiaomi.com/verify?ticket=secret"}"#),
            RecoveryResponse::Network => MockResponse::json(503, "unavailable"),
            RecoveryResponse::Protocol => MockResponse::json(200, "not-json"),
            _ => MockResponse::json(200, &json!({"code":0,"userId":if matches!(scenario, RecoveryResponse::WrongAccount) {"other"} else {"uid"},"cUserId":"new-cuser","passToken":"new-pass","ssecurity":"bmV3LXNlY3VyaXR5","location":"https://sts.api.io.mi.com/sts?ticket=secret"}).to_string()),
        }
    });
    let (mijia_base, mijia) = dynamic_mock_server(20, move |request| {
        if request.headers.contains("service-old") {
            return if matches!(scenario, RecoveryResponse::BadSignature) {
                MockResponse::json(200, r#"{"code":1,"message":"invalid signature"}"#)
            } else {
                MockResponse::json(401, "expired")
            };
        }
        let uid = if matches!(scenario, RecoveryResponse::WrongSession) {
            "other"
        } else {
            "uid"
        };
        MockResponse::json(200, &json!({"code":0,"result":{"homelist":[{"uid":uid,"id":"1","dids":[],"roomlist":[]}],"has_more":false}}).to_string())
    });
    let (ha_base, ha) = dynamic_mock_server(20, |_| {
        MockResponse::json(200, &home_body(Some("uid"), &[]))
    });
    (
        AuthService {
            store: store.xiaomi(),
            cloud: CloudClient::for_test(&ha_base, Duration::from_secs(1)).unwrap(),
            account: AccountClient::for_test(&account_base, Duration::from_secs(1)),
            mijia: MijiaClient::for_test(&mijia_base, Duration::from_secs(1)),
            now,
        },
        account,
        mijia,
        ha,
    )
}

#[test]
fn invalid_signature_does_not_trigger_account_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let original = recovery_record();
    store.xiaomi().fixture_replace(&original).unwrap();
    let (auth, account, _, ha) = recovery_service(&store, RecoveryResponse::BadSignature);
    let report = block_on(auth.check()).unwrap();
    assert!(matches!(
        report.mijia,
        AuthenticationState::Unavailable(FailureReason::Cloud(_))
    ));
    assert_eq!(account.try_iter().count(), 0);
    assert_eq!(ha.try_iter().count(), 1);
    assert_eq!(store.xiaomi().load().unwrap(), Some(original));
}

#[test]
fn rejected_saved_session_is_restored_once_and_saved_as_one_pair() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store.xiaomi().fixture_replace(&recovery_record()).unwrap();
    let (auth, account, mijia, ha) = recovery_service(&store, RecoveryResponse::Success);
    let report = block_on(auth.check()).unwrap();
    assert!(report.is_success(), "{report:?}");
    let saved = store.xiaomi().load().unwrap().unwrap();
    assert_eq!(saved.recovery.credentials.pass_token(), "new-pass");
    assert_eq!(saved.mijia.service_token(), "restored-service");
    assert_eq!(account.try_iter().count(), 2);
    assert_eq!(mijia.try_iter().count(), 2);
    assert_eq!(ha.try_iter().count(), 1);
}

#[test]
fn an_online_check_adopts_a_background_mijia_recovery_only_for_the_same_login() {
    enum Change {
        Recovery,
        Login,
        Logout,
    }

    for change in [Change::Recovery, Change::Login, Change::Logout] {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let record = recovery_record();
        store.xiaomi().fixture_replace(&record).unwrap();
        let generation = store
            .xiaomi()
            .snapshot()
            .unwrap()
            .mijia_generation()
            .unwrap();
        let (check, account, _, ha) = recovery_service(&store, RecoveryResponse::Success);
        let result = block_on(async {
            let mut checking = std::pin::pin!(check.check());
            assert!(future::poll_once(checking.as_mut()).await.is_none());
            match change {
                Change::Recovery => {
                    let (background, ..) = recovery_service(&store, RecoveryResponse::Success);
                    let restored = background
                        .recover_mijia(&record.uid, generation)
                        .await
                        .unwrap();
                    assert_eq!(restored.service_token(), "restored-service");
                }
                Change::Login => store
                    .xiaomi()
                    .commit_login(
                        &record.recovery.credentials,
                        &record.mijia,
                        &record.ha.client_uuid,
                        &record.ha.authorization,
                    )
                    .unwrap(),
                Change::Logout => check.logout().unwrap(),
            }
            checking.await
        });
        assert_eq!(account.try_iter().count(), 0);
        if matches!(change, Change::Recovery) {
            let report = result.unwrap();
            assert!(report.is_success(), "{report:?}");
            assert_eq!(ha.try_iter().count(), 1);
        } else {
            assert!(matches!(result, Err(AuthError::Missing(_))), "{result:?}");
            assert_eq!(ha.try_iter().count(), 0);
        }
    }
}

#[test]
fn known_rejected_session_is_restored_without_probing_it_again() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let record = recovery_record();
    store.xiaomi().fixture_replace(&record).unwrap();
    let generation = store
        .xiaomi()
        .snapshot()
        .unwrap()
        .mijia_generation()
        .unwrap();
    let (auth, account, mijia, _) = recovery_service(&store, RecoveryResponse::Success);
    let session = block_on(auth.recover_mijia(&record.uid, generation)).unwrap();
    assert_eq!(session.service_token(), "restored-service");
    assert_eq!(account.try_iter().count(), 2);
    let requests: Vec<_> = mijia.try_iter().collect();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].headers.contains("restored-service"));
}

#[test]
fn stopped_recovery_is_not_retried_from_an_older_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let record = recovery_record();
    store.xiaomi().fixture_replace(&record).unwrap();
    let generation = store
        .xiaomi()
        .snapshot()
        .unwrap()
        .mijia_generation()
        .unwrap();
    store
        .xiaomi()
        .stop_recovery(RecoveryStop::VerificationRequired)
        .unwrap();
    let (auth, account, mijia, _) = recovery_service(&store, RecoveryResponse::Success);
    assert!(matches!(
        block_on(auth.recover_mijia(&record.uid, generation)),
        Err(AuthError::Failure(FailureReason::RecoveryStopped(
            RecoveryStop::VerificationRequired
        )))
    ));
    assert_eq!(account.try_iter().count(), 0);
    assert_eq!(mijia.try_iter().count(), 0);
}

#[test]
fn a_late_recovery_cannot_clear_verification_required_by_an_online_check() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let mut expected = recovery_record();
    store.xiaomi().fixture_replace(&expected).unwrap();
    let generation = store
        .xiaomi()
        .snapshot()
        .unwrap()
        .mijia_generation()
        .unwrap();
    let (background, ..) = recovery_service(&store, RecoveryResponse::Success);
    let (check, ..) = recovery_service(&store, RecoveryResponse::Verification);
    block_on(async {
        let mut pending = std::pin::pin!(background.recover_mijia(&expected.uid, generation));
        assert!(future::poll_once(pending.as_mut()).await.is_none());
        let report = check.check().await.unwrap();
        assert_eq!(
            report.recovery_stop,
            Some(RecoveryStop::VerificationRequired)
        );
        assert!(matches!(
            pending.await,
            Err(AuthError::Failure(FailureReason::RecoveryStopped(
                RecoveryStop::VerificationRequired
            )))
        ));
    });
    expected.recovery.stop = Some(RecoveryStop::VerificationRequired);
    assert_eq!(store.xiaomi().load().unwrap(), Some(expected));
}

#[test]
fn rejected_or_verification_recovery_stops_across_reopen_but_not_ha_or_certificate() {
    for (scenario, stop) in [
        (RecoveryResponse::Rejected, RecoveryStop::Rejected),
        (
            RecoveryResponse::Verification,
            RecoveryStop::VerificationRequired,
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.xiaomi().fixture_replace(&recovery_record()).unwrap();
        let (auth, account, _, ha) = recovery_service(&store, scenario);
        let first = block_on(auth.check()).unwrap();
        assert!(matches!(
            first.mijia,
            AuthenticationState::SignInRequired(_)
        ));
        assert!(matches!(first.ha, AuthenticationState::Authenticated));
        assert!(first.certificate.is_some());
        assert_eq!(account.try_iter().count(), 1);
        assert_eq!(ha.try_iter().count(), 1);
        drop(auth);
        drop(store);
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(
            store.xiaomi().load().unwrap().unwrap().recovery.stop,
            Some(stop)
        );
        let (auth, account, _, ha) = recovery_service(&store, scenario);
        for _ in 0..2 {
            let report = block_on(auth.check()).unwrap();
            assert!(matches!(
                report.mijia,
                AuthenticationState::SignInRequired(FailureReason::RecoveryStopped(_))
            ));
            assert!(matches!(report.ha, AuthenticationState::Authenticated));
        }
        assert_eq!(account.try_iter().count(), 0);
        assert_eq!(ha.try_iter().count(), 2);
    }
}

#[test]
fn temporary_protocol_and_identity_failures_never_stop_or_replace_recovery() {
    for scenario in [
        RecoveryResponse::Network,
        RecoveryResponse::Protocol,
        RecoveryResponse::WrongAccount,
        RecoveryResponse::WrongSession,
        RecoveryResponse::StsRejected,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let original = recovery_record();
        store.xiaomi().fixture_replace(&original).unwrap();
        let (auth, account, _, ha) = recovery_service(&store, scenario);
        let report = block_on(auth.check()).unwrap();
        assert!(matches!(report.mijia, AuthenticationState::Unavailable(_)));
        assert!(matches!(report.ha, AuthenticationState::Authenticated));
        assert_eq!(store.xiaomi().load().unwrap(), Some(original));
        assert!(account.try_iter().count() <= 2);
        assert_eq!(ha.try_iter().count(), 1);
    }
}

#[test]
fn stopped_recovery_does_not_invalidate_a_working_saved_session() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let mut original = recovery_record();
    original.recovery.stop = Some(RecoveryStop::VerificationRequired);
    original.mijia = crate::xiaomi::mijia::MijiaSession::new(
        "uid".into(),
        "working".into(),
        "c2VjdXJpdHk=".into(),
        "agent".into(),
    )
    .unwrap();
    store.xiaomi().fixture_replace(&original).unwrap();
    let (auth, account, _, _) = recovery_service(&store, RecoveryResponse::Rejected);
    let report = block_on(auth.check()).unwrap();
    assert!(report.is_success(), "{report:?}");
    let output = crate::terminal::format_report(
        &report,
        NOW,
        std::path::Path::new("migate"),
        std::path::Path::new("/data"),
    );
    assert!(output.contains("Mijia session: available."));
    assert!(output.contains("Account recovery: stopped"));
    assert!(output.contains("https://www.mi.com/"));
    assert!(output.contains("auth login"));
    assert_eq!(account.try_iter().count(), 0);
    assert_eq!(store.xiaomi().load().unwrap(), Some(original));
}
