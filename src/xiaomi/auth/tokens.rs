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
mod tests {
    use super::*;
    use crate::xiaomi::cloud::CloudErrorKind;

    #[test]
    fn expiry_and_refresh_times_use_response_completion() {
        for (expires_in, expires_at, refresh_at) in
            [(1000, 1100, 800), (11, 111, 107), (1, 101, 100)]
        {
            let tokens = from_response(
                TokenResponse {
                    access_token: "access-secret".into(),
                    refresh_token: "refresh-secret".into(),
                    expires_in,
                },
                100,
            )
            .unwrap();
            assert_eq!(tokens.access_token, "access-secret");
            assert_eq!(tokens.refresh_token, "refresh-secret");
            assert_eq!(tokens.expires_at, expires_at);
            assert_eq!(tokens.refresh_at, refresh_at);
        }
    }

    #[test]
    fn unrepresentable_token_times_are_rejected() {
        for (completed_at, expires_in) in [(1, i64::MAX), (1, i64::MAX / 7 + 1), (i64::MAX, 1)] {
            let error = from_response(
                TokenResponse {
                    access_token: "access-secret".into(),
                    refresh_token: "refresh-secret".into(),
                    expires_in,
                },
                completed_at,
            )
            .unwrap_err();
            assert_eq!(error.kind(), &CloudErrorKind::Protocol);
            assert_eq!(error.operation(), "calculate token expiry");
            assert!(!format!("{error:?} {error}").contains("secret"));
        }
    }
}
