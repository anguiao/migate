use crate::{
    storage::TokenSet,
    xiaomi::cloud::{CloudError, TokenResponse},
};

pub(super) fn from_response(
    response: TokenResponse,
    completed_at: i64,
) -> Result<TokenSet, CloudError> {
    let expires_at = completed_at
        .checked_add(response.expires_in)
        .ok_or_else(|| CloudError::protocol("calculate token expiry"))?;
    let refresh_offset = response
        .expires_in
        .checked_mul(7)
        .ok_or_else(|| CloudError::protocol("calculate token expiry"))?
        / 10;
    let refresh_at = completed_at
        .checked_add(refresh_offset)
        .ok_or_else(|| CloudError::protocol("calculate token expiry"))?;
    Ok(TokenSet {
        access_token: response.access_token,
        refresh_token: response.refresh_token,
        expires_at,
        refresh_at,
    })
}

#[cfg(test)]
mod tests;
