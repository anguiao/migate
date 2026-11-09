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
        let authorization = Url::parse(attempt.authorization_url())
            .map_err(|_| AccountError::protocol(stage).at("authorization URL"))?;
        self.open_consent(&authorization).await?;

        let mut json_url = authorization.clone();
        json_url.query_pairs_mut().append_pair("_json", "true");
        let response = self
            .send_get(&json_url, self.timeout, stage)
            .await
            .map_err(|error| error.at("authorization context request"))?;
        if response.status().is_redirection() {
            let target = redirect_url(&json_url, &response, stage)
                .map_err(|error| error.at("authorization context redirect"))?;
            validate_request_url(&target, stage)
                .map_err(|error| error.at("authorization context redirect target"))?;
            return Err(AccountError::new(stage, AccountErrorKind::Rejected)
                .at("authorization context redirect"));
        }
        let response = response_json(response, stage)
            .await
            .map_err(|error| error.at("authorization context response"))?;
        check_response(&response, stage)
            .map_err(|error| error.at("authorization context response"))?;
        let data = response
            .get("data")
            .filter(|v| v.is_object())
            .ok_or_else(|| AccountError::protocol(stage).at("authorization context data"))?;
        check_response(data, stage).map_err(|error| error.at("authorization context data"))?;
        let user = data
            .get("user")
            .filter(|v| v.is_object())
            .ok_or_else(|| AccountError::protocol(stage).at("authorization context user"))?;
        if field(user, "userId", stage)
            .map_err(|error| error.at("authorization context user ID"))?
            != uid
        {
            return Err(AccountError::new(stage, AccountErrorKind::IdentityMismatch)
                .at("authorization context user ID"));
        }
        if field(data, "client_id", stage)
            .map_err(|error| error.at("authorization context client ID"))?
            != CLIENT_ID.to_string()
        {
            return Err(AccountError::protocol(stage).at("authorization context client ID"));
        }
        if field(data, "redirect_uri", stage)
            .map_err(|error| error.at("authorization context callback"))?
            != attempt.redirect_uri()
        {
            return Err(AccountError::protocol(stage).at("authorization context callback"));
        }
        let followup = field(data, "followup", stage)
            .map_err(|error| error.at("authorization context follow-up"))?;
        validate_context_followup(&followup, attempt)
            .map_err(|error| error.at("authorization context follow-up"))?;
        let device_id = field(data, "device_id", stage)
            .map_err(|error| error.at("authorization context device ID"))?;
        if device_id.len() != 32 || !device_id.bytes().all(|v| v.is_ascii_hexdigit()) {
            return Err(AccountError::protocol(stage).at("authorization context device ID"));
        }
        let pt = field(data, "pt", stage)
            .map_err(|error| error.at("authorization context consent parameters"))?;
        let scope = field(data, "scope_id", stage)
            .map_err(|error| error.at("authorization context scope"))?;
        let ssign = field(data, "_ssign", stage)
            .map_err(|error| error.at("authorization context signature"))?;
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
        let post_url = endpoint(super::ACCOUNT_URL, "/oauth2/userAuthorization", stage)
            .map_err(|error| error.at("authorization submission URL"))?;
        let response = self
            .send_post(&post_url, body, stage)
            .await
            .map_err(|error| error.at("authorization submission"))?;
        if response.status().is_redirection() {
            let target = redirect_url(&post_url, &response, stage)
                .map_err(|error| error.at("authorization submission redirect"))?;
            validate_request_url(&target, stage)
                .map_err(|error| error.at("authorization submission redirect target"))?;
            return Err(AccountError::new(stage, AccountErrorKind::Rejected)
                .at("authorization submission redirect"));
        }
        let response = response_json(response, stage)
            .await
            .map_err(|error| error.at("authorization submission response"))?;
        check_response(&response, stage)
            .map_err(|error| error.at("authorization submission response"))?;
        let data = response
            .get("data")
            .filter(|v| v.is_object())
            .ok_or_else(|| AccountError::protocol(stage).at("authorization submission data"))?;
        check_response(data, stage).map_err(|error| error.at("authorization submission data"))?;
        let followup = field(data, "followup", stage)
            .map_err(|error| error.at("authorization submission follow-up"))?;
        let url = Url::parse(&followup)
            .map_err(|_| AccountError::protocol(stage).at("authorization submission follow-up"))?;
        self.capture_callback(url, attempt).await
    }

    async fn open_consent(&self, authorization: &Url) -> Result<(), AccountError> {
        let stage = AccountStage::HaAuthorization;
        let mut url = authorization.clone();
        for _ in 0..=MAX_REDIRECTS {
            validate_request_url(&url, stage)
                .map_err(|error| error.at("consent redirect target"))?;
            let response = self
                .send_get(&url, self.timeout, stage)
                .await
                .map_err(|error| error.at("consent request"))?;
            if response.status().is_redirection() {
                url = redirect_url(&url, &response, stage)
                    .map_err(|error| error.at("consent redirect"))?;
                continue;
            }
            status(response.status().as_u16(), stage)
                .map_err(|error| error.at("consent response status"))?;
            if url.path() == "/fe/service/oauth2/authorize" {
                return Ok(());
            }
            let data = response_json(response, stage)
                .await
                .map_err(|error| error.at("consent response"))?;
            check_response(&data, stage).map_err(|error| error.at("consent response"))?;
            return Err(AccountError::new(stage, AccountErrorKind::Rejected).at("consent response"));
        }
        Err(AccountError::protocol(stage).at("consent redirect limit"))
    }

    async fn capture_callback(
        &self,
        mut url: Url,
        attempt: &AuthorizationAttempt,
    ) -> Result<String, AccountError> {
        let stage = AccountStage::HaAuthorization;
        for _ in 0..=MAX_REDIRECTS {
            let expected = Url::parse(attempt.redirect_uri())
                .map_err(|_| AccountError::protocol(stage).at("callback URL"))?;
            if url.host_str() == expected.host_str() {
                if url.scheme() != expected.scheme()
                    || url.port_or_known_default() != expected.port_or_known_default()
                    || url.path() != expected.path()
                    || !url.username().is_empty()
                    || url.password().is_some()
                    || url.fragment().is_some()
                {
                    return Err(AccountError::protocol(stage).at("callback target"));
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
                                return Err(AccountError::protocol(stage).at("callback rejection"));
                            }
                            _ => (),
                        }
                    }
                    if state.as_deref() != Some(attempt.state()) {
                        return Err(AccountError::protocol(stage).at("callback state"));
                    }
                    return Err(AccountError::new(stage, AccountErrorKind::Rejected)
                        .at("callback rejection"));
                }
                return attempt
                    .parse_callback(url.as_str())
                    .map_err(|_| AccountError::protocol(stage).at("callback parameters"));
            }
            validate_request_url(&url, stage)
                .map_err(|error| error.at("callback redirect target"))?;
            let response = self
                .send_get(&url, self.timeout, stage)
                .await
                .map_err(|error| error.at("callback redirect request"))?;
            if !response.status().is_redirection() {
                status(response.status().as_u16(), stage)
                    .map_err(|error| error.at("callback redirect status"))?;
                let data = response_json(response, stage)
                    .await
                    .map_err(|error| error.at("callback redirect JSON"))?;
                check_response(&data, stage)
                    .map_err(|error| error.at("callback redirect result"))?;
                return Err(
                    AccountError::protocol(stage).at("callback redirect non-redirect response")
                );
            }
            url = redirect_url(&url, &response, stage)
                .map_err(|error| error.at("callback redirect"))?;
        }
        Err(AccountError::protocol(stage).at("callback redirect limit"))
    }
}

fn validate_context_followup(
    value: &str,
    attempt: &AuthorizationAttempt,
) -> Result<(), AccountError> {
    let stage = AccountStage::HaAuthorization;
    let url = Url::parse(value)
        .map_err(|_| AccountError::protocol(stage).at("authorization follow-up URL"))?;
    validate_request_url(&url, stage)
        .map_err(|error| error.at("authorization follow-up target"))?;
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
                return Err(AccountError::protocol(stage).at("authorization follow-up parameters"));
            }
            _ => (),
        }
    }
    if device_id.as_deref() != Some(attempt.device_id()) {
        return Err(AccountError::protocol(stage).at("authorization follow-up device ID"));
    }
    if state.as_deref() != Some(attempt.state()) {
        return Err(AccountError::protocol(stage).at("authorization follow-up state"));
    }
    Ok(())
}
