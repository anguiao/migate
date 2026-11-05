use super::{
    AccountError, AccountErrorKind, AccountStage, AuthorizationAttempt, check_response, field,
    http::{AccountFlow, endpoint, redirect_url, response_json, status, validate_request_url},
};
use crate::xiaomi::cloud::CLIENT_ID;
use url::Url;

const MAX_REDIRECTS: usize = 8;

impl AccountFlow {
    pub(super) async fn authorize(
        &self,
        attempt: &AuthorizationAttempt,
        uid: &str,
    ) -> Result<String, AccountError> {
        let stage = AccountStage::HaAuthorization;
        let authorization =
            Url::parse(attempt.authorization_url()).map_err(|_| AccountError::protocol(stage))?;
        self.open_consent(&authorization).await?;

        let mut json_url = authorization.clone();
        json_url.query_pairs_mut().append_pair("_json", "true");
        let response = self.send_get(&json_url, self.timeout, stage).await?;
        if response.status().is_redirection() {
            let target = redirect_url(&json_url, &response, stage)?;
            validate_request_url(&target, stage)?;
            return Err(AccountError::new(stage, AccountErrorKind::Rejected));
        }
        let response = response_json(response, stage).await?;
        check_response(&response, stage)?;
        let data = response
            .get("data")
            .filter(|v| v.is_object())
            .ok_or_else(|| AccountError::protocol(stage))?;
        check_response(data, stage)?;
        let user = data
            .get("user")
            .filter(|v| v.is_object())
            .ok_or_else(|| AccountError::protocol(stage))?;
        if field(user, "userId", stage)? != uid {
            return Err(AccountError::new(stage, AccountErrorKind::IdentityMismatch));
        }
        if field(data, "client_id", stage)? != CLIENT_ID.to_string()
            || field(data, "redirect_uri", stage)? != attempt.redirect_uri()
        {
            return Err(AccountError::protocol(stage));
        }
        let followup = field(data, "followup", stage)?;
        validate_context_followup(&followup, attempt)?;
        let device_id = field(data, "device_id", stage)?;
        if device_id.len() != 32 || !device_id.bytes().all(|v| v.is_ascii_hexdigit()) {
            return Err(AccountError::protocol(stage));
        }
        let pt = field(data, "pt", stage)?;
        let scope = field(data, "scope_id", stage)?;
        let ssign = field(data, "_ssign", stage)?;
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("pt", &pt)
            .append_pair("device_id", &device_id)
            .append_pair("followup", &followup)
            .append_pair("scope_id", &scope)
            .append_pair("redirect_uri", attempt.redirect_uri())
            .append_pair("client_id", &CLIENT_ID.to_string())
            .append_pair("_ssign", &ssign)
            .append_pair("_json", "true")
            .finish();
        let post_url = endpoint(super::ACCOUNT_URL, "/oauth2/userAuthorization", stage)?;
        let response = self.send_post(&post_url, body, stage).await?;
        if response.status().is_redirection() {
            let target = redirect_url(&post_url, &response, stage)?;
            validate_request_url(&target, stage)?;
            return Err(AccountError::new(stage, AccountErrorKind::Rejected));
        }
        let response = response_json(response, stage).await?;
        check_response(&response, stage)?;
        let data = response
            .get("data")
            .filter(|v| v.is_object())
            .ok_or_else(|| AccountError::protocol(stage))?;
        check_response(data, stage)?;
        let followup = field(data, "followup", stage)?;
        let url = Url::parse(&followup).map_err(|_| AccountError::protocol(stage))?;
        self.capture_callback(url, attempt).await
    }

    async fn open_consent(&self, authorization: &Url) -> Result<(), AccountError> {
        let stage = AccountStage::HaAuthorization;
        let mut url = authorization.clone();
        for _ in 0..=MAX_REDIRECTS {
            validate_request_url(&url, stage)?;
            let response = self.send_get(&url, self.timeout, stage).await?;
            if response.status().is_redirection() {
                url = redirect_url(&url, &response, stage)?;
                continue;
            }
            status(response.status().as_u16(), stage)?;
            if url.path() == "/fe/service/oauth2/authorize" {
                return Ok(());
            }
            let data = response_json(response, stage).await?;
            check_response(&data, stage)?;
            return Err(AccountError::new(stage, AccountErrorKind::Rejected));
        }
        Err(AccountError::protocol(stage))
    }

    async fn capture_callback(
        &self,
        mut url: Url,
        attempt: &AuthorizationAttempt,
    ) -> Result<String, AccountError> {
        let stage = AccountStage::HaAuthorization;
        for _ in 0..=MAX_REDIRECTS {
            let expected =
                Url::parse(attempt.redirect_uri()).map_err(|_| AccountError::protocol(stage))?;
            if url.host_str() == expected.host_str() {
                if url.scheme() != expected.scheme()
                    || url.port_or_known_default() != expected.port_or_known_default()
                    || url.path() != expected.path()
                    || !url.username().is_empty()
                    || url.password().is_some()
                    || url.fragment().is_some()
                {
                    return Err(AccountError::protocol(stage));
                }
                if url.query_pairs().any(|(key, _)| key == "error") {
                    let mut state = None;
                    let mut rejection = false;
                    for (key, value) in url.query_pairs() {
                        match key.as_ref() {
                            "state" if state.is_none() && !value.is_empty() => {
                                state = Some(value.into_owned());
                            }
                            "error" if !rejection && !value.is_empty() => rejection = true,
                            "state" | "error" | "code" => {
                                return Err(AccountError::protocol(stage));
                            }
                            _ => (),
                        }
                    }
                    if state.as_deref() != Some(attempt.state()) {
                        return Err(AccountError::protocol(stage));
                    }
                    return Err(AccountError::new(stage, AccountErrorKind::Rejected));
                }
                return attempt
                    .parse_callback(url.as_str())
                    .map_err(|_| AccountError::protocol(stage));
            }
            validate_request_url(&url, stage)?;
            let response = self.send_get(&url, self.timeout, stage).await?;
            if !response.status().is_redirection() {
                status(response.status().as_u16(), stage)?;
                let data = response_json(response, stage).await?;
                check_response(&data, stage)?;
                return Err(AccountError::protocol(stage));
            }
            url = redirect_url(&url, &response, stage)?;
        }
        Err(AccountError::protocol(stage))
    }
}

fn validate_context_followup(
    value: &str,
    attempt: &AuthorizationAttempt,
) -> Result<(), AccountError> {
    let stage = AccountStage::HaAuthorization;
    let url = Url::parse(value).map_err(|_| AccountError::protocol(stage))?;
    validate_request_url(&url, stage)?;
    let mut device_id = None;
    let mut state = None;
    let mut client = false;
    let mut redirect = false;
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "device_id" if device_id.is_none() => device_id = Some(value.into_owned()),
            "state" if state.is_none() => state = Some(value.into_owned()),
            "client_id" if !client && value == CLIENT_ID.to_string() => client = true,
            "redirect_uri" if !redirect && value == attempt.redirect_uri() => redirect = true,
            "device_id" | "state" | "client_id" | "redirect_uri" => {
                return Err(AccountError::protocol(stage));
            }
            _ => (),
        }
    }
    if device_id.as_deref() != Some(attempt.device_id())
        || state.as_deref() != Some(attempt.state())
    {
        return Err(AccountError::protocol(stage));
    }
    Ok(())
}
