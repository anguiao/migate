use super::cloud::CloudErrorKind;
use serde::Deserialize;
use std::error::Error as _;

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

pub(super) fn response_read_error_kind(error: &std::io::Error) -> CloudErrorKind {
    if error.kind() == std::io::ErrorKind::TimedOut {
        return CloudErrorKind::Timeout;
    }
    let mut source = error.source();
    while let Some(current) = source {
        if current
            .downcast_ref::<isahc::Error>()
            .is_some_and(isahc::Error::is_timeout)
        {
            return CloudErrorKind::Timeout;
        }
        source = current.source();
    }
    CloudErrorKind::Network
}
