use super::*;
use crate::{storage::XiaomiStore, xiaomi::mijia::MijiaClient};

pub(super) const NOW: i64 = 2_000_000_000;
pub(super) const UUID: &str = "550e8400-e29b-41d4-a716-446655440000";

pub(super) fn now() -> i64 {
    NOW
}

pub(super) fn token_body(access: &str, refresh: &str) -> String {
    json!({"code":0,"result":{"access_token":access,"refresh_token":refresh,"expires_in":1000}})
        .to_string()
}

pub(super) fn home_body(uid: Option<&str>, dids: &[&str]) -> String {
    let homes = uid
        .map(|uid| vec![json!({"uid":uid,"dids":dids})])
        .unwrap_or_default();
    json!({"code":0,"result":{"homelist":homes}}).to_string()
}

pub(super) fn certificate_response(
    request: &crate::xiaomi::tests::support::ReceivedRequest,
) -> MockResponse {
    let body: Value = serde_json::from_str(&request.body).unwrap();
    let csr = STANDARD.decode(body["csr"].as_str().unwrap()).unwrap();
    let csr = String::from_utf8(csr).unwrap();
    let certificate = sign_csr(&csr, NOW - 60, NOW + 10 * 24 * 60 * 60).unwrap();
    MockResponse::json(
        200,
        &json!({"code":0,"result":{"cert":certificate}}).to_string(),
    )
}

pub(super) fn valid_record(
    uid: &str,
    access: &str,
    refresh_at: i64,
    not_after: i64,
) -> XiaomiRecord {
    let identity = ClientIdentity::generate(uid).unwrap();
    let certificate = sign_csr(&identity.csr_pem, NOW - 60, not_after).unwrap();
    XiaomiRecord {
        mijia: crate::xiaomi::mijia::MijiaSession::new(
            uid.into(),
            "service-old".into(),
            "c2VjdXJpdHk=".into(),
            "test-agent".into(),
        )
        .unwrap(),
        ha: crate::storage::HaClient {
            client_uuid: UUID.into(),
            authorization: crate::storage::HaAuthorization {
                redirect_uri:
                    "http://homeassistant.local:8123/api/webhook/0123456789abcdef0123456789abcdef"
                        .into(),
                tokens: TokenSet {
                    access_token: access.into(),
                    refresh_token: "refresh-old".into(),
                    expires_at: NOW + 1000,
                    refresh_at,
                },
            },
            stopped: false,
        },
        hub: Some(crate::storage::HubIdentity {
            virtual_did: identity.virtual_did,
            private_key_pem: identity.private_key_pem,
            certificate_pem: Some(certificate),
        }),
        ..crate::storage::tests::xiaomi_credentials(uid)
    }
}

pub(super) fn service(
    store: &Store,
    responses: Vec<MockResponse>,
) -> (
    AuthService,
    std::sync::mpsc::Receiver<crate::xiaomi::tests::support::ReceivedRequest>,
) {
    let (base, requests) = mock_server(responses);
    let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    (
        AuthService::with_clock(store.xiaomi(), cloud, now),
        requests,
    )
}

pub(super) async fn wait_for_signal(signal: Arc<AtomicBool>) {
    while !signal.load(Ordering::SeqCst) {
        async_io::Timer::after(Duration::from_millis(1)).await;
    }
}

impl AuthService {
    pub(crate) fn with_clock(store: XiaomiStore, cloud: CloudClient, now: fn() -> i64) -> Self {
        let (base, _) = dynamic_mock_server(20, |request| {
            let cookie = request
                .headers
                .lines()
                .find(|line| line.to_ascii_lowercase().starts_with("cookie:"))
                .unwrap();
            let uid = cookie
                .split("userId=")
                .nth(1)
                .unwrap()
                .split(';')
                .next()
                .unwrap();
            MockResponse::json(200, &json!({"code":0,"result":{"homelist":[{"uid":uid,"id":"1","dids":[],"roomlist":[]}],"has_more":false}}).to_string())
        });
        Self {
            store,
            cloud,
            account: AccountClient::for_test("http://127.0.0.1:9", Duration::from_secs(1)),
            mijia: crate::xiaomi::mijia::MijiaClient::for_test(&base, Duration::from_secs(1)),
            now,
        }
    }

    pub(in crate::xiaomi) fn for_adapter_test(
        store: XiaomiStore,
        cloud: CloudClient,
        base: &str,
    ) -> Self {
        Self {
            store,
            cloud,
            account: AccountClient::for_test(base, Duration::from_secs(1)),
            mijia: MijiaClient::for_test(base, Duration::from_secs(1)),
            now: || time::OffsetDateTime::now_utc().unix_timestamp(),
        }
    }
}

impl AuthReport {
    pub(crate) fn for_test(
        authentication: AuthenticationState,
        certificate: Option<CertificateValidity>,
        certificate_update: CertificateUpdate,
        completed_at: i64,
    ) -> Self {
        Self {
            account: (!matches!(authentication, AuthenticationState::NotSignedIn)).then(|| {
                AccountIdentity {
                    uid: "uid".into(),
                    region: "cn".into(),
                }
            }),
            recovery_stop: None,
            mijia: authentication.clone(),
            ha: authentication,
            certificate,
            certificate_update,
            login_failure: None,
            completed_at,
        }
    }
}
