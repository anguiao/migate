mod account;
mod bridge;
mod check;
mod login;
mod mijia;
mod refresh;
mod report;
mod service;
mod tokens;

pub use account::{
    AccountClient, AccountClientIdentity, AccountCredentials, AccountError, AccountErrorKind,
    AccountGrant, AccountStage, AuthenticatedAccount, QrAttempt, RestoredAccount,
};
pub(crate) use bridge::BridgeCertificate;
pub use login::LoginAttempt;
pub(crate) use refresh::HaRefreshes;
pub use report::{
    AccountIdentity, AuthError, AuthReport, AuthenticationState, CertificateUpdate, FailureReason,
    LoginFailure,
};
pub use service::AuthService;

#[cfg(test)]
pub(crate) mod tests;
