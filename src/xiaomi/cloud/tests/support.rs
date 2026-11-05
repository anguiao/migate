use super::*;

pub(super) fn home(dids: impl IntoIterator<Item = impl Into<String>>) -> OwnedHome {
    OwnedHome {
        uid: "account".into(),
        home_id: "home".into(),
        group_id: group_id("account", "home"),
        dids: dids.into_iter().map(Into::into).collect(),
    }
}

pub(super) fn device(did: &str, online: bool) -> Value {
    json!({"did":did,"model":"vendor.plug","spec_type":"instance-type","isOnline":online})
}

pub(super) fn response(result: Value) -> MockResponse {
    MockResponse::json(200, &json!({"code":0,"result":result}).to_string())
}

pub(super) async fn get_home_devices(
    cloud: &crate::xiaomi::mijia::MijiaClient,
    token: &str,
    home: &OwnedHome,
) -> Result<Vec<CloudDeviceInfo>, CloudError> {
    cloud
        .get_home_devices(&session(token), home, &home.dids)
        .await
}

pub(super) fn session(token: &str) -> crate::xiaomi::mijia::MijiaSession {
    crate::xiaomi::mijia::MijiaSession::new(
        "account".into(),
        token.into(),
        "c2VjdXJpdHk=".into(),
        "test-agent".into(),
    )
    .unwrap()
}
pub(super) fn form_data(body: &str) -> Value {
    let data = url::form_urlencoded::parse(body.as_bytes())
        .find(|(key, _)| key == "data")
        .unwrap()
        .1;
    serde_json::from_str(&data).unwrap()
}
