use crate::{
    storage::StorageError,
    xiaomi::{
        certificate::{CertificateStatus, CertificateValidity},
        cloud::CloudError,
    },
};
use std::{
    error::Error as StdError,
    fmt::{self, Write as _},
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

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

impl AuthReport {
    pub(crate) fn log_status(&self, now: i64) {
        let authentication_level = match &self.authentication {
            AuthenticationState::SignInRequired(_) | AuthenticationState::Unavailable(_) => {
                log::Level::Warn
            }
            _ => log::Level::Info,
        };
        log::log!(
            authentication_level,
            "{}",
            format_authentication(&self.authentication)
        );
        let certificate_level = match self.certificate.map(|validity| validity.status(now)) {
            Some(CertificateStatus::NotYetValid | CertificateStatus::Expired) => log::Level::Warn,
            _ => log::Level::Info,
        };
        log::log!(
            certificate_level,
            "{}",
            format_certificate(self.certificate, now)
        );
        if let CertificateUpdate::Failed(reason) = &self.certificate_update {
            log::warn!("Gateway certificate update failed: {reason}.");
        }
    }

    pub(crate) fn format_status(&self, now: i64) -> String {
        let mut output = format!(
            "{}\n{}",
            format_authentication(&self.authentication),
            format_certificate(self.certificate, now)
        );
        if let CertificateUpdate::Failed(reason) = &self.certificate_update {
            let _ = write!(output, "\nGateway certificate update failed: {reason}.");
        }
        output
    }
}

fn format_authentication(authentication: &AuthenticationState) -> String {
    match authentication {
        AuthenticationState::NotSignedIn => "Xiaomi: not signed in (cn).".to_owned(),
        AuthenticationState::Checking => "Xiaomi: checking authentication (cn)...".to_owned(),
        AuthenticationState::Authenticated => "Xiaomi: authenticated (cn).".to_owned(),
        AuthenticationState::SignInRequired(reason) => {
            format!("Xiaomi: sign-in required (cn). {reason}.")
        }
        AuthenticationState::Unavailable(reason) => {
            format!("Xiaomi: authentication unavailable (cn). {reason}.")
        }
    }
}

fn format_certificate(validity: Option<CertificateValidity>, now: i64) -> String {
    let Some(validity) = validity else {
        return "Gateway certificate: not prepared.".to_owned();
    };
    match validity.status(now) {
        CertificateStatus::NotYetValid => format!(
            "Gateway certificate: not valid before {}.",
            timestamp(validity.not_before)
        ),
        CertificateStatus::Valid => format!(
            "Gateway certificate: valid until {}.",
            timestamp(validity.not_after)
        ),
        CertificateStatus::RenewalDue => format!(
            "Gateway certificate: valid until {}; renewal due.",
            timestamp(validity.not_after)
        ),
        CertificateStatus::Expired => format!(
            "Gateway certificate: expired at {}.",
            timestamp(validity.not_after)
        ),
    }
}

fn timestamp(value: i64) -> String {
    OffsetDateTime::from_unix_timestamp(value)
        .expect("validated certificate timestamp")
        .format(&Rfc3339)
        .expect("RFC 3339 timestamp formatting")
}
