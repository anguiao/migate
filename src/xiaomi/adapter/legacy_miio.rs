use crate::xiaomi::{
    capability::PropertyBinding,
    legacy_miio::{FeedbackError, Mapping},
};
use serde_json::{Value, json};

#[derive(Debug)]
pub(super) enum WriteError {
    Mapping,
    Request(crate::xiaomi::lan::RequestError),
}

pub(super) async fn set_property(
    mapping: &Mapping,
    client: &crate::xiaomi::lan::Client,
    properties: &[(PropertyBinding, Value)],
    guard: impl Fn() -> bool,
    on_sent: impl FnOnce(),
) -> Result<
    crate::xiaomi::property::PropertyResults<(), crate::xiaomi::lan::RequestError>,
    WriteError,
> {
    use crate::xiaomi::{
        lan::{Failure, RequestError, SendStage},
        property::PropertyResults,
    };
    let [(binding, value)] = properties else {
        return Err(WriteError::Mapping);
    };
    let Some((method, value)) = mapping.encode(*binding, value) else {
        return Err(WriteError::Mapping);
    };
    let result = client
        .request_guarded(method, json!([value]), guard, on_sent)
        .await
        .map_err(WriteError::Request)?;
    if result != json!(["ok"]) {
        return Err(WriteError::Request(RequestError {
            stage: SendStage::Sent,
            failure: Failure::InvalidResponse,
        }));
    }
    Ok(PropertyResults {
        items: vec![Ok(())],
        protocol_error: false,
    })
}

pub(super) async fn read(
    mapping: &Mapping,
    client: &crate::xiaomi::lan::Client,
    guard: impl Fn() -> bool,
) -> Result<Vec<Result<Value, FeedbackError>>, crate::xiaomi::lan::RequestError> {
    use crate::xiaomi::lan::{Failure, RequestError, SendStage};
    let result =
        Box::pin(client.request_guarded("get_prop", json!(mapping.fields()), guard, || {})).await?;
    mapping.decode(&result).map_err(|_| RequestError {
        stage: SendStage::Sent,
        failure: Failure::InvalidResponse,
    })
}
