use super::{CloudClient, CloudError, CloudErrorKind, response::valid_description};
use crate::xiaomi::capability::PropertyBinding;
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;

#[derive(Deserialize)]
struct PropertyResult {
    did: String,
    siid: u32,
    piid: u32,
    code: i64,
    #[serde(default)]
    value: Value,
}

impl PropertyResult {
    fn validate(
        &self,
        operation: &'static str,
        did: &str,
        binding: PropertyBinding,
        accepted: impl FnOnce(i64) -> bool,
    ) -> Result<(), CloudError> {
        if self.did != did || self.siid != binding.siid || self.piid != binding.piid {
            return Err(CloudError::protocol(operation));
        }
        if !accepted(self.code) {
            return Err(CloudError::new(
                operation,
                CloudErrorKind::PropertyRejected(self.code),
            ));
        }
        Ok(())
    }
}

impl CloudClient {
    pub async fn read_property(
        &self,
        access_token: &str,
        did: &str,
        binding: PropertyBinding,
    ) -> Result<bool, CloudError> {
        let operation = "read Xiaomi property";
        validate_target(operation, did, binding)?;
        let [result]: [PropertyResult; 1] = self
            .protected_post_with_timeout(
                operation,
                "/app/v2/miotspec/prop/get",
                access_token,
                json!({"datasource":1,"params":[{"did":did,"siid":binding.siid,"piid":binding.piid}]}),
                Duration::from_secs(30),
            )
            .await?;
        result.validate(operation, did, binding, |code| code == 0)?;
        result
            .value
            .as_bool()
            .ok_or_else(|| CloudError::protocol(operation))
    }

    pub async fn set_property(
        &self,
        access_token: &str,
        did: &str,
        binding: PropertyBinding,
        value: bool,
    ) -> Result<(), CloudError> {
        let operation = "write Xiaomi property";
        validate_target(operation, did, binding)?;
        let [result]: [PropertyResult; 1] = self
            .protected_post_with_timeout(
                operation,
                "/app/v2/miotspec/prop/set",
                access_token,
                json!({"params":[{"did":did,"siid":binding.siid,"piid":binding.piid,"value":value}]}),
                Duration::from_secs(15),
            )
            .await?;
        result.validate(operation, did, binding, |code| matches!(code, 0 | 1))
    }
}

fn validate_target(
    operation: &'static str,
    did: &str,
    binding: PropertyBinding,
) -> Result<(), CloudError> {
    if !valid_description(did) || binding.siid == 0 || binding.piid == 0 {
        return Err(CloudError::input(operation));
    }
    Ok(())
}
