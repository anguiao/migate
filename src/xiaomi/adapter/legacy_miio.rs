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

/// A direct built-in command distinguishes transport failure from an
/// already-unwrapped business response that is not the expected acknowledgement.
#[derive(Debug)]
pub(super) enum CommandError {
    Request(crate::xiaomi::lan::RequestError),
    Unexpected,
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

/// Read the shared legacy group as one raw response. Shape validation belongs
/// to the shared decoder so a malformed shape can be isolated from transport
/// failure while a business/field error remains observable.
pub(super) async fn read_shared(
    client: &crate::xiaomi::lan::Client,
    guard: impl Fn() -> bool,
) -> Result<Value, crate::xiaomi::lan::RequestError> {
    Box::pin(client.request_guarded(
        "get_prop",
        json!(crate::xiaomi::legacy_miio::SHARED_FIELDS),
        guard,
        || {},
    ))
    .await
}

/// One direct built-in command outside the legacy field mapping (for example
/// `set_bh_mode`). It is not a MIoT action or a synthetic property.
pub(super) async fn command(
    client: &crate::xiaomi::lan::Client,
    method: &str,
    params: Value,
    guard: impl Fn() -> bool,
    on_sent: impl FnOnce(),
) -> Result<(), CommandError> {
    let result = Box::pin(client.request_guarded(method, params, guard, on_sent))
        .await
        .map_err(CommandError::Request)?;
    if result != json!(["ok"]) {
        return Err(CommandError::Unexpected);
    }
    Ok(())
}
