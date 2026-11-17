use super::{MijiaClient, MijiaSession, client::WRITE_TIMEOUT, response::valid_description};
use crate::xiaomi::cloud::CloudError;
use serde_json::{Value, json};
use std::fmt;

impl MijiaClient {
    /// Returns the correlated native action code and output without treating
    /// acceptance as a state observation.
    pub(in crate::xiaomi) async fn action(
        &self,
        session: &MijiaSession,
        did: &str,
        siid: u32,
        aiid: u32,
        input: Vec<Value>,
    ) -> Result<ActionResult, CloudError> {
        let operation = "invoke Xiaomi action";
        if !valid_description(did) || siid == 0 || aiid == 0 {
            return Err(CloudError::input(operation));
        }
        let result: Value = self
            .protected_post_with_timeout(
                operation,
                "/app/miotspec/action",
                session,
                json!({"params":{"did":did,"siid":siid,"aiid":aiid,"in":input}}),
                WRITE_TIMEOUT,
            )
            .await?;
        if result.get("did").and_then(Value::as_str) != Some(did)
            || result.get("siid").and_then(Value::as_u64) != Some(u64::from(siid))
            || result.get("aiid").and_then(Value::as_u64) != Some(u64::from(aiid))
        {
            return Err(CloudError::protocol(operation));
        }
        let code = result
            .get("code")
            .and_then(Value::as_i64)
            .ok_or_else(|| CloudError::protocol(operation))?;
        Ok(ActionResult { code, result })
    }
}

#[derive(Clone, Eq, PartialEq)]
pub(in crate::xiaomi) struct ActionResult {
    pub(in crate::xiaomi) code: i64,
    pub(in crate::xiaomi) result: Value,
}
impl fmt::Debug for ActionResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ActionResult")
            .field("code", &self.code)
            .field("result", &"[REDACTED]")
            .finish()
    }
}
