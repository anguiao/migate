use super::*;
use crate::xiaomi::tests::support::ReceivedRequest;
use crate::{
    config::AuthCommand,
    storage::{AccountRecovery, RecoveryStop},
    terminal::{
        auth::OnlineAuth,
        management::{Output, Session, read_foreground},
    },
};
use async_io::{Async, Timer};
use futures_lite::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use std::{
    cell::Cell, net::Shutdown, os::unix::net::UnixStream, path::Path, pin::pin,
    sync::mpsc::Receiver,
};
use url::Url;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Failure {
    None,
    LongQr,
    QrVerification,
    Expired,
    WrongQrUid,
    Sts,
    Mijia,
    MijiaUid,
    Consent,
    ContextUid,
    Verification,
    Token,
    TokenDelay,
    TokenPending,
    HomeUid,
    HomeMixedUid,
    HomeMissing,
    Certificate,
    CertificatePending,
}

struct Peers {
    auth: AuthService,
    account: Receiver<ReceivedRequest>,
    mijia: Receiver<ReceivedRequest>,
    ha: Receiver<ReceivedRequest>,
    token_started: Arc<AtomicBool>,
    token_release: flume::Sender<()>,
    certificate_started: Arc<AtomicBool>,
    certificate_finished: Arc<AtomicBool>,
    certificate_release: flume::Sender<()>,
}

fn response(status: u16, headers: &str, body: &str) -> MockResponse {
    MockResponse::raw(format!(
        "HTTP/1.1 {status} Test\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    ))
}
fn redirect(location: &str) -> MockResponse {
    response(302, &format!("Location: {location}\r\n"), "")
}
fn qr_body() -> String {
    json!({"loginUrl":"https://c3.account.xiaomi.com/longPolling/scan?ticket=qr-secret", "lp":"https://c3.lp.account.xiaomi.com/longPolling/login?ticket=poll-secret", "qr":"https://c3.account.xiaomi.com/longPolling/qr?ticket=qr-secret", "timeout":60}).to_string()
}
fn make_peers(store: &Store, failure: Failure) -> Peers {
    let mut step = 0usize;
    let mut redirect_uri = String::new();
    let mut state = String::new();
    let mut followup = String::new();
    let (account_base, account) = dynamic_mock_server(30, move |request| {
        let current = step % 10;
        step += 1;
        match current {
            0 if failure == Failure::QrVerification => redirect("https://account.xiaomi.com/pass/notification?ticket=verify-secret"),
            0 => {
                let body = if failure == Failure::LongQr { qr_body().replace("qr-secret", &"x".repeat(5000)) } else if failure == Failure::Expired { qr_body().replace("\"timeout\":60", "\"timeout\":1") } else { qr_body() };
                response(200, "Set-Cookie: qrSession=qr-cookie; Domain=account.xiaomi.com; Path=/\r\n", &body)
            }
            1 => {
                let response = MockResponse::json(200, &format!("&&&START&&&{}", json!({"code":0,"userId":if failure == Failure::WrongQrUid {"other"} else {"uid"},"cUserId":"cuser-new","passToken":"pass-new","ssecurity":"c2VjdXJpdHk=","location":"https://sts.api.io.mi.com/sts?ticket=sts-secret"})));
                if failure == Failure::Expired { response.delayed(Duration::from_millis(1200)) } else { response }
            }
            2 if failure == Failure::Sts => MockResponse::json(503, "unavailable"),
            2 => response(200, "Set-Cookie: serviceToken=service-new; Path=/\r\n", ""),
            3 => {
                let url = Url::parse(&format!("https://account.xiaomi.com{}", request.target)).unwrap();
                let query: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
                redirect_uri = query["redirect_uri"].clone();
                state = query["state"].clone();
                followup = url.to_string();
                redirect("/pass/serviceLogin?sid=oauth")
            }
            4 => redirect("/sts/oauth?ticket=oauth-ticket"),
            5 => redirect("/fe/service/oauth2/authorize"),
            6 => response(200, "Content-Type: text/html\r\n", "consent SPA"),
            7 if failure == Failure::Consent => MockResponse::json(200, r#"{"code":96012}"#),
            7 if failure == Failure::Verification => MockResponse::json(200, r#"{"notificationUrl":"https://account.xiaomi.com/pass/notification?ticket=verify-secret"}"#),
            7 => MockResponse::json(200, &json!({"code":0,"data":{"user":{"userId":if failure == Failure::ContextUid {"other"} else {"uid"}},"client_id":crate::xiaomi::cloud::CLIENT_ID.to_string(),"redirect_uri":redirect_uri,"pt":"pt-secret","device_id":"0123456789abcdef0123456789abcdef","followup":followup,"scope_id":"1 2","_ssign":"ssign-secret"}}).to_string()),
            8 => MockResponse::json(200, r#"{"code":0,"data":{"followup":"https://account.xiaomi.com/sts/oauth?ticket=grant-secret"}}"#),
            9 => redirect(&format!("{redirect_uri}?code=code-secret&state={state}")),
            _ => unreachable!(),
        }
    });
    let (mijia_base, mijia) = dynamic_mock_server(20, move |_| {
        if failure == Failure::Mijia {
            return MockResponse::json(503, "unavailable");
        }
        let uid = if failure == Failure::MijiaUid {
            "other"
        } else {
            "uid"
        };
        MockResponse::json(200, &json!({"code":0,"result":{"homelist":[{"uid":uid,"id":"1","dids":[],"roomlist":[]}],"has_more":false}}).to_string())
    });
    let mut certificates = 0;
    let token_started = Arc::new(AtomicBool::new(false));
    let token_signal = token_started.clone();
    let (token_release, token_gate) = flume::bounded(1);
    let certificate_started = Arc::new(AtomicBool::new(false));
    let certificate_signal = certificate_started.clone();
    let certificate_finished = Arc::new(AtomicBool::new(false));
    let certificate_completion = certificate_finished.clone();
    let (certificate_release, certificate_gate) = flume::bounded(1);
    let (ha_base, ha) = dynamic_mock_server(30, move |request| {
        if request.target.contains("get_token") {
            token_signal.store(true, Ordering::SeqCst);
            if failure == Failure::Token {
                MockResponse::json(503, "unavailable")
            } else if failure == Failure::TokenPending {
                MockResponse::json(200, &token_body("access-new", "refresh-new"))
                    .gated(token_gate.clone())
            } else if failure == Failure::TokenDelay {
                MockResponse::json(200, &token_body("access-new", "refresh-new"))
                    .delayed(Duration::from_millis(200))
            } else {
                MockResponse::json(200, &token_body("access-new", "refresh-new"))
            }
        } else if request.target.ends_with("gethome") {
            match failure {
                Failure::HomeUid => MockResponse::json(200, &home_body(Some("other"), &[])),
                Failure::HomeMixedUid => MockResponse::json(
                    200,
                    r#"{"code":0,"result":{"homelist":[{"uid":"uid","dids":[]},{"uid":"other","dids":[]}]}}"#,
                ),
                Failure::HomeMissing => MockResponse::json(200, &home_body(None, &[])),
                _ => MockResponse::json(200, &home_body(Some("uid"), &[])),
            }
        } else if request.target.contains("get_central_crt") {
            certificates += 1;
            if failure == Failure::Certificate && certificates == 1 {
                MockResponse::json(503, "unavailable")
            } else if failure == Failure::CertificatePending && certificates == 1 {
                certificate_signal.store(true, Ordering::SeqCst);
                let finished = certificate_completion.clone();
                certificate_response(request)
                    .gated(certificate_gate.clone())
                    .on_sent(move || finished.store(true, Ordering::SeqCst))
            } else {
                certificate_response(request)
            }
        } else {
            panic!("unexpected HA request {}", request.target)
        }
    });
    Peers {
        auth: AuthService {
            store: store.xiaomi(),
            cloud: CloudClient::for_test(&ha_base, Duration::from_secs(1)).unwrap(),
            account: AccountClient::for_test(&account_base, Duration::from_secs(1)),
            mijia: crate::xiaomi::mijia::MijiaClient::for_test(&mijia_base, Duration::from_secs(1)),
            now,
        },
        account,
        mijia,
        ha,
        token_started,
        token_release,
        certificate_started,
        certificate_finished,
        certificate_release,
    }
}
fn previous_record() -> XiaomiRecord {
    let mut previous = valid_record("uid", "access-old", NOW + 500, NOW + 1_000_000);
    previous.recovery = AccountRecovery {
        credentials: AccountCredentials::new(
            "uid".into(),
            "cuser-old".into(),
            "pass-old".into(),
            AccountClientIdentity::new("device-old".into(), "agent-old".into()).unwrap(),
        )
        .unwrap(),
        stop: Some(RecoveryStop::Rejected),
    };
    previous.ha.stopped = true;
    previous
}

#[test]
fn every_login_scans_and_exchanges_a_new_grant_even_when_credentials_exist() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let previous = previous_record();
    store.xiaomi().fixture_replace(&previous).unwrap();
    let peers = make_peers(&store, Failure::None);
    for _ in 0..2 {
        let attempt = block_on(peers.auth.begin_login()).unwrap();
        assert!(attempt.remaining().as_secs() <= 60);
        assert!(!format!("{attempt:?}").contains("secret"));
        let report = block_on(peers.auth.complete_login(attempt)).unwrap();
        assert!(report.is_success(), "{report:?}");
    }
    let saved = store.xiaomi().load().unwrap().unwrap();
    assert_eq!(saved.recovery.credentials.pass_token(), "pass-new");
    assert_eq!(saved.recovery.stop, None);
    assert!(!saved.ha.stopped);
    assert_eq!(saved.ha.client_uuid, UUID);
    assert_eq!(saved.hub, previous.hub);
    let account: Vec<_> = peers.account.try_iter().collect();
    assert_eq!(
        account
            .iter()
            .filter(|r| r.target.starts_with("/longPolling/loginUrl?"))
            .count(),
        2
    );
    assert_eq!(
        account
            .iter()
            .filter(|r| r.target.starts_with("/oauth2/userAuthorization"))
            .count(),
        2
    );
    assert_eq!(peers.mijia.try_iter().count(), 2);
    let ha: Vec<_> = peers.ha.try_iter().collect();
    assert_eq!(
        ha.iter().filter(|r| r.target.contains("get_token")).count(),
        2
    );
    assert_eq!(
        ha.iter().filter(|r| r.target.ends_with("gethome")).count(),
        2
    );
    assert!(ha.iter().all(|r| !r.target.ends_with("device_list_page")));
    for request in ha.iter().filter(|r| r.target.contains("get_token")) {
        assert!(request.target.contains("code-secret"));
        assert!(!request.target.contains("refresh_token"));
    }
}

#[test]
fn failed_candidate_at_each_stage_preserves_every_old_field_and_stop() {
    for failure in [
        Failure::Expired,
        Failure::WrongQrUid,
        Failure::Sts,
        Failure::Mijia,
        Failure::MijiaUid,
        Failure::Consent,
        Failure::ContextUid,
        Failure::Verification,
        Failure::Token,
        Failure::HomeUid,
        Failure::HomeMixedUid,
        Failure::HomeMissing,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let previous = previous_record();
        store.xiaomi().fixture_replace(&previous).unwrap();
        let peers = make_peers(&store, failure);
        let attempt = block_on(peers.auth.begin_login()).unwrap();
        let report = block_on(peers.auth.complete_login(attempt)).unwrap();
        assert!(!report.is_success());
        assert!(report.login_failure.is_some());
        assert_eq!(store.xiaomi().load().unwrap(), Some(previous));
        let output =
            crate::terminal::format_report(&report, NOW, Path::new("migate"), Path::new("/data"));
        for secret in [
            "pass-old",
            "pass-new",
            "service-old",
            "service-new",
            "qr-secret",
            "sts-secret",
            "verify-secret",
            "access-old",
            "access-new",
        ] {
            assert!(!format!("{report:?} {output}").contains(secret));
        }
        if failure == Failure::Verification {
            assert!(output.contains("https://www.mi.com/"));
            assert!(output.contains("may not prevent another verification"));
        }
        if failure == Failure::WrongQrUid {
            assert!(output.contains("auth logout"));
        }
        if matches!(
            failure,
            Failure::MijiaUid | Failure::ContextUid | Failure::HomeUid | Failure::HomeMixedUid
        ) {
            assert!(!output.contains("auth logout"));
        }
    }
}

#[test]
fn cancelling_a_pending_candidate_preserves_the_old_record() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let previous = previous_record();
    store.xiaomi().fixture_replace(&previous).unwrap();
    let peers = make_peers(&store, Failure::TokenDelay);
    let attempt = block_on(peers.auth.begin_login()).unwrap();
    block_on(future::or(
        async {
            let _ = peers.auth.complete_login(attempt).await;
        },
        wait_for_signal(peers.token_started.clone()),
    ));
    thread::sleep(Duration::from_millis(250));
    assert_eq!(store.xiaomi().load().unwrap(), Some(previous));
}

async fn wait_for_test_signal(signal: Arc<AtomicBool>) {
    future::or(wait_for_signal(signal), async {
        async_io::Timer::after(Duration::from_secs(2)).await;
        panic!("timed out waiting for the test peer");
    })
    .await;
}

async fn wait_for_requests(receiver: &Receiver<ReceivedRequest>, expected: usize) {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    for _ in 0..expected {
        loop {
            if receiver.try_recv().is_ok() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for a test request"
            );
            async_io::Timer::after(Duration::from_millis(1)).await;
        }
    }
}

#[test]
fn cancelling_before_commit_reports_local_state_and_preserves_old_credentials() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let previous = previous_record();
    store.xiaomi().fixture_replace(&previous).unwrap();
    let peers = make_peers(&store, Failure::TokenDelay);
    let mut output = Vec::new();
    block_on(async {
        let interrupted = future::or(
            async {
                crate::terminal::auth::login(
                    &peers.auth,
                    Path::new("migate"),
                    Path::new("/data"),
                    &mut output,
                )
                .await
                .map(|_| false)
            },
            async {
                wait_for_test_signal(peers.token_started.clone()).await;
                Ok::<_, crate::RuntimeError>(true)
            },
        )
        .await
        .unwrap();
        assert!(interrupted);
        wait_for_requests(&peers.account, 10).await;
        wait_for_requests(&peers.mijia, 1).await;
        wait_for_requests(&peers.ha, 1).await;
        crate::terminal::auth::report_local_status(
            &peers.auth,
            Path::new("migate"),
            Path::new("/data"),
            &mut output,
        )
        .await
        .unwrap();
        assert!(peers.account.try_recv().is_err());
        assert!(peers.mijia.try_recv().is_err());
        assert!(peers.ha.try_recv().is_err());
    });
    thread::sleep(Duration::from_millis(250));
    assert_eq!(store.xiaomi().load().unwrap(), Some(previous));
    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("Xiaomi account: uid (cn)."), "{output}");
    assert!(output.contains("Mijia session: checking."), "{output}");
    assert!(output.contains("HA OAuth: sign-in required."), "{output}");
    assert!(
        output.contains("Gateway certificate: valid until"),
        "{output}"
    );
    assert!(!output.contains("Mijia session: available."));
    assert!(!output.contains("HA OAuth: available."));
    for secret in [
        "pass-old",
        "service-old",
        "access-old",
        "pass-new",
        "ticket=",
    ] {
        assert!(!output.contains(secret));
    }
}

#[test]
fn cancelling_during_certificate_request_reports_saved_credentials_and_check_can_resume() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let peers = make_peers(&store, Failure::CertificatePending);
    let mut output = Vec::new();
    block_on(async {
        let interrupted = future::or(
            async {
                crate::terminal::auth::login(
                    &peers.auth,
                    Path::new("migate"),
                    Path::new("/data"),
                    &mut output,
                )
                .await
                .map(|_| false)
            },
            async {
                wait_for_test_signal(peers.certificate_started.clone()).await;
                Ok::<_, crate::RuntimeError>(true)
            },
        )
        .await
        .unwrap();
        assert!(interrupted);
        wait_for_requests(&peers.account, 10).await;
        wait_for_requests(&peers.mijia, 1).await;
        wait_for_requests(&peers.ha, 3).await;
        crate::terminal::auth::report_local_status(
            &peers.auth,
            Path::new("migate"),
            Path::new("/data"),
            &mut output,
        )
        .await
        .unwrap();
        assert!(peers.account.try_recv().is_err());
        assert!(peers.mijia.try_recv().is_err());
        assert!(peers.ha.try_recv().is_err());
    });
    let saved = store.xiaomi().load().unwrap().unwrap();
    assert_eq!(saved.recovery.credentials.pass_token(), "pass-new");
    assert_eq!(saved.mijia.service_token(), "service-new");
    assert_eq!(saved.ha.authorization.tokens.access_token, "access-new");
    let hub = saved.hub.as_ref().unwrap();
    assert!(!hub.virtual_did.is_empty() && !hub.private_key_pem.is_empty());
    assert!(hub.certificate_pem.is_none());
    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("Xiaomi account: uid (cn)."), "{output}");
    assert!(output.contains("Mijia session: checking."), "{output}");
    assert!(output.contains("HA OAuth: checking."), "{output}");
    assert!(
        output.contains("Gateway certificate: not prepared."),
        "{output}"
    );
    assert!(output.contains("auth check"), "{output}");
    assert!(!output.contains("Mijia session: available."));
    assert!(!output.contains("HA OAuth: available."));
    for secret in [
        "pass-new",
        "service-new",
        "access-new",
        "refresh-new",
        "code-secret",
        "ticket=",
        "-----BEGIN",
    ] {
        assert!(!output.contains(secret));
    }
    peers.certificate_release.send(()).unwrap();
    block_on(wait_for_test_signal(peers.certificate_finished.clone()));
    assert_eq!(store.xiaomi().load().unwrap(), Some(saved.clone()));
    let report = block_on(peers.auth.check()).unwrap();
    assert!(report.is_success(), "{report:?}");
    let updated = store.xiaomi().load().unwrap().unwrap();
    assert_eq!(updated.hub.as_ref().unwrap().virtual_did, hub.virtual_did);
    assert_eq!(
        updated.hub.as_ref().unwrap().private_key_pem,
        hub.private_key_pem
    );
    assert!(updated.hub.unwrap().certificate_pem.is_some());
}

#[test]
fn commit_failure_rolls_back_all_candidate_credentials_and_stop_flags() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let previous = previous_record();
    store.xiaomi().fixture_replace(&previous).unwrap();
    Connection::open(store.path()).unwrap().execute_batch("CREATE TRIGGER fail_login BEFORE INSERT ON xiaomi_mijia_session BEGIN SELECT RAISE(ABORT, 'test failure'); END;").unwrap();
    let peers = make_peers(&store, Failure::None);
    let attempt = block_on(peers.auth.begin_login()).unwrap();
    assert!(matches!(
        block_on(peers.auth.complete_login(attempt)),
        Err(AuthError::LoginStep {
            stage: "credential commit",
            ..
        })
    ));
    assert_eq!(store.xiaomi().load().unwrap(), Some(previous));
}

#[test]
fn failed_certificate_keeps_committed_credentials_and_hub_and_check_retries() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let peers = make_peers(&store, Failure::Certificate);
    let attempt = block_on(peers.auth.begin_login()).unwrap();
    let report = block_on(peers.auth.complete_login(attempt)).unwrap();
    assert!(matches!(
        report.certificate_update,
        CertificateUpdate::Failed(FailureReason::Cloud(_))
    ));
    let saved = store.xiaomi().load().unwrap().unwrap();
    assert_eq!(saved.recovery.credentials.pass_token(), "pass-new");
    assert!(saved.hub.as_ref().unwrap().certificate_pem.is_none());
    let report = block_on(peers.auth.check()).unwrap();
    assert!(report.is_success(), "{report:?}");
    let updated = store.xiaomi().load().unwrap().unwrap();
    assert_eq!(
        saved.hub.as_ref().unwrap().private_key_pem,
        updated.hub.as_ref().unwrap().private_key_pem
    );
    assert!(updated.hub.unwrap().certificate_pem.is_some());
    assert_eq!(
        peers
            .account
            .try_iter()
            .filter(|r| r.target.starts_with("/longPolling/loginUrl?"))
            .count(),
        1
    );
}

#[test]
fn terminal_displays_qr_and_purpose_without_manual_callback_or_ticket_text() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let peers = make_peers(&store, Failure::None);
    let mut output = Vec::new();
    let result = block_on(crate::terminal::auth::login(
        &peers.auth,
        Path::new("migate"),
        Path::new("/data"),
        &mut output,
    ));
    assert!(result.is_ok(), "{result:?}");
    let output = String::from_utf8(output).unwrap();
    assert!(
        output.contains("device HTTP")
            && output.contains("cloud MQTT")
            && output.contains("gateway certificate")
    );
    assert!(
        output.contains("Scan this QR code")
            && output.contains("seconds")
            && output.contains("Waiting for scan")
    );
    assert!(output.contains('█'));
    assert!(
        !output.contains("Paste")
            && !output.contains("homeassistant.local")
            && !output.contains("ticket=")
    );
}

#[test]
fn terminal_login_failures_report_each_stage_with_one_recovery_hint() {
    for (failure, stage) in [
        (Failure::Verification, "HA OAuth authorization"),
        (Failure::QrVerification, "QR request"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let mut previous = previous_record();
        if failure == Failure::QrVerification {
            previous.recovery.stop = None;
            previous.ha.stopped = false;
        }
        store.xiaomi().fixture_replace(&previous).unwrap();
        let peers = make_peers(&store, failure);
        assert!(peers.auth.local_status().unwrap().login_failure.is_none());
        let mut output = Vec::new();
        let result = block_on(crate::terminal::auth::login(
            &peers.auth,
            Path::new("migate"),
            Path::new("/data"),
            &mut output,
        ));
        assert!(result.is_err(), "{stage}");
        let output = String::from_utf8(output).unwrap();
        assert!(
            output.contains(&format!("Login failed during {stage}:")),
            "{output}"
        );
        assert!(
            output.contains("Complete the requested verification at https://www.mi.com/"),
            "{output}"
        );
        assert!(
            output.contains("Sign in with:\n  'migate' --data-dir '/data' auth login"),
            "{output}"
        );
        assert_eq!(
            output
                .matches("Complete the requested verification")
                .count(),
            1,
            "{output}"
        );
        if failure == Failure::QrVerification {
            assert!(!output.contains("Scan this QR code"), "{output}");
        }
        assert!(!output.contains("verify-secret"), "{output}");
        assert_eq!(store.xiaomi().load().unwrap(), Some(previous));
    }
}

#[test]
fn terminal_reports_qr_render_failure_with_existing_status() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let previous = previous_record();
    store.xiaomi().fixture_replace(&previous).unwrap();
    let peers = make_peers(&store, Failure::LongQr);
    let mut output = Vec::new();
    let result = block_on(crate::terminal::auth::login(
        &peers.auth,
        Path::new("migate"),
        Path::new("/data"),
        &mut output,
    ));
    assert!(result.is_err());
    let output = String::from_utf8(output).unwrap();
    assert!(
        output.contains("Login failed during QR display"),
        "{output}"
    );
    assert!(output.contains("Xiaomi account: uid (cn)."), "{output}");
    assert!(!output.contains("ticket="));
    assert_eq!(store.xiaomi().load().unwrap(), Some(previous));
}

#[test]
fn terminal_identifies_failed_credential_commit_and_shows_old_status() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let previous = previous_record();
    store.xiaomi().fixture_replace(&previous).unwrap();
    Connection::open(store.path()).unwrap().execute_batch(
        "CREATE TRIGGER fail_login BEFORE INSERT ON xiaomi_mijia_session BEGIN SELECT RAISE(ABORT, 'test failure'); END;"
    ).unwrap();
    let peers = make_peers(&store, Failure::None);
    let mut output = Vec::new();
    let result = block_on(crate::terminal::auth::login(
        &peers.auth,
        Path::new("migate"),
        Path::new("/data"),
        &mut output,
    ));
    assert!(result.is_err());
    let output = String::from_utf8(output).unwrap();
    assert!(
        output.contains("Login failed during credential commit"),
        "{output}"
    );
    assert!(output.contains("Xiaomi account: uid (cn)."), "{output}");
    assert_eq!(store.xiaomi().load().unwrap(), Some(previous));
}

fn terminal_socket_pair() -> (Async<UnixStream>, Async<UnixStream>) {
    let (server, client) = UnixStream::pair().unwrap();
    (Async::new(server).unwrap(), Async::new(client).unwrap())
}

async fn foreground_login(
    online: &OnlineAuth<'_>,
    socket: &Async<UnixStream>,
    on_credentials: impl Fn(),
) {
    let mut output = Output::terminal(socket);
    read_foreground(BufReader::new(socket), async |words, session| {
        assert_eq!(words, ["auth", "login"]);
        online
            .execute(
                AuthCommand::Login,
                session,
                &mut output,
                &on_credentials,
                async || panic!("unexpected logout"),
            )
            .await?;
        Ok(())
    })
    .await
    .unwrap();
}

async fn wait_for_qr(socket: &Async<UnixStream>) -> String {
    let mut input = BufReader::new(socket);
    let mut output = String::new();
    let mut line = String::new();
    loop {
        assert_ne!(input.read_line(&mut line).await.unwrap(), 0);
        output.push_str(&line);
        if line.ends_with("Waiting for scan confirmation...\n") {
            return output;
        }
        line.clear();
    }
}

fn run_online_test(test: impl Future<Output = ()>) {
    block_on(future::or(test, async {
        Timer::after(Duration::from_secs(3)).await;
        panic!("online authentication test did not complete");
    }));
}

#[test]
fn foreground_eof_cancels_online_login_before_commit_and_other_sessions_get_busy() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let previous = previous_record();
    store.xiaomi().fixture_replace(&previous).unwrap();
    let peers = make_peers(&store, Failure::TokenPending);
    let online = OnlineAuth::new(&peers.auth, Path::new("migate"), directory.path());
    let (server, client) = terminal_socket_pair();
    let login = foreground_login(&online, &server, || panic!("cancelled login committed"));

    let concurrent = async {
        (&client).write_all(b"auth login\n").await.unwrap();
        let output = wait_for_qr(&client).await;
        assert!(output.contains("Scan this QR code"), "{output}");
        wait_for_test_signal(peers.token_started.clone()).await;

        let session = Session::default();
        let mut busy = Vec::new();
        for command in [AuthCommand::Login, AuthCommand::Check, AuthCommand::Logout] {
            let ok = online
                .execute(
                    command,
                    &session,
                    &mut Output::terminal(&mut busy),
                    || panic!("the other session changed credentials"),
                    async || panic!("the other session logged out"),
                )
                .await
                .unwrap();
            assert!(!ok);
        }
        let busy = String::from_utf8(busy).unwrap();
        assert_eq!(
            busy,
            "The auth login command is in progress. Please retry shortly.\n".repeat(3)
        );
        client.get_ref().shutdown(Shutdown::Write).unwrap();
    };
    run_online_test(async {
        future::zip(login, concurrent).await;
        peers.token_release.send(()).unwrap();
        assert_eq!(store.xiaomi().load().unwrap(), Some(previous));

        let mut output = Vec::new();
        assert!(
            online
                .execute(
                    AuthCommand::Logout,
                    &Session::default(),
                    &mut Output::terminal(&mut output),
                    || panic!("unexpected check or login"),
                    async || Ok(()),
                )
                .await
                .unwrap()
        );
        assert_eq!(output, b"Xiaomi credentials removed.\n");
    });
    assert!(store.xiaomi().load().unwrap().is_none());
}

#[test]
fn online_login_notifies_at_commit_and_continues_certificate_work_after_eof() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let peers = make_peers(&store, Failure::CertificatePending);
    let online = OnlineAuth::new(&peers.auth, Path::new("migate"), directory.path());
    let observed = Cell::new(0);
    let (server, client) = terminal_socket_pair();
    let on_credentials = || {
        let record = store.xiaomi().load().unwrap().unwrap();
        assert_eq!(record.mijia.service_token(), "service-new");
        assert_eq!(record.ha.authorization.tokens.access_token, "access-new");
        match observed.get() {
            0 => {
                assert!(!peers.certificate_started.load(Ordering::SeqCst));
                assert!(record.hub.is_none());
            }
            1 => assert!(record.hub.unwrap().certificate_pem.is_some()),
            _ => panic!("unexpected additional credential change"),
        }
        observed.set(observed.get() + 1);
    };
    run_online_test(async {
        let mut login = pin!(foreground_login(&online, &server, on_credentials));
        future::or(
            async {
                login.as_mut().await;
                panic!("login finished while the certificate response was held");
            },
            async {
                (&client).write_all(b"auth login\n").await.unwrap();
                wait_for_qr(&client).await;
                wait_for_test_signal(peers.certificate_started.clone()).await;
            },
        )
        .await;

        assert_eq!(observed.get(), 1);
        client.get_ref().shutdown(Shutdown::Write).unwrap();
        assert!(future::poll_once(login.as_mut()).await.is_none());
        peers.certificate_release.send(()).unwrap();
        login.await;
    });
    assert_eq!(observed.get(), 2);
    let saved = store.xiaomi().load().unwrap().unwrap();
    assert_eq!(saved.ha.authorization.tokens.access_token, "access-new");
    assert!(saved.hub.unwrap().certificate_pem.is_some());
}

#[test]
fn online_logout_keeps_a_failed_transaction_and_waits_for_stop_after_commit() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let previous = previous_record();
    store.xiaomi().fixture_replace(&previous).unwrap();
    let auth = AuthService::new(store.xiaomi(), CloudClient::new().unwrap()).unwrap();
    let online = OnlineAuth::new(&auth, Path::new("migate"), directory.path());
    let db = Connection::open(store.path()).unwrap();
    db.execute_batch("CREATE TRIGGER fail_logout BEFORE DELETE ON xiaomi_account BEGIN SELECT RAISE(ABORT, 'test cleanup failure'); END;").unwrap();
    let session = Session::default();
    let mut output = Vec::new();
    let ok = block_on(online.execute(
        AuthCommand::Logout,
        &session,
        &mut Output::terminal(&mut output),
        || panic!("unexpected check or login"),
        async || panic!("failed logout triggered cleanup"),
    ))
    .unwrap();
    assert!(!ok);
    assert!(
        String::from_utf8(output)
            .unwrap()
            .contains("Failed to delete Xiaomi credentials")
    );
    assert_eq!(store.xiaomi().load().unwrap(), Some(previous));
    db.execute_batch("DROP TRIGGER fail_logout;").unwrap();

    let (stopped, wait_for_stop) = flume::bounded(1);
    let mut output = Vec::new();
    run_online_test(async {
        let mut output = Output::terminal(&mut output);
        let mut logout = pin!(online.execute(
            AuthCommand::Logout,
            &session,
            &mut output,
            || panic!("unexpected check or login"),
            async || {
                assert!(store.xiaomi().load().unwrap().is_none());
                wait_for_stop.recv_async().await?;
                Ok(())
            },
        ));
        assert!(future::poll_once(logout.as_mut()).await.is_none());
        stopped.send(()).unwrap();
        assert!(logout.await.unwrap());
    });
    assert_eq!(output, b"Xiaomi credentials removed.\n");
    assert!(store.xiaomi().load().unwrap().is_none());
}
