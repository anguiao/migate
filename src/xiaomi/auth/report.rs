use crate::{
    storage::StorageError,
    xiaomi::{certificate::CertificateValidity, cloud::CloudError},
};
use std::{error::Error as StdError, fmt};

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
    Cloud(CloudError),
    CannotDetermineAccountIdentity,
    AccountIdentityMismatch,
    InvalidClientIdentity,
    InvalidCertificate,
    CertificateNotCurrentlyValid,
}

impl fmt::Display for FailureReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cloud(error) => error.fmt(formatter),
            Self::CannotDetermineAccountIdentity => formatter
                .write_str("Cannot determine account identity required for gateway certificate"),
            Self::AccountIdentityMismatch => {
                formatter.write_str("Cloud account identity does not match stored credentials")
            }
            Self::InvalidClientIdentity => {
                formatter.write_str("Gateway certificate client identity is invalid")
            }
            Self::InvalidCertificate => {
                formatter.write_str("Gateway certificate response is invalid")
            }
            Self::CertificateNotCurrentlyValid => {
                formatter.write_str("Gateway certificate is not currently valid")
            }
        }
    }
}

impl StdError for FailureReason {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthReport {
    pub authentication: AuthenticationState,
    pub certificate: Option<CertificateValidity>,
    pub certificate_update: CertificateUpdate,
    pub(super) completed_at: i64,
}

impl AuthReport {
    pub fn is_success(&self) -> bool {
        matches!(self.authentication, AuthenticationState::Authenticated)
            && self
                .certificate
                .is_some_and(|validity| validity.currently_valid(self.completed_at))
            && !matches!(self.certificate_update, CertificateUpdate::Failed(_))
    }

    pub fn completed_at(&self) -> i64 {
        self.completed_at
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        authentication: AuthenticationState,
        certificate: Option<CertificateValidity>,
        certificate_update: CertificateUpdate,
        completed_at: i64,
    ) -> Self {
        Self {
            authentication,
            certificate,
            certificate_update,
            completed_at,
        }
    }

    pub(super) fn failed_auth(
        reason: FailureReason,
        certificate: Option<CertificateValidity>,
        completed_at: i64,
    ) -> Self {
        Self {
            authentication: AuthenticationState::Unavailable(reason),
            certificate,
            certificate_update: CertificateUpdate::NotNeeded,
            completed_at,
        }
    }

    pub(super) fn failed_cloud_auth(
        error: CloudError,
        certificate: Option<CertificateValidity>,
        completed_at: i64,
    ) -> Self {
        let authentication = if error.is_unauthorized() {
            AuthenticationState::SignInRequired(FailureReason::Cloud(error))
        } else {
            AuthenticationState::Unavailable(FailureReason::Cloud(error))
        };
        Self {
            authentication,
            certificate,
            certificate_update: CertificateUpdate::NotNeeded,
            completed_at,
        }
    }

    pub(super) fn failed_certificate(
        reason: FailureReason,
        certificate: Option<CertificateValidity>,
        completed_at: i64,
    ) -> Self {
        let authentication = match &reason {
            FailureReason::Cloud(error) if error.is_unauthorized() => {
                AuthenticationState::SignInRequired(reason.clone())
            }
            _ => AuthenticationState::Authenticated,
        };
        Self {
            authentication,
            certificate,
            certificate_update: CertificateUpdate::Failed(reason),
            completed_at,
        }
    }
}

#[derive(Debug)]
pub enum AuthError {
    Storage(StorageError),
    Cloud(CloudError),
}

impl fmt::Display for AuthError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Storage(error) => error.fmt(formatter),
            Self::Cloud(error) => error.fmt(formatter),
        }
    }
}

impl StdError for AuthError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Storage(error) => Some(error),
            Self::Cloud(error) => Some(error),
        }
    }
}

impl From<StorageError> for AuthError {
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}
