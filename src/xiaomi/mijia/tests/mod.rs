use super::*;
use crate::xiaomi::{
    capability::PropertyBinding,
    cloud::{CloudError, CloudErrorKind},
    tests::support::{MockResponse, ReceivedRequest, dynamic_mock_server, mock_server},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures_lite::future::{self, block_on};
use hmac::{Hmac, KeyInit, Mac};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::HashMap, time::Duration};

mod catalog;
mod device_info;
mod directory;
mod light_groups;
mod properties;
mod requests;
mod support;

fn session() -> MijiaSession {
    MijiaSession::new(
        "123".into(),
        "service+token/secret".into(),
        "MDEyMzQ1Njc4OWFiY2RlZg==".into(),
        "Mijia-Test/1".into(),
    )
    .unwrap()
}

fn client(base: &str, timeout: Duration) -> MijiaClient {
    MijiaClient::for_test(base, timeout)
}

fn form(request: &ReceivedRequest) -> HashMap<String, String> {
    url::form_urlencoded::parse(request.body.as_bytes())
        .into_owned()
        .collect()
}

fn data(request: &ReceivedRequest) -> Value {
    serde_json::from_str(&form(request)["data"]).unwrap()
}

fn assert_signed(request: &ReceivedRequest) {
    let fields = form(request);
    assert_eq!(fields.len(), 3);
    let nonce = STANDARD.decode(&fields["_nonce"]).unwrap();
    assert_eq!(nonce.len(), 12);
    let signed_nonce = Sha256::digest([b"0123456789abcdef".as_slice(), nonce.as_slice()].concat());
    let signature_input = format!(
        "{}&{}&{}&data={}",
        request.target.strip_prefix("/app").unwrap(),
        STANDARD.encode(signed_nonce),
        fields["_nonce"],
        fields["data"]
    );
    let mut mac = Hmac::<Sha256>::new_from_slice(&signed_nonce).unwrap();
    mac.update(signature_input.as_bytes());
    assert_eq!(
        fields["signature"],
        STANDARD.encode(mac.finalize().into_bytes())
    );
}

fn response(result: Value) -> MockResponse {
    MockResponse::json(200, &json!({"code":0,"result":result}).to_string())
}

#[test]
fn signed_http_reads_owned_home_directory() {
    let (base, requests) = mock_server(vec![response(
        json!({"homelist":[{"uid":"123","id":"9","dids":["one"],"roomlist":[]}],"has_more":false}),
    )]);
    let client = client(&base, Duration::from_secs(1));
    let minute = || time::OffsetDateTime::now_utc().unix_timestamp() / 60;
    let start = minute();
    let homes = block_on(client.get_owned_homes(&session())).unwrap();
    let end = minute();
    assert_eq!(homes.homes[0].dids, ["one"]);
    let request = requests.recv().unwrap();
    assert_eq!(request.target, "/app/v2/homeroom/gethome");
    let headers = request.headers.to_ascii_lowercase();
    assert!(headers.starts_with("post "));
    assert!(headers.contains("content-type: application/x-www-form-urlencoded"));
    assert!(headers.contains("user-agent: mijia-test/1"));
    assert!(headers.contains("x-xiaomi-protocal-flag-cli: protocal-http2"));
    assert!(headers.contains("cookie: userid=123; servicetoken=service+token/secret"));
    let fields = form(&request);
    assert_eq!(fields.len(), 3);
    assert_eq!(
        fields["data"],
        r#"{"app_ver":9,"fetch_share":false,"fetch_share_dev":false,"limit":150,"plat_form":0}"#
    );
    assert_signed(&request);
    let nonce = STANDARD.decode(&fields["_nonce"]).unwrap();
    let signed_minute = u32::from_be_bytes(nonce[8..].try_into().unwrap());
    assert!((start..=end).contains(&i64::from(signed_minute)));
    assert!(request.body.contains("%3D"));
    assert!(requests.try_recv().is_err());
}

#[test]
fn session_rejects_invalid_fields_and_redacts_credentials() {
    let good = session();
    let debug = format!("{good:?}");
    assert!(!debug.contains(good.service_token()));
    assert!(!debug.contains(good.ssecurity()));
    for (uid, token, key, agent) in [
        ("", "token", "c2VjcmV0", "UA"),
        ("bad;user", "token", "c2VjcmV0", "UA"),
        ("user", "bad\ntoken", "c2VjcmV0", "UA"),
        ("user", "bad token", "c2VjcmV0", "UA"),
        ("user", "", "c2VjcmV0", "UA"),
        ("user", "token", "", "UA"),
        ("user", "token", "not base64", "UA"),
        ("user", "token", "c2VjcmV0", ""),
        ("user", "token", "c2VjcmV0", "bad\nUA"),
    ] {
        let error =
            MijiaSession::new(uid.into(), token.into(), key.into(), agent.into()).unwrap_err();
        assert_eq!(error.kind(), &CloudErrorKind::InvalidInput);
        if !key.is_empty() {
            assert!(!format!("{error:?} {error}").contains(key));
        }
    }
}
