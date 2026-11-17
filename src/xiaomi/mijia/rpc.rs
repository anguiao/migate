use super::{MijiaClient, MijiaSession, client::READ_TIMEOUT, response::valid_description};
use crate::xiaomi::cloud::CloudError;
use serde_json::{Value, json, value::RawValue};
use std::{
    fmt,
    sync::{
        OnceLock,
        atomic::{AtomicU32, Ordering},
    },
};
use url::Url;

static RPC_IDS: OnceLock<AtomicU32> = OnceLock::new();

impl MijiaClient {
    /// Sends exactly one RPC request and preserves its native result or error.
    pub(in crate::xiaomi) async fn rpc(
        &self,
        session: &MijiaSession,
        did: &str,
        method: &str,
        params: Value,
    ) -> Result<RpcOutcome, CloudError> {
        let operation = "invoke Xiaomi RPC";
        if !valid_description(did) || matches!(did, "." | "..") || !valid_description(method) {
            return Err(CloudError::input(operation));
        }
        let mut url = Url::parse(&format!("{}/app/home/rpc/", self.base_url))
            .map_err(|_| CloudError::protocol(operation))?;
        url.path_segments_mut()
            .map_err(|_| CloudError::protocol(operation))?
            .pop_if_empty()
            .push(did);
        let path = url.path();
        let id = RPC_IDS
            .get_or_init(|| AtomicU32::new(rand::random()))
            .fetch_add(1, Ordering::Relaxed)
            % 0x8000_0000
            + 1;
        let envelope = self
            .exchange(
                operation,
                path,
                session,
                json!({"id":id,"method":method,"params":params}),
                READ_TIMEOUT,
            )
            .await?;
        if envelope.id != Some(u64::from(id)) {
            return Err(CloudError::protocol(operation));
        }
        let decode = |raw: Box<RawValue>| {
            serde_json::from_str::<Value>(raw.get()).map_err(|_| CloudError::protocol(operation))
        };
        match (envelope.result, envelope.error) {
            (Some(value), None) => Ok(RpcOutcome::Result(decode(value)?)),
            (None, Some(error)) => {
                let error = decode(error)?;
                if error.get("code").and_then(Value::as_i64).is_none() {
                    return Err(CloudError::protocol(operation));
                }
                Ok(RpcOutcome::Error(error))
            }
            _ => Err(CloudError::protocol(operation)),
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub(in crate::xiaomi) enum RpcOutcome {
    Result(Value),
    Error(Value),
}
impl fmt::Debug for RpcOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Result(_) => formatter.write_str("RpcOutcome::Result([REDACTED])"),
            Self::Error(_) => formatter.write_str("RpcOutcome::Error([REDACTED])"),
        }
    }
}

impl MijiaClient {
    /// Read the physical device's shared legacy feedback group exactly once.
    pub(in crate::xiaomi) async fn read_legacy_group(
        &self,
        session: &MijiaSession,
        did: &str,
    ) -> Result<Value, CloudError> {
        let operation = "read legacy feedback";
        match self
            .rpc(
                session,
                did,
                "get_prop",
                json!(crate::xiaomi::legacy_miio::SHARED_FIELDS),
            )
            .await?
        {
            RpcOutcome::Result(value) => Ok(value),
            RpcOutcome::Error(error) => {
                let code = error
                    .get("code")
                    .and_then(Value::as_i64)
                    .ok_or_else(|| CloudError::protocol(operation))?;
                Err(CloudError::new(
                    operation,
                    crate::xiaomi::cloud::CloudErrorKind::Business(code),
                ))
            }
        }
    }
}
