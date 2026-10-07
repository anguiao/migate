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
    cloud: &CloudClient,
    token: &str,
    home: &OwnedHome,
) -> Result<Vec<CloudDeviceInfo>, CloudError> {
    cloud.get_home_devices(token, home, &home.dids).await
}
