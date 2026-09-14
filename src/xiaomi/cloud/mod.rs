mod client;
mod error;
mod oauth;

pub use client::{CLOUD_BASE_URL, CloudClient, HomePage, TokenResponse};
pub use error::{CloudError, CloudErrorKind};
pub use oauth::{AUTHORIZATION_URL, AuthorizationAttempt, validate_saved_redirect_uri};

pub const REGION: &str = "cn";
pub const CLIENT_ID: u64 = 2_882_303_761_520_251_711;

#[cfg(test)]
mod tests;
