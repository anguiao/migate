use super::{CloudError, CloudErrorKind};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::Value;

#[derive(Deserialize)]
pub(super) struct ApiResponse {
    code: i64,
    #[serde(default)]
    result: Value,
    #[serde(default)]
    message: Value,
}

impl ApiResponse {
    pub fn into_result<T: DeserializeOwned>(
        self,
        operation: &'static str,
    ) -> Result<T, CloudError> {
        if self.code != 0 {
            return Err(CloudError::new(
                operation,
                CloudErrorKind::Business(self.code),
            ));
        }
        serde_json::from_value(self.result).map_err(|_| CloudError::protocol(operation))
    }

    pub fn oauth_error_code(&self) -> Option<i64> {
        #[derive(Deserialize)]
        struct OAuthError {
            error: i64,
        }
        let details: OAuthError = serde_json::from_str(self.message.as_str()?).ok()?;
        Some(details.error)
    }
}
