use crate::xiaomi::{
    capability::PropertyBinding,
    lan::{Client, Failure, RequestError, SendStage},
    property::{self, PropertyReading, PropertyResults},
};
use serde_json::{Value, json};

fn invalid_response() -> RequestError {
    RequestError {
        stage: SendStage::Sent,
        failure: Failure::InvalidResponse,
    }
}

fn property_code(item: &Value) -> Result<(), RequestError> {
    match item.get("code").and_then(Value::as_i64) {
        Some(0) => Ok(()),
        Some(code) => Err(RequestError {
            stage: SendStage::Sent,
            failure: Failure::Rejected(code),
        }),
        None => Err(invalid_response()),
    }
}

pub(in crate::xiaomi::adapter) async fn set_properties(
    client: &Client,
    did: &str,
    properties: &[(PropertyBinding, Value)],
    guard: impl Fn() -> bool,
    on_sent: impl FnOnce(),
) -> Result<PropertyResults<(), RequestError>, RequestError> {
    let params = properties.iter().map(|(binding, value)| {
        json!({"did":did, "siid":binding.siid, "piid":binding.piid, "value":value})
    }).collect::<Vec<_>>();
    let result =
        Box::pin(client.request_guarded("set_properties", json!(params), guard, on_sent)).await?;
    let items = result.as_array().ok_or_else(invalid_response)?;
    let bindings = properties
        .iter()
        .map(|(binding, _)| *binding)
        .collect::<Vec<_>>();
    Ok(property::correlate(
        did,
        &bindings,
        items,
        invalid_response(),
        property_code,
    ))
}

pub(in crate::xiaomi::adapter) async fn read_property(
    client: &Client,
    did: &str,
    binding: PropertyBinding,
    guard: impl Fn() -> bool,
) -> Result<PropertyReading, RequestError> {
    let result = client
        .request_guarded(
            "get_properties",
            json!([{"did":did, "siid":binding.siid, "piid":binding.piid}]),
            guard,
            || {},
        )
        .await?;
    let items = result.as_array().ok_or_else(invalid_response)?;
    let results = property::correlate(did, &[binding], items, invalid_response(), |item| {
        property_code(item)?;
        let value = item
            .get("value")
            .filter(|value| property::valid_observation(value))
            .ok_or_else(invalid_response)?;
        Ok(PropertyReading {
            value: value.clone(),
            update_time: property::update_time(item),
        })
    });
    if results.protocol_error {
        return Err(invalid_response());
    }
    results.items.into_iter().next().unwrap()
}
