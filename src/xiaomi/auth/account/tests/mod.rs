use super::*;
use crate::xiaomi::{
    cloud::CLIENT_ID,
    test_support::{MockResponse, ReceivedRequest, dynamic_mock_server, mock_server},
};
use futures_lite::future::block_on;
use serde_json::json;
use std::{collections::HashMap, sync::mpsc::Receiver, time::Duration};

const DEVICE_ID: &str = "stable-device-id";
const UA: &str = "migate-test-agent";
const UID: &str = "12345";
const SECURITY: &str = "c2VjdXJpdHk=";
const PASS: &str = "pass-secret";
const SERVICE: &str = "service-secret";

fn identity() -> AccountClientIdentity {
    AccountClientIdentity::new(DEVICE_ID.into(), UA.into()).unwrap()
}
fn credentials() -> AccountCredentials {
    AccountCredentials::new(UID.into(), "cuser-secret".into(), PASS.into(), identity()).unwrap()
}
fn qr_body() -> String {
    json!({"loginUrl":"https://c3.account.xiaomi.com/longPolling/scan?ticket=qr-secret", "lp":"https://c3.lp.account.xiaomi.com/longPolling/login?ticket=poll-secret", "qr":"https://c3.account.xiaomi.com/longPolling/qr?ticket=qr-secret", "timeout":60}).to_string()
}
fn poll_body(uid: &str) -> String {
    format!(
        "&&&START&&&{}",
        json!({"code":0,"userId":uid,"cUserId":"cuser-secret","passToken":PASS,"ssecurity":SECURITY,"location":"https://sts.api.io.mi.com/sts?ticket=sts-secret"})
    )
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
fn query(target: &str) -> HashMap<String, String> {
    Url::parse(&format!("http://test{target}"))
        .unwrap()
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}
fn cookie(request: &ReceivedRequest) -> &str {
    request
        .headers
        .lines()
        .find(|line| line.to_ascii_lowercase().starts_with("cookie:"))
        .unwrap_or("")
}
fn assert_user_agent(request: &ReceivedRequest) {
    assert!(
        request
            .headers
            .lines()
            .any(|line| line.eq_ignore_ascii_case(&format!("user-agent: {UA}")))
    );
}

#[derive(Clone, Copy)]
enum Mutation {
    None,
    Uid,
    Client,
    Redirect,
    FollowupDevice,
    FollowupState,
    OpaqueDevice,
    CallbackState,
    ContextRejected,
    ContextVerification,
    PostRejected,
    PostVerification,
    CallbackRejected,
    CallbackRejectedBadState,
}

fn happy_peer(
    attempt: &AuthorizationAttempt,
    mutation: Mutation,
) -> (String, Receiver<ReceivedRequest>) {
    let original = Url::parse(attempt.authorization_url()).unwrap();
    let mut followup = original.clone();
    if matches!(mutation, Mutation::FollowupDevice | Mutation::FollowupState) {
        let pairs = original
            .query_pairs()
            .map(|(key, value)| {
                let v = if matches!(mutation, Mutation::FollowupDevice) && key == "device_id" {
                    "ha.wrong".into()
                } else if matches!(mutation, Mutation::FollowupState) && key == "state" {
                    "wrong-state".into()
                } else {
                    value.into_owned()
                };
                (key.into_owned(), v)
            })
            .collect::<Vec<_>>();
        followup.set_query(None);
        followup.query_pairs_mut().extend_pairs(pairs);
    }
    let redirect_uri = attempt.redirect_uri().to_owned();
    let state = if matches!(
        mutation,
        Mutation::CallbackState | Mutation::CallbackRejectedBadState
    ) {
        "wrong-state".to_owned()
    } else {
        attempt.state().to_owned()
    };
    let mut step = 0;
    dynamic_mock_server(16, move |request| {
        let current = step;
        step += 1;
        match current {
            0 => response(200, "Set-Cookie: qrSession=qr-cookie; Domain=account.xiaomi.com; Path=/\r\n", &qr_body()),
            1 => MockResponse::json(200, &poll_body(UID)),
            2 => response(200, "Set-Cookie: serviceToken=service-secret; Path=/\r\n", ""),
            3 => redirect("/pass/serviceLogin?sid=oauth"),
            4 => redirect("/sts/oauth?ticket=oauth-ticket"),
            5 => redirect("/fe/service/oauth2/authorize"),
            6 => response(200, "Content-Type: text/html\r\n", "consent SPA"),
            7 if matches!(mutation, Mutation::ContextRejected) => MockResponse::json(200, &json!({"code":96012}).to_string()),
            7 if matches!(mutation, Mutation::ContextVerification) => MockResponse::json(200, &json!({"notificationUrl":"https://account.xiaomi.com/pass/notification?ticket=secret"}).to_string()),
            7 => MockResponse::json(200, &json!({"code":0,"data":{
                "user":{"userId":if matches!(mutation, Mutation::Uid) {"another-user"} else {UID}},
                "client_id":if matches!(mutation, Mutation::Client) {"999".to_owned()} else {CLIENT_ID.to_string()},
                "redirect_uri":if matches!(mutation, Mutation::Redirect) {"http://homeassistant.local:8123/api/webhook/wrong"} else {&redirect_uri},
                "pt":"pt-secret", "device_id":if matches!(mutation, Mutation::OpaqueDevice) {"invalid"} else {"0123456789abcdef0123456789abcdef"},
                "followup":followup.as_str(),"scope_id":"1 2","_ssign":"ssign-secret"}}).to_string()),
            8 if matches!(mutation, Mutation::PostRejected) => MockResponse::json(200, &json!({"code":96012}).to_string()),
            8 if matches!(mutation, Mutation::PostVerification) => MockResponse::json(200, &json!({"code":0,"data":{"notificationUrl":"https://account.xiaomi.com/pass/notification?ticket=secret"}}).to_string()),
            8 => MockResponse::json(200, &json!({"code":0,"data":{"followup":"https://account.xiaomi.com/sts/oauth?ticket=grant-secret"}}).to_string()),
            9 if matches!(mutation, Mutation::CallbackRejected | Mutation::CallbackRejectedBadState) => {
                redirect(&format!("{redirect_uri}?error=access_denied&state={state}"))
            }
            9 => redirect(&format!("{redirect_uri}?code=code-secret&state={state}")),
            _ => panic!("unexpected request: {}", request.target),
        }
    })
}

#[test]
fn qr_login_starts_a_fresh_account_request() {
    let (base, requests) = mock_server(vec![MockResponse::json(200, &qr_body())]);
    let client = AccountClient::for_test(&base, Duration::from_secs(1));
    let attempt = block_on(client.start_qr(identity())).unwrap();
    assert!(attempt.login_url().contains("qr-secret"));
    assert!(attempt.remaining() <= Duration::from_secs(60));
    let request = requests.recv().unwrap();
    assert!(request.target.starts_with("/longPolling/loginUrl?"));
    let query = query(&request.target);
    assert_eq!(query["qs"], "%3Fsid%3Dxiaomiio%26_json%3Dtrue");
    assert_eq!(query["sid"], "xiaomiio");
    assert_eq!(query["callback"], "https://sts.api.io.mi.com/sts");
    assert_eq!(query["_qrsize"], "400");
    assert_eq!(query["_locale"], "zh_CN");
    assert_user_agent(&request);
    assert!(cookie(&request).contains("deviceId=stable-device-id"));
    assert!(!format!("{attempt:?}").contains("secret"));
}

#[test]
fn qr_sts_and_ha_grant_use_one_isolated_account_session() {
    let authorization = AuthorizationAttempt::new(None).unwrap();
    let (base, requests) = happy_peer(&authorization, Mutation::None);
    let client = AccountClient::for_test(&base, Duration::from_secs(1));
    let qr = block_on(client.start_qr(identity())).unwrap();
    let account = block_on(qr.poll(Some(UID))).unwrap();
    assert_eq!(account.session().service_token(), SERVICE);
    assert_eq!(account.session().ssecurity(), SECURITY);
    assert_eq!(account.session().user_agent(), UA);
    assert_eq!(account.credentials().pass_token(), PASS);
    let grant = block_on(account.authorize(&authorization)).unwrap();
    assert_eq!(grant.code(), "code-secret");
    for secret in [
        PASS,
        SERVICE,
        SECURITY,
        "cuser-secret",
        "code-secret",
        "ssign-secret",
        "qr-secret",
    ] {
        assert!(!format!("{grant:?}").contains(secret));
    }
    let requests: Vec<_> = requests.try_iter().collect();
    assert_eq!(requests.len(), 10);
    assert!(requests[1].target.starts_with("/longPolling/login?"));
    assert!(cookie(&requests[1]).contains("qrSession=qr-cookie"));
    assert!(requests[2].target.starts_with("/sts?"));
    for secret in [PASS, "cuser-secret", "qr-cookie"] {
        assert!(!cookie(&requests[2]).contains(secret));
    }
    let auth_query = query(&requests[3].target);
    assert_eq!(auth_query["skip_confirm"], "false");
    assert_eq!(auth_query["device_id"], authorization.device_id());
    for request in &requests[3..] {
        assert!(cookie(request).contains("passToken=pass-secret"));
        assert!(!cookie(request).contains("service-secret"));
        assert_user_agent(request);
    }
    assert_eq!(query(&requests[7].target)["_json"], "true");
    assert!(
        requests[8]
            .headers
            .starts_with("POST /oauth2/userAuthorization ")
    );
    let form: HashMap<_, _> = url::form_urlencoded::parse(requests[8].body.as_bytes())
        .into_owned()
        .collect();
    assert_eq!(form.len(), 8);
    assert_eq!(form["device_id"], "0123456789abcdef0123456789abcdef");
    assert_eq!(form["pt"], "pt-secret");
    assert_eq!(form["client_id"], CLIENT_ID.to_string());
    assert_eq!(form["redirect_uri"], authorization.redirect_uri());
    assert_eq!(form["followup"], authorization.authorization_url());
    assert_eq!(form["_ssign"], "ssign-secret");
    assert_eq!(form["scope_id"], "1 2");
    assert_eq!(form["_json"], "true");
    assert!(requests[9].target.starts_with("/sts/oauth?"));
}

#[test]
fn consent_and_callback_must_match_the_current_login() {
    for mutation in [
        Mutation::Uid,
        Mutation::Client,
        Mutation::Redirect,
        Mutation::FollowupDevice,
        Mutation::FollowupState,
        Mutation::OpaqueDevice,
        Mutation::CallbackState,
    ] {
        let authorization = AuthorizationAttempt::new(None).unwrap();
        let (base, requests) = happy_peer(&authorization, mutation);
        let client = AccountClient::for_test(&base, Duration::from_secs(1));
        let qr = block_on(client.start_qr(identity())).unwrap();
        let account = block_on(qr.poll(Some(UID))).unwrap();
        let error = block_on(account.authorize(&authorization)).unwrap_err();
        assert_eq!(error.stage(), AccountStage::HaAuthorization);
        if matches!(mutation, Mutation::Uid) {
            assert_eq!(error.kind(), AccountErrorKind::IdentityMismatch);
        } else {
            assert_eq!(error.kind(), AccountErrorKind::Protocol);
        }
        let requests: Vec<_> = requests.try_iter().collect();
        assert_eq!(
            requests.len(),
            if matches!(mutation, Mutation::CallbackState) {
                10
            } else {
                8
            }
        );
        assert!(!format!("{error:?} {error}").contains("secret"));
    }
}

#[test]
fn recovery_is_one_round_and_preserves_the_new_security_pair() {
    let (base, requests) = mock_server(vec![
        MockResponse::json(200, &json!({"code":0,"userId":UID,"cUserId":"new-cuser","passToken":"new-pass","ssecurity":"bmV3LXNlY3VyaXR5","location":"https://sts.api.io.mi.com/sts?ticket=restore-secret"}).to_string()),
        response(200, "Set-Cookie: serviceToken=new-service; Path=/\r\n", ""),
    ]);
    let client = AccountClient::for_test(&base, Duration::from_secs(1));
    let restored = block_on(client.restore(&credentials())).unwrap();
    assert_eq!(restored.session().service_token(), "new-service");
    assert_eq!(restored.session().ssecurity(), "bmV3LXNlY3VyaXR5");
    assert_eq!(restored.session().user_agent(), UA);
    assert_eq!(restored.credentials().pass_token(), "new-pass");
    assert_eq!(restored.credentials().c_user_id(), "new-cuser");
    let requests: Vec<_> = requests.try_iter().collect();
    assert_eq!(requests.len(), 2);
    let params = query(&requests[0].target);
    assert_eq!(params["_json"], "true");
    assert_eq!(params["sid"], "xiaomiio");
    for expected in [
        "deviceId=stable-device-id",
        "userId=12345",
        "cUserId=cuser-secret",
        "passToken=pass-secret",
    ] {
        assert!(cookie(&requests[0]).contains(expected));
    }
    assert_user_agent(&requests[0]);
    assert!(!cookie(&requests[1]).contains(PASS));
    assert!(!format!("{restored:?}").contains("new-service"));
    assert!(!format!("{restored:?}").contains("new-pass"));
}

#[test]
fn qr_and_recovery_uid_mismatch_stop_before_sts() {
    let (base, requests) = mock_server(vec![
        MockResponse::json(200, &qr_body()),
        MockResponse::json(200, &poll_body("another-user")),
    ]);
    let client = AccountClient::for_test(&base, Duration::from_secs(1));
    let qr = block_on(client.start_qr(identity())).unwrap();
    let error = block_on(qr.poll(Some(UID))).unwrap_err();
    assert_eq!(error.kind(), AccountErrorKind::IdentityMismatch);
    assert_eq!(requests.try_iter().count(), 2);

    let (base, requests) = mock_server(vec![MockResponse::json(200, &json!({"code":0,"userId":"other", "ssecurity":SECURITY,"location":"https://sts.api.io.mi.com/sts"}).to_string())]);
    let client = AccountClient::for_test(&base, Duration::from_secs(1));
    let error = block_on(client.restore(&credentials())).unwrap_err();
    assert_eq!(error.kind(), AccountErrorKind::IdentityMismatch);
    assert_eq!(requests.try_iter().count(), 1);
}

#[test]
fn rejected_verification_and_temporary_failures_are_typed_by_stage() {
    for (body, expected) in [
        (json!({"code":70016}), AccountErrorKind::Rejected),
        (
            json!({"code":87001}),
            AccountErrorKind::VerificationRequired,
        ),
        (
            json!({"code":70016,"notificationUrl":"https://account.xiaomi.com/pass/notification?secret=1"}),
            AccountErrorKind::VerificationRequired,
        ),
        (json!({"code":99999}), AccountErrorKind::Protocol),
    ] {
        let (base, requests) = mock_server(vec![MockResponse::json(200, &body.to_string())]);
        let client = AccountClient::for_test(&base, Duration::from_secs(1));
        let error = block_on(client.restore(&credentials())).unwrap_err();
        assert_eq!(error.stage(), AccountStage::ServiceLogin);
        assert_eq!(error.kind(), expected);
        assert_eq!(requests.try_iter().count(), 1);
        assert!(!format!("{error:?} {error}").contains("secret"));
    }
    for (status_code, expected) in [
        (403, AccountErrorKind::Rejected),
        (408, AccountErrorKind::Timeout),
        (503, AccountErrorKind::Network),
    ] {
        let (base, requests) = mock_server(vec![MockResponse::json(status_code, "{}")]);
        let client = AccountClient::for_test(&base, Duration::from_secs(1));
        let error = block_on(client.restore(&credentials())).unwrap_err();
        assert_eq!(error.kind(), expected);
        assert_eq!(requests.try_iter().count(), 1);
    }
    for (body, expected) in [
        (json!({"code":70016}), AccountErrorKind::Rejected),
        (
            json!({"captchaUrl":"https://account.xiaomi.com/captcha?secret=1"}),
            AccountErrorKind::VerificationRequired,
        ),
    ] {
        let (base, requests) = mock_server(vec![
            MockResponse::json(200, &qr_body()),
            MockResponse::json(200, &body.to_string()),
        ]);
        let client = AccountClient::for_test(&base, Duration::from_secs(1));
        let qr = block_on(client.start_qr(identity())).unwrap();
        let error = block_on(qr.poll(Some(UID))).unwrap_err();
        assert_eq!(
            (error.stage(), error.kind()),
            (AccountStage::QrPolling, expected)
        );
        assert_eq!(requests.try_iter().count(), 2);
    }
    for (body, expected) in [
        (json!({"code":70016}), AccountErrorKind::Rejected),
        (
            json!({"notificationUrl":"https://account.xiaomi.com/pass/notification?secret=1"}),
            AccountErrorKind::VerificationRequired,
        ),
    ] {
        let (base, requests) = mock_server(vec![
            MockResponse::json(200, &qr_body()),
            MockResponse::json(200, &poll_body(UID)),
            MockResponse::json(200, &body.to_string()),
        ]);
        let client = AccountClient::for_test(&base, Duration::from_secs(1));
        let qr = block_on(client.start_qr(identity())).unwrap();
        let error = block_on(qr.poll(Some(UID))).unwrap_err();
        assert_eq!((error.stage(), error.kind()), (AccountStage::Sts, expected));
        assert_eq!(requests.try_iter().count(), 3);
    }
}

#[test]
fn qr_expiry_and_cancellation_do_not_restart_or_share_cookies() {
    let (base, requests) = mock_server(vec![
        response(
            200,
            "Set-Cookie: qrSession=one-attempt-only; Domain=account.xiaomi.com; Path=/\r\n",
            &qr_body(),
        ),
        MockResponse::json(200, &qr_body()),
    ]);
    let client = AccountClient::for_test(&base, Duration::from_secs(1));
    let mut first = block_on(client.start_qr(identity())).unwrap();
    first.expires_at = Instant::now();
    let error = block_on(first.poll(None)).unwrap_err();
    assert_eq!(
        (error.stage(), error.kind()),
        (AccountStage::QrPolling, AccountErrorKind::Timeout)
    );
    let second = block_on(client.start_qr(identity())).unwrap();
    drop(second);
    let requests: Vec<_> = requests.try_iter().collect();
    assert_eq!(requests.len(), 2);
    assert!(!cookie(&requests[1]).contains("one-attempt-only"));
}

#[test]
fn poll_request_timeouts_retry_within_the_same_qr_deadline() {
    let mut step = 0;
    let (base, requests) = dynamic_mock_server(4, move |_| {
        let current = step;
        step += 1;
        match current {
            0 => MockResponse::json(200, &qr_body()),
            1 => MockResponse::json(200, &poll_body(UID)).delayed(Duration::from_millis(150)),
            2 => MockResponse::json(200, &poll_body(UID)),
            _ => response(
                200,
                "Set-Cookie: serviceToken=service-secret; Path=/\r\n",
                "",
            ),
        }
    });
    let client = AccountClient::for_test(&base, Duration::from_millis(60));
    let qr = block_on(client.start_qr(identity())).unwrap();
    let account = block_on(qr.poll(Some(UID))).unwrap();
    assert_eq!(account.session().service_token(), SERVICE);
    let requests: Vec<_> = requests.try_iter().collect();
    assert_eq!(requests.len(), 4);
    assert!(requests[1].target.starts_with("/longPolling/login?"));
    assert!(requests[2].target.starts_with("/longPolling/login?"));
}

#[test]
fn oauth_consent_and_submission_report_rejection_and_verification() {
    for (mutation, expected, count) in [
        (Mutation::ContextRejected, AccountErrorKind::Rejected, 8),
        (
            Mutation::ContextVerification,
            AccountErrorKind::VerificationRequired,
            8,
        ),
        (Mutation::PostRejected, AccountErrorKind::Rejected, 9),
        (
            Mutation::PostVerification,
            AccountErrorKind::VerificationRequired,
            9,
        ),
    ] {
        let authorization = AuthorizationAttempt::new(None).unwrap();
        let (base, requests) = happy_peer(&authorization, mutation);
        let client = AccountClient::for_test(&base, Duration::from_secs(1));
        let qr = block_on(client.start_qr(identity())).unwrap();
        let account = block_on(qr.poll(Some(UID))).unwrap();
        let error = block_on(account.authorize(&authorization)).unwrap_err();
        assert_eq!(
            (error.stage(), error.kind()),
            (AccountStage::HaAuthorization, expected)
        );
        assert_eq!(requests.try_iter().count(), count);
        assert!(!format!("{error:?} {error}").contains("secret"));
    }
}

#[test]
fn untrusted_urls_never_receive_a_request_or_account_cookies() {
    let mut metadata: Value = serde_json::from_str(&qr_body()).unwrap();
    metadata["lp"] =
        json!("https://c3.lp.account.xiaomi.com.evil.invalid/longPolling/login?ticket=secret");
    let (base, requests) = mock_server(vec![MockResponse::json(200, &metadata.to_string())]);
    let client = AccountClient::for_test(&base, Duration::from_secs(1));
    assert_eq!(
        block_on(client.start_qr(identity())).unwrap_err().kind(),
        AccountErrorKind::Protocol
    );
    assert_eq!(requests.try_iter().count(), 1);

    for location in [
        "http://sts.api.io.mi.com/sts?ticket=secret",
        "https://sts.api.io.mi.com:444/sts?ticket=secret",
        "https://sts.api.io.mi.com.evil.invalid/sts?ticket=secret",
        "https://api.io.mi.com/sts?ticket=secret",
        "https://sts.api.io.mi.com/other?ticket=secret",
    ] {
        let mut body: Value =
            serde_json::from_str(poll_body(UID).trim_start_matches("&&&START&&&")).unwrap();
        body["location"] = json!(location);
        let (base, requests) = mock_server(vec![
            MockResponse::json(200, &qr_body()),
            MockResponse::json(200, &body.to_string()),
        ]);
        let client = AccountClient::for_test(&base, Duration::from_secs(1));
        let qr = block_on(client.start_qr(identity())).unwrap();
        let error = block_on(qr.poll(Some(UID))).unwrap_err();
        assert_eq!(
            (error.stage(), error.kind()),
            (AccountStage::Sts, AccountErrorKind::Protocol)
        );
        assert_eq!(requests.try_iter().count(), 2);
    }
    let (base, requests) = mock_server(vec![
        MockResponse::json(200, &qr_body()),
        MockResponse::json(200, &poll_body(UID)),
        response(
            200,
            "Set-Cookie: serviceToken=service-secret; Path=/\r\n",
            "",
        ),
        redirect("https://evil.invalid/steal?secret=redirect"),
    ]);
    let client = AccountClient::for_test(&base, Duration::from_secs(1));
    let qr = block_on(client.start_qr(identity())).unwrap();
    let account = block_on(qr.poll(Some(UID))).unwrap();
    let error = block_on(account.authorize(&AuthorizationAttempt::new(None).unwrap())).unwrap_err();
    assert_eq!(
        (error.stage(), error.kind()),
        (AccountStage::HaAuthorization, AccountErrorKind::Protocol)
    );
    assert_eq!(requests.try_iter().count(), 4);
}

#[test]
fn redirect_loops_are_bounded_and_verification_redirects_are_not_followed() {
    for (location, expected, count) in [
        (
            "/pass/serviceLogin?sid=oauth",
            AccountErrorKind::Protocol,
            12,
        ),
        (
            "/pass/notification?ticket=verification-secret",
            AccountErrorKind::VerificationRequired,
            4,
        ),
    ] {
        let mut step = 0;
        let (base, requests) = dynamic_mock_server(20, move |_| {
            let current = step;
            step += 1;
            match current {
                0 => MockResponse::json(200, &qr_body()),
                1 => MockResponse::json(200, &poll_body(UID)),
                2 => response(
                    200,
                    "Set-Cookie: serviceToken=service-secret; Path=/\r\n",
                    "",
                ),
                _ => redirect(location),
            }
        });
        let client = AccountClient::for_test(&base, Duration::from_secs(1));
        let qr = block_on(client.start_qr(identity())).unwrap();
        let account = block_on(qr.poll(Some(UID))).unwrap();
        let error =
            block_on(account.authorize(&AuthorizationAttempt::new(None).unwrap())).unwrap_err();
        assert_eq!(
            (error.stage(), error.kind()),
            (AccountStage::HaAuthorization, expected)
        );
        assert_eq!(requests.try_iter().count(), count);
        assert!(!format!("{error:?} {error}").contains("verification-secret"));
    }
}

#[test]
fn incomplete_or_invalid_service_pairs_never_become_candidates() {
    let login = json!({"code":0,"userId":UID,"ssecurity":"not base64!","location":"https://sts.api.io.mi.com/sts"}).to_string();
    for (login, sts) in [
        (login, response(200, "Set-Cookie: serviceToken=service-secret; Path=/\r\n", "")),
        (json!({"code":0,"userId":UID,"ssecurity":SECURITY,"location":"https://sts.api.io.mi.com/sts"}).to_string(), MockResponse::json(200, "{}")),
    ] {
        let (base, requests) = mock_server(vec![MockResponse::json(200, &login), sts]);
        let client = AccountClient::for_test(&base, Duration::from_secs(1));
        let error = block_on(client.restore(&credentials())).unwrap_err();
        assert_eq!((error.stage(), error.kind()), (AccountStage::Sts, AccountErrorKind::Protocol));
        assert_eq!(requests.try_iter().count(), 2);
    }
}

#[test]
fn explicit_callback_rejection_requires_the_current_state() {
    for (mutation, expected) in [
        (Mutation::CallbackRejected, AccountErrorKind::Rejected),
        (
            Mutation::CallbackRejectedBadState,
            AccountErrorKind::Protocol,
        ),
    ] {
        let authorization = AuthorizationAttempt::new(None).unwrap();
        let (base, requests) = happy_peer(&authorization, mutation);
        let client = AccountClient::for_test(&base, Duration::from_secs(1));
        let qr = block_on(client.start_qr(identity())).unwrap();
        let account = block_on(qr.poll(Some(UID))).unwrap();
        let error = block_on(account.authorize(&authorization)).unwrap_err();
        assert_eq!(
            (error.stage(), error.kind()),
            (AccountStage::HaAuthorization, expected)
        );
        assert_eq!(requests.try_iter().count(), 10);
    }
}

#[test]
fn sts_error_cannot_be_overridden_by_a_service_cookie() {
    for (status_code, body, expected) in [
        (403, "{}", AccountErrorKind::Rejected),
        (500, "{}", AccountErrorKind::Network),
        (
            200,
            r#"{"notificationUrl":"https://account.xiaomi.com/pass/notification?secret=1"}"#,
            AccountErrorKind::VerificationRequired,
        ),
        (200, r#"{"code":70016}"#, AccountErrorKind::Rejected),
    ] {
        let login = json!({"code":0,"userId":UID,"ssecurity":SECURITY,"location":"https://sts.api.io.mi.com/sts"}).to_string();
        let (base, requests) = mock_server(vec![
            MockResponse::json(200, &login),
            response(
                status_code,
                "Set-Cookie: serviceToken=service-secret; Path=/\r\n",
                body,
            ),
        ]);
        let client = AccountClient::for_test(&base, Duration::from_secs(1));
        let error = block_on(client.restore(&credentials())).unwrap_err();
        assert_eq!((error.stage(), error.kind()), (AccountStage::Sts, expected));
        assert_eq!(requests.try_iter().count(), 2);
    }
}

#[test]
fn verification_redirects_are_typed_at_each_account_stage_without_following() {
    const VERIFICATION: &str = "https://account.xiaomi.com/pass/notification?ticket=secret";
    let (base, requests) = mock_server(vec![redirect(VERIFICATION)]);
    let client = AccountClient::for_test(&base, Duration::from_secs(1));
    let error = block_on(client.start_qr(identity())).unwrap_err();
    assert_eq!(
        (error.stage(), error.kind()),
        (
            AccountStage::QrRequest,
            AccountErrorKind::VerificationRequired
        )
    );
    assert_eq!(requests.try_iter().count(), 1);

    let (base, requests) = mock_server(vec![
        MockResponse::json(200, &qr_body()),
        redirect(VERIFICATION),
    ]);
    let client = AccountClient::for_test(&base, Duration::from_secs(1));
    let qr = block_on(client.start_qr(identity())).unwrap();
    let error = block_on(qr.poll(None)).unwrap_err();
    assert_eq!(
        (error.stage(), error.kind()),
        (
            AccountStage::QrPolling,
            AccountErrorKind::VerificationRequired
        )
    );
    assert_eq!(requests.try_iter().count(), 2);

    let (base, requests) = mock_server(vec![redirect(VERIFICATION)]);
    let client = AccountClient::for_test(&base, Duration::from_secs(1));
    let error = block_on(client.restore(&credentials())).unwrap_err();
    assert_eq!(
        (error.stage(), error.kind()),
        (
            AccountStage::ServiceLogin,
            AccountErrorKind::VerificationRequired
        )
    );
    assert_eq!(requests.try_iter().count(), 1);

    let (base, requests) = mock_server(vec![
        MockResponse::json(200, &qr_body()),
        MockResponse::json(200, &poll_body(UID)),
        redirect(VERIFICATION),
    ]);
    let client = AccountClient::for_test(&base, Duration::from_secs(1));
    let qr = block_on(client.start_qr(identity())).unwrap();
    let error = block_on(qr.poll(None)).unwrap_err();
    assert_eq!(
        (error.stage(), error.kind()),
        (AccountStage::Sts, AccountErrorKind::VerificationRequired)
    );
    assert_eq!(requests.try_iter().count(), 3);
}

#[test]
fn sts_errors_with_leading_json_whitespace_cannot_be_overridden_by_a_service_cookie() {
    let mut outcomes = Vec::new();
    for body in [
        " \n{\"code\":70016}",
        "&&&START&&& \n{\"notificationUrl\":\"https://account.xiaomi.com/pass/notification?ticket=secret\"}",
    ] {
        let login = json!({"code":0,"userId":UID,"ssecurity":SECURITY,"location":"https://sts.api.io.mi.com/sts"}).to_string();
        let (base, requests) = mock_server(vec![
            MockResponse::json(200, &login),
            response(
                200,
                "Set-Cookie: serviceToken=service-secret; Path=/\r\n",
                body,
            ),
        ]);
        let client = AccountClient::for_test(&base, Duration::from_secs(1));
        let outcome = block_on(client.restore(&credentials()));
        outcomes.push(outcome.err().map(|error| (error.stage(), error.kind())));
        assert_eq!(requests.try_iter().count(), 2);
    }
    assert_eq!(
        outcomes,
        [
            Some((AccountStage::Sts, AccountErrorKind::Rejected)),
            Some((AccountStage::Sts, AccountErrorKind::VerificationRequired)),
        ]
    );
}

#[test]
fn sts_success_with_a_service_cookie_preserves_empty_and_normal_bodies() {
    for body in [
        "",
        "ok",
        "\t\r\n {\"code\":0}",
        "&&&START&&& \n{\"code\":0}",
    ] {
        let login = json!({"code":0,"userId":UID,"ssecurity":SECURITY,"location":"https://sts.api.io.mi.com/sts"}).to_string();
        let (base, requests) = mock_server(vec![
            MockResponse::json(200, &login),
            response(
                200,
                "Set-Cookie: serviceToken=service-secret; Path=/\r\n",
                body,
            ),
        ]);
        let client = AccountClient::for_test(&base, Duration::from_secs(1));
        let restored = block_on(client.restore(&credentials())).unwrap();
        assert_eq!(restored.session().service_token(), SERVICE);
        assert_eq!(restored.session().ssecurity(), SECURITY);
        assert_eq!(requests.try_iter().count(), 2);
    }
}
