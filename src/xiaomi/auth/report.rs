use super::{AccountError, AccountErrorKind};
use crate::{
    storage::{RecoveryStop, StorageError},
    xiaomi::{certificate::CertificateValidity, cloud::CloudError},
};
use std::{error::Error as StdError, fmt};

/// State of one credential group, independent of the other services.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AuthenticationState {
    NotSignedIn,
    Checking,
    Authenticated,
    SignInRequired(FailureReason),
    Unavailable(FailureReason),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CertificateUpdate {
    NotNeeded,
    Updated,
    Failed(FailureReason),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FailureReason {
    Account(AccountError),
    Cloud(CloudError),
    CloudNotification(crate::xiaomi::cloud::NotificationError),
    AccessTokenExpired,
    MissingMijiaSession,
    MissingHaAuthorization,
    RecoveryStopped(RecoveryStop),
    MissingCertificate,
    CannotDetermineAccountIdentity,
    DifferentScannedAccount,
    AccountIdentityMismatch,
    InvalidClientIdentity,
    InvalidCertificate,
    CertificateNotCurrentlyValid,
}

impl fmt::Display for FailureReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Account(error) => error.fmt(formatter),
            Self::Cloud(error) => error.fmt(formatter),
            Self::CloudNotification(error) => error.fmt(formatter),
            Self::AccessTokenExpired => formatter.write_str("Xiaomi access token has expired"),
            Self::MissingMijiaSession => formatter.write_str("Mijia session is unavailable"),
            Self::MissingHaAuthorization => {
                formatter.write_str("HA OAuth credentials are unavailable")
            }
            Self::RecoveryStopped(RecoveryStop::Rejected) => {
                formatter.write_str("Account recovery was rejected")
            }
            Self::RecoveryStopped(RecoveryStop::VerificationRequired) => {
                formatter.write_str("Account recovery requires additional verification")
            }
            Self::MissingCertificate => formatter.write_str("Gateway certificate is unavailable"),
            Self::CannotDetermineAccountIdentity => {
                formatter.write_str("Cannot determine account identity")
            }
            Self::DifferentScannedAccount => formatter.write_str("Scanned account differs from the stored account; run auth logout before changing accounts"),
            Self::AccountIdentityMismatch => formatter.write_str("Account identity does not match the expected account"),
            Self::InvalidClientIdentity => {
                formatter.write_str("Gateway certificate client identity is invalid")
            }
            Self::InvalidCertificate => formatter.write_str("Gateway certificate is invalid"),
            Self::CertificateNotCurrentlyValid => {
                formatter.write_str("Gateway certificate is not currently valid")
            }
        }
    }
}

impl FailureReason {
    pub fn requires_verification(&self) -> bool {
        matches!(self, Self::Account(error) if error.kind() == AccountErrorKind::VerificationRequired)
            || matches!(
                self,
                Self::RecoveryStopped(RecoveryStop::VerificationRequired)
            )
    }
}

impl StdError for FailureReason {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AccountIdentity {
    pub uid: String,
    pub region: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoginFailure {
    pub stage: &'static str,
    pub reason: FailureReason,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthReport {
    pub account: Option<AccountIdentity>,
    pub recovery_stop: Option<RecoveryStop>,
    pub mijia: AuthenticationState,
    pub ha: AuthenticationState,
    pub certificate: Option<CertificateValidity>,
    pub certificate_update: CertificateUpdate,
    pub login_failure: Option<LoginFailure>,
    pub(super) completed_at: i64,
}

impl AuthReport {
    pub fn is_success(&self) -> bool {
        self.login_failure.is_none()
            && matches!(self.mijia, AuthenticationState::Authenticated)
            && matches!(self.ha, AuthenticationState::Authenticated)
            && self
                .certificate
                .is_some_and(|validity| validity.currently_valid(self.completed_at))
            && !matches!(self.certificate_update, CertificateUpdate::Failed(_))
    }

    pub fn completed_at(&self) -> i64 {
        self.completed_at
    }
    pub(crate) fn set_completed_at(&mut self, completed_at: i64) {
        self.completed_at = completed_at;
    }
}

#[derive(Debug)]
pub enum AuthError {
    Storage(StorageError),
    Cloud(CloudError),
    Account(AccountError),
    Failure(FailureReason),
    LoginStep {
        stage: &'static str,
        source: Box<AuthError>,
    },
    Missing(&'static str),
}
impl fmt::Display for AuthError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Storage(error) => error.fmt(formatter),
            Self::Cloud(error) => error.fmt(formatter),
            Self::Account(error) => error.fmt(formatter),
            Self::Failure(reason) => reason.fmt(formatter),
            Self::LoginStep { stage, source } => write!(formatter, "{stage}: {source}"),
            Self::Missing(message) => formatter.write_str(message),
        }
    }
}
impl StdError for AuthError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Storage(error) => Some(error),
            Self::Cloud(error) => Some(error),
            Self::Account(error) => Some(error),
            Self::Failure(reason) => Some(reason),
            Self::LoginStep { source, .. } => Some(source.as_ref()),
            Self::Missing(_) => None,
        }
    }
}
impl From<StorageError> for AuthError {
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}
impl From<CloudError> for AuthError {
    fn from(error: CloudError) -> Self {
        Self::Cloud(error)
    }
}
impl From<AccountError> for AuthError {
    fn from(error: AccountError) -> Self {
        Self::Account(error)
    }
}
