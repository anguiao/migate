use super::{CloudClient, CloudError, CloudErrorKind, response::valid_description};
use crate::xiaomi::{
    capability::PropertyBinding,
    property::{PropertyResults, correlate, valid_bindings, valid_value},
};
use serde_json::{Value, json};
use std::time::Duration;

impl CloudClient {
    pub async fn read_property(
        &self,
        access_token: &str,
        did: &str,
        binding: PropertyBinding,
    ) -> Result<bool, CloudError> {
        self.read_property_value(access_token, did, binding)
            .await?
            .as_bool()
            .ok_or_else(|| CloudError::protocol("read Xiaomi property"))
    }

    pub async fn read_property_value(
        &self,
        access_token: &str,
        did: &str,
        binding: PropertyBinding,
    ) -> Result<Value, CloudError> {
        single_result(
            "read Xiaomi property",
            self.read_properties(access_token, did, &[binding]).await?,
        )
    }

    pub async fn read_properties(
        &self,
        access_token: &str,
        did: &str,
        bindings: &[PropertyBinding],
    ) -> Result<PropertyResults<Value, CloudError>, CloudError> {
        let operation = "read Xiaomi property";
        validate_targets(operation, did, bindings)?;
        let params: Vec<_> = bindings
            .iter()
            .map(|binding| json!({"did":did,"siid":binding.siid,"piid":binding.piid}))
            .collect();
        let result: Vec<Value> = self
            .protected_post_with_timeout(
                operation,
                "/app/v2/miotspec/prop/get",
                access_token,
                json!({"datasource":1,"params":params}),
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
                item.get("value")
                    .filter(|value| valid_value(value))
                    .cloned()
                    .ok_or_else(|| CloudError::protocol(operation))
            },
        ))
    }

    pub async fn set_property(
        &self,
        access_token: &str,
        did: &str,
        binding: PropertyBinding,
        value: bool,
    ) -> Result<(), CloudError> {
        single_result(
            "write Xiaomi property",
            self.set_properties(access_token, did, &[(binding, json!(value))])
                .await?,
        )
    }

    pub async fn set_properties(
        &self,
        access_token: &str,
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
                "/app/v2/miotspec/prop/set",
                access_token,
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

fn single_result<T>(
    operation: &'static str,
    result: PropertyResults<T, CloudError>,
) -> Result<T, CloudError> {
    if result.protocol_error {
        return Err(CloudError::protocol(operation));
    }
    result
        .items
        .into_iter()
        .next()
        .expect("one property was requested")
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
