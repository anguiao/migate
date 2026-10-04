use super::{
    AuthError, AuthReport, AuthService, AuthenticationState, CertificateUpdate, FailureReason,
    tokens,
};
use crate::{
    storage::{StorageError, TokenSet, XiaomiRecord},
    xiaomi::{
        certificate::{CertificateValidity, ClientIdentity, validate_certificate},
        cloud::{CloudError, HomePage, OwnedHome},
    },
};

impl AuthService {
    async fn read_check_home(
        &self,
        access_token: &str,
        homes: Option<&mut Vec<OwnedHome>>,
    ) -> Result<HomePage, CloudError> {
        match homes {
            Some(homes) => {
                let catalog = self.cloud.get_owned_homes(access_token).await?;
                *homes = catalog.homes;
                Ok(HomePage {
                    uid: catalog.uid,
                    dids: Vec::new(),
                })
            }
            None => self.cloud.get_home(access_token).await,
        }
    }

    pub(super) async fn check_mode(
        &self,
        mut bridge_homes: Option<&mut Vec<OwnedHome>>,
    ) -> Result<AuthReport, AuthError> {
        let Some(stored) = self.load_validated()? else {
            return Ok(AuthReport {
                authentication: AuthenticationState::NotSignedIn,
                certificate: None,
                certificate_update: CertificateUpdate::NotNeeded,
                completed_at: (self.now)(),
            });
        };
        let record = stored.record;
        let old_validity = stored.validity;
        let mut tokens = record.tokens.clone();
        let mut refreshed = false;

        if (self.now)() >= tokens.refresh_at
            && let Err(failure) = self
                .refresh_once(&record, &mut tokens, &mut refreshed)
                .await
        {
            return self.request_failure_report(failure, old_validity);
        }

        let home = match self
            .read_check_home(&tokens.access_token, bridge_homes.as_deref_mut())
            .await
        {
            Ok(home) => home,
            Err(error) if error.is_unauthorized() && !refreshed => {
                if let Err(failure) = self
                    .refresh_once(&record, &mut tokens, &mut refreshed)
                    .await
                {
                    return self.request_failure_report(failure, old_validity);
                }
                match self
                    .read_check_home(&tokens.access_token, bridge_homes.as_deref_mut())
                    .await
                {
                    Ok(home) => home,
                    Err(error) => {
                        return Ok(self.protected_failure_report(error, old_validity, true));
                    }
                }
            }
            Err(error) => {
                return Ok(self.protected_failure_report(error, old_validity, refreshed));
            }
        };

        if bridge_homes.is_none()
            && let Err(error) = self
                .cloud
                .get_devices(&tokens.access_token, &home.dids)
                .await
        {
            if error.is_unauthorized() && !refreshed {
                if let Err(failure) = self
                    .refresh_once(&record, &mut tokens, &mut refreshed)
                    .await
                {
                    return self.request_failure_report(failure, old_validity);
                }
                if let Err(error) = self
                    .cloud
                    .get_devices(&tokens.access_token, &home.dids)
                    .await
                {
                    return Ok(self.protected_failure_report(error, old_validity, true));
                }
            } else {
                return Ok(self.protected_failure_report(error, old_validity, refreshed));
            }
        }

        let Some(uid) = home.uid else {
            return Ok(AuthReport::failed_auth(
                FailureReason::CannotDetermineAccountIdentity,
                Some(old_validity),
                (self.now)(),
            ));
        };
        if uid != record.uid {
            return Ok(AuthReport::failed_auth(
                FailureReason::AccountIdentityMismatch,
                Some(old_validity),
                (self.now)(),
            ));
        }

        let renewal_time = (self.now)();
        if !old_validity.renewal_due(renewal_time) {
            return Ok(AuthReport {
                authentication: AuthenticationState::Authenticated,
                certificate: Some(old_validity),
                certificate_update: CertificateUpdate::NotNeeded,
                completed_at: (self.now)(),
            });
        }

        let identity = ClientIdentity::from_private_key(
            &record.uid,
            &record.virtual_did,
            &record.private_key_pem,
        )
        .map_err(|error| self.corrupt_record(error))?;
        let certificate_pem = loop {
            match self
                .cloud
                .get_certificate(&tokens.access_token, &identity.csr_pem)
                .await
            {
                Ok(certificate) => break certificate,
                Err(error) if !error.is_unauthorized() => {
                    return Ok(AuthReport::failed_certificate(
                        FailureReason::Cloud(error),
                        Some(old_validity),
                        (self.now)(),
                    ));
                }
                Err(error) if refreshed => {
                    return Ok(self.certificate_sign_in_required(error, old_validity));
                }
                Err(_) => {
                    if let Err(failure) = self
                        .refresh_once(&record, &mut tokens, &mut refreshed)
                        .await
                    {
                        return self.certificate_request_failure_report(failure, old_validity);
                    }
                }
            }
        };
        let new_validity = match validate_certificate(
            &record.uid,
            &record.virtual_did,
            &record.private_key_pem,
            &certificate_pem,
        ) {
            Ok(validity) => validity,
            Err(_) => {
                return Ok(AuthReport::failed_certificate(
                    FailureReason::InvalidCertificate,
                    Some(old_validity),
                    (self.now)(),
                ));
            }
        };
        let completed_at = (self.now)();
        if !new_validity.currently_valid(completed_at) {
            return Ok(AuthReport::failed_certificate(
                FailureReason::CertificateNotCurrentlyValid,
                Some(old_validity),
                completed_at,
            ));
        }
        self.store
            .update_certificate(&certificate_pem)
            .map_err(AuthError::Storage)?;
        Ok(AuthReport {
            authentication: AuthenticationState::Authenticated,
            certificate: Some(new_validity),
            certificate_update: CertificateUpdate::Updated,
            completed_at,
        })
    }

    async fn refresh_once(
        &self,
        record: &XiaomiRecord,
        tokens: &mut TokenSet,
        refreshed: &mut bool,
    ) -> Result<(), RequestFailure> {
        *refreshed = true;
        let response = self
            .cloud
            .refresh_token(
                &record.oauth_client_uuid,
                &record.redirect_uri,
                &tokens.refresh_token,
            )
            .await
            .map_err(|error| {
                if error.is_unauthorized() {
                    RequestFailure::SignInRequired(FailureReason::Cloud(error))
                } else {
                    RequestFailure::Unavailable(FailureReason::Cloud(error))
                }
            })?;
        let replacement = tokens::from_response(response, (self.now)())
            .map_err(|error| RequestFailure::Unavailable(FailureReason::Cloud(error)))?;
        self.store
            .update_tokens(&replacement)
            .map_err(RequestFailure::Storage)?;
        *tokens = replacement;
        Ok(())
    }

    fn request_failure_report(
        &self,
        failure: RequestFailure,
        validity: CertificateValidity,
    ) -> Result<AuthReport, AuthError> {
        match failure {
            RequestFailure::SignInRequired(reason) => Ok(AuthReport {
                authentication: AuthenticationState::SignInRequired(reason),
                certificate: Some(validity),
                certificate_update: CertificateUpdate::NotNeeded,
                completed_at: (self.now)(),
            }),
            RequestFailure::Unavailable(reason) => Ok(AuthReport::failed_auth(
                reason,
                Some(validity),
                (self.now)(),
            )),
            RequestFailure::Storage(error) => Err(AuthError::Storage(error)),
        }
    }

    fn protected_failure_report(
        &self,
        error: CloudError,
        validity: CertificateValidity,
        refreshed: bool,
    ) -> AuthReport {
        let authentication = if error.is_unauthorized() && refreshed {
            AuthenticationState::SignInRequired(FailureReason::Cloud(error))
        } else {
            AuthenticationState::Unavailable(FailureReason::Cloud(error))
        };
        AuthReport {
            authentication,
            certificate: Some(validity),
            certificate_update: CertificateUpdate::NotNeeded,
            completed_at: (self.now)(),
        }
    }

    fn certificate_request_failure_report(
        &self,
        failure: RequestFailure,
        validity: CertificateValidity,
    ) -> Result<AuthReport, AuthError> {
        match failure {
            RequestFailure::SignInRequired(reason) => Ok(AuthReport {
                authentication: AuthenticationState::SignInRequired(reason.clone()),
                certificate: Some(validity),
                certificate_update: CertificateUpdate::Failed(reason),
                completed_at: (self.now)(),
            }),
            RequestFailure::Unavailable(reason) => Ok(AuthReport {
                authentication: AuthenticationState::Unavailable(reason.clone()),
                certificate: Some(validity),
                certificate_update: CertificateUpdate::Failed(reason),
                completed_at: (self.now)(),
            }),
            RequestFailure::Storage(error) => Err(AuthError::Storage(error)),
        }
    }

    fn certificate_sign_in_required(
        &self,
        error: CloudError,
        validity: CertificateValidity,
    ) -> AuthReport {
        let reason = FailureReason::Cloud(error);
        AuthReport {
            authentication: AuthenticationState::SignInRequired(reason.clone()),
            certificate: Some(validity),
            certificate_update: CertificateUpdate::Failed(reason),
            completed_at: (self.now)(),
        }
    }
}

enum RequestFailure {
    SignInRequired(FailureReason),
    Unavailable(FailureReason),
    Storage(StorageError),
}
