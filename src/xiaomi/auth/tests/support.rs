use super::*;
use crate::storage::XiaomiStore;

pub(super) const NOW: i64 = 2_000_000_000;
pub(super) const UUID: &str = "550e8400-e29b-41d4-a716-446655440000";

pub(super) fn cached_bridge_device(uid: &str) -> crate::storage::CachedPhysicalDevice {
    crate::storage::CachedPhysicalDevice {
        uid: uid.into(),
        home_id: "local-home".into(),
        group_id: crate::xiaomi::cloud::group_id(uid, "local-home"),
        model: "vendor.plug".into(),
        instance_type: "urn:miot-spec-v2:device:outlet:0000A002:vendor-v1:1".into(),
        device: crate::xiaomi::capability::ParsedDevice {
            key: crate::device::DeviceKey::new("xiaomi", "target"),
            functions: vec![crate::xiaomi::capability::ParsedFunction {
                description: crate::device::FunctionDescription {
                    id: "main".into(),
                    label: "主电源".into(),
                    category: crate::device::Category::Outlet,
                    capabilities: vec![crate::device::Capability::OnOff],
                },
                power: crate::xiaomi::capability::PropertyBinding { siid: 2, piid: 1 },
            }],
            refresh_trigger: None,
        },
    }
}

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

pub(super) fn devices_body() -> &'static str {
    r#"{"code":0,"result":{"list":[]}}"#
}

pub(super) fn certificate_response(
    request: &crate::xiaomi::test_support::ReceivedRequest,
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

pub(super) fn service(
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

pub(super) fn callback(attempt: &LoginAttempt) -> String {
    format!(
        "{}?code=code&state={}",
        attempt.authorization().redirect_uri(),
        attempt.authorization().state()
    )
}

pub(super) async fn wait_for_signal(signal: Arc<AtomicBool>) {
    while !signal.load(Ordering::SeqCst) {
        async_io::Timer::after(Duration::from_millis(1)).await;
    }
}

impl AuthService {
    pub(crate) fn with_clock(store: XiaomiStore, cloud: CloudClient, now: fn() -> i64) -> Self {
        Self { store, cloud, now }
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
            authentication,
            certificate,
            certificate_update,
            completed_at,
        }
    }
}
