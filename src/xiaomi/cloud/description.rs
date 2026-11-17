use super::{CloudClient, CloudError};
use isahc::Request;
use serde_json::Value;
use url::Url;

impl CloudClient {
    pub(in crate::xiaomi) async fn get_instance(
        &self,
        instance_type: &str,
    ) -> Result<Value, CloudError> {
        let operation = "read MIoT instance description";
        if instance_type.is_empty() {
            return Err(CloudError::input(operation));
        }
        let mut url = Url::parse(&format!("{}/miot-spec-v2/instance", self.instance_base_url))
            .map_err(|_| CloudError::input(operation))?;
        url.query_pairs_mut().append_pair("type", instance_type);
        let request = Request::builder()
            .method("GET")
            .uri(url.as_str())
            .body(String::new())
            .map_err(|_| CloudError::protocol(operation))?;
        let value: Value = self.send(request, operation).await?;
        if value.get("type").and_then(Value::as_str) != Some(instance_type) {
            return Err(CloudError::protocol(operation));
        }
        Ok(value)
    }
}
