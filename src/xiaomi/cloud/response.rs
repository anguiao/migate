use super::{CloudError, CloudErrorKind};
use serde::{Deserialize, Deserializer, de::DeserializeOwned};
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

#[derive(Deserialize)]
#[serde(untagged)]
pub(super) enum Identifier {
    Text(String),
    Signed(i64),
    Unsigned(u64),
}

impl Identifier {
    pub fn into_string(self) -> String {
        match self {
            Self::Text(value) => value,
            Self::Signed(value) => value.to_string(),
            Self::Unsigned(value) => value.to_string(),
        }
    }
}

pub(super) fn valid_description(value: &str) -> bool {
    !value.is_empty() && !value.chars().any(char::is_control)
}

pub(super) fn description<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    let value = String::deserialize(deserializer)?;
    if !valid_description(&value) {
        return Err(serde::de::Error::custom("invalid description"));
    }
    Ok(value)
}

pub(super) fn null_list<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Option::<Vec<T>>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Deserialize)]
pub(super) struct Members {
    #[serde(default, deserialize_with = "null_list")]
    dids: Vec<String>,
    #[serde(default, deserialize_with = "null_list")]
    roomlist: Vec<Room>,
}

#[derive(Deserialize)]
pub(super) struct Room {
    #[serde(default, deserialize_with = "null_list")]
    pub(super) dids: Vec<String>,
}

impl Members {
    pub fn into_dids(self) -> impl Iterator<Item = String> {
        self.dids
            .into_iter()
            .chain(self.roomlist.into_iter().flat_map(|room| room.dids))
    }
}
