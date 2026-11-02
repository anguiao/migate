use crate::{
    storage::LanRecord,
    xiaomi::{
        capability::PropertyBinding,
        lan::{Client, Failure, RequestError, SendStage},
        legacy_miio::Mapping,
    },
};
use serde_json::{Value, json};
use std::{
    net::SocketAddr,
    time::{Duration, Instant},
};

pub(super) async fn probe(
    record: &LanRecord,
    address: SocketAddr,
    binding: PropertyBinding,
    mapping: Option<&'static Mapping>,
    timeout: Duration,
) -> Result<Client, RequestError> {
    let client = Client::new(
        record.did.parse().expect("LAN DID validated"),
        address,
        record.token.clone(),
    );
    #[cfg(test)]
    let client = {
        let mut client = client;
        client.set_timeout(timeout);
        client
    };
    let invalid = |failure| RequestError {
        stage: SendStage::Sent,
        failure,
    };
    let started = Instant::now();
    {
        let info = Box::pin(client.request("miIO.info", json!([]))).await?;
        if info.get("model").and_then(Value::as_str) != Some(&record.model) {
            return Err(invalid(Failure::Identity));
        }
        let remaining = timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Err(RequestError {
                stage: SendStage::BeforeSend,
                failure: Failure::Timeout,
            });
        }
        if let Some(mapping) = mapping {
            let result = Box::pin(client.request_with_timeout(
                "get_prop",
                json!(mapping.fields()),
                remaining,
            ))
            .await?;
            let values = mapping
                .decode(&result)
                .map_err(|_| invalid(Failure::InvalidResponse))?;
            let index = mapping
                .index(binding)
                .expect("legacy profile contains the identity binding");
            if values[index].is_err() {
                return Err(invalid(Failure::InvalidResponse));
            }
            return Ok(client.clone());
        }
        let result = Box::pin(client.request_with_timeout(
            "get_properties",
            json!([{"did":record.did,"siid":binding.siid,"piid":binding.piid}]),
            remaining,
        ))
        .await?;
        let item = result
            .as_array()
            .and_then(|items| (items.len() == 1).then(|| &items[0]))
            .ok_or_else(|| invalid(Failure::InvalidResponse))?;
        if item.get("did").and_then(Value::as_str) != Some(&record.did)
            || item.get("siid").and_then(Value::as_u64) != Some(u64::from(binding.siid))
            || item.get("piid").and_then(Value::as_u64) != Some(u64::from(binding.piid))
        {
            return Err(invalid(Failure::Identity));
        }
        let code = item
            .get("code")
            .and_then(Value::as_i64)
            .ok_or_else(|| invalid(Failure::InvalidResponse))?;
        if code != 0 {
            return Err(invalid(Failure::Rejected(code)));
        }
        if item.get("value").and_then(Value::as_bool).is_none() {
            return Err(invalid(Failure::InvalidResponse));
        }
        Ok(client.clone())
    }
}
