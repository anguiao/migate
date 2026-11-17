use super::{MijiaClient, MijiaSession, response::valid_description};
use crate::xiaomi::{
    capability::PropertyBinding,
    cloud::{CloudError, CloudErrorKind},
    property::{
        PropertyReading, PropertyResults, correlate, update_time, valid_bindings,
        valid_observation, valid_value,
    },
};
use serde_json::{Value, json};
use std::time::Duration;

impl MijiaClient {
    pub(in crate::xiaomi) async fn read_properties(
        &self,
        credential: &MijiaSession,
        did: &str,
        bindings: &[PropertyBinding],
    ) -> Result<PropertyResults<PropertyReading, CloudError>, CloudError> {
        let operation = "read Xiaomi property";
        validate_targets(operation, did, bindings)?;
        let params: Vec<_> = bindings
            .iter()
            .map(|binding| json!({"did":did,"siid":binding.siid,"piid":binding.piid}))
            .collect();
        let result: Vec<Value> = self
            .protected_post_with_timeout(
                operation,
                "/app/miotspec/prop/get",
                credential,
                json!({"datasource":2,"params":params}),
                Duration::from_secs(30),
            )
            .await?;
        Ok(correlate(
            did,
            bindings,
            &result,
            CloudError::protocol(operation),
            |item| {
                accepted_code(operation, item, |code| code == 0)?;
                Ok(PropertyReading {
                    value: item
                        .get("value")
                        .filter(|value| valid_observation(value))
                        .cloned()
                        .ok_or_else(|| CloudError::protocol(operation))?,
                    update_time: update_time(item),
                })
            },
        ))
    }

    pub(in crate::xiaomi) async fn set_properties(
        &self,
        credential: &MijiaSession,
        did: &str,
        properties: &[(PropertyBinding, Value)],
    ) -> Result<PropertyResults<(), CloudError>, CloudError> {
        let operation = "write Xiaomi property";
        let bindings: Vec<_> = properties.iter().map(|(binding, _)| *binding).collect();
        validate_targets(operation, did, &bindings)?;
        if properties.iter().any(|(_, value)| !valid_value(value)) {
            return Err(CloudError::input(operation));
        }
        let params: Vec<_> = properties.iter().map(|(binding, value)| {
            json!({"did":did,"siid":binding.siid,"piid":binding.piid,"value":value})
        }).collect();
        let result: Vec<Value> = self
            .protected_post_with_timeout(
                operation,
                "/app/miotspec/prop/set",
                credential,
                json!({"params":params}),
                Duration::from_secs(15),
            )
            .await?;
        Ok(correlate(
            did,
            &bindings,
            &result,
            CloudError::protocol(operation),
            |item| accepted_code(operation, item, |code| matches!(code, 0 | 1)),
        ))
    }
}

fn accepted_code(
    operation: &'static str,
    item: &Value,
    accepted: impl FnOnce(i64) -> bool,
) -> Result<(), CloudError> {
    let code = item
        .get("code")
        .and_then(Value::as_i64)
        .ok_or_else(|| CloudError::protocol(operation))?;
    if accepted(code) {
        Ok(())
    } else {
        Err(CloudError::new(
            operation,
            CloudErrorKind::PropertyRejected(code),
        ))
    }
}

fn validate_targets(
    operation: &'static str,
    did: &str,
    bindings: &[PropertyBinding],
) -> Result<(), CloudError> {
    if !valid_description(did) || !valid_bindings(bindings) {
        return Err(CloudError::input(operation));
    }
    Ok(())
}
