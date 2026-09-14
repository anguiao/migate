use std::{error::Error as StdError, fmt};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CloudErrorKind {
    Unauthorized,
    HttpStatus(u16),
    Business(i64),
    Protocol,
    Network,
    Timeout,
    InvalidInput,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CloudError {
    operation: &'static str,
    kind: CloudErrorKind,
    oauth_code: Option<i64>,
}

impl CloudError {
    pub(super) fn new(operation: &'static str, kind: CloudErrorKind) -> Self {
        Self {
            operation,
            kind,
            oauth_code: None,
        }
    }
    pub(super) fn input(operation: &'static str) -> Self {
        Self::new(operation, CloudErrorKind::InvalidInput)
    }
    pub(in crate::xiaomi) fn protocol(operation: &'static str) -> Self {
        Self::new(operation, CloudErrorKind::Protocol)
    }
    pub(super) fn with_oauth_code(mut self, code: Option<i64>) -> Self {
        if matches!(self.kind, CloudErrorKind::Business(_)) {
            self.oauth_code = code;
        }
        self
    }

    pub fn operation(&self) -> &'static str {
        self.operation
    }
    pub fn kind(&self) -> &CloudErrorKind {
        &self.kind
    }
    pub fn is_unauthorized(&self) -> bool {
        self.kind == CloudErrorKind::Unauthorized
    }
    pub fn is_timeout(&self) -> bool {
        self.kind == CloudErrorKind::Timeout
    }
    pub fn http_status(&self) -> Option<u16> {
        if let CloudErrorKind::HttpStatus(value) = self.kind {
            Some(value)
        } else {
            None
        }
    }
}

impl fmt::Display for CloudError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "Failed to {}: ", self.operation)?;
        match self.kind {
            CloudErrorKind::Unauthorized => {
                formatter.write_str("authorization was rejected (HTTP 401)")
            }
            CloudErrorKind::HttpStatus(status) => write!(formatter, "HTTP status {status}"),
            CloudErrorKind::Business(code) => {
                write!(formatter, "cloud business code {code}")?;
                if let Some(oauth_code) = self.oauth_code {
                    write!(formatter, " (OAuth error {oauth_code}")?;
                    match oauth_code {
                        96002 => formatter.write_str(": missing or invalid request parameters")?,
                        96013 => formatter.write_str(": invalid authorization code")?,
                        _ => {}
                    }
                    formatter.write_str(")")?;
                }
                Ok(())
            }
            CloudErrorKind::Protocol => formatter.write_str("invalid cloud response"),
            CloudErrorKind::Network => formatter.write_str("network request failed"),
            CloudErrorKind::Timeout => formatter.write_str("network request timed out"),
            CloudErrorKind::InvalidInput => formatter.write_str("invalid input"),
        }
    }
}

impl StdError for CloudError {}
