#[derive(Default)]
pub(super) enum HaSync {
    #[default]
    Idle,
    // Retained during refresh so concurrent failures coalesce into the same attempt.
    Rejected(String),
    Refreshed(String),
    Paused,
}
use super::{
    model::AdapterError,
    state::{Backoff, Context},
};
use crate::{
    storage::XiaomiRecord,
    xiaomi::auth::{
        AccountErrorKind, AuthError, AuthService, AuthenticationState, CertificateUpdate,
        FailureReason,
    },
};
use futures_lite::future;
use std::{rc::Rc, time::Duration};

fn deadline(context: &Context, unix: i64) -> Duration {
    context.clock.now()
        + Duration::from_secs(unix.saturating_sub(context.clock.unix_now()).max(0) as u64)
}

fn saved(auth: &AuthService, context: &Context) -> Result<XiaomiRecord, AdapterError> {
    let record = auth
        .saved_bridge_credentials()?
        .ok_or_else(|| AdapterError::Unavailable("Xiaomi sign-in is required".into()))?;
    if context
        .live
        .borrow()
        .credentials
        .as_ref()
        .is_some_and(|current| current.uid != record.uid)
    {
        return Err(AdapterError::Unavailable(
            "Xiaomi account changed; restart after sign-in".into(),
        ));
    }
    context.adopt_credentials(record.clone())?;
    Ok(record)
}

fn eligibility_changed(context: &Context, reason: &str) {
    context
        .status
        .borrow_mut()
        .auth
        .set_completed_at(context.clock.unix_now());
    context.update_all_availability();
    context.update_routing(reason);
    context.credentials_changed.notify(usize::MAX);
    context.changed.notify(usize::MAX);
    context.refresh_changed.notify(usize::MAX);
    context.status_changed.notify(usize::MAX);
}

fn token_expired(context: &Context) {
    if !matches!(*context.ha_sync.borrow(), HaSync::Paused) {
        context.ha_authentication(AuthenticationState::Unavailable(
            FailureReason::AccessTokenExpired,
        ));
    }
    eligibility_changed(context, "access token expired");
}

async fn tokens(auth: &AuthService, context: Rc<Context>) -> Result<(), AdapterError> {
    let mut backoff = Backoff::new(1, 30);
    let mut retry = None;
    let mut observed_expiry = None;
    loop {
        let credentials_changed = context.credentials_changed.listen();
        let record = saved(auth, &context)?;
        let Some(authorization) = record.ha_authorization() else {
            if !matches!(
                context.status.borrow().auth.ha,
                AuthenticationState::SignInRequired(_)
            ) {
                context.ha_authentication(AuthenticationState::SignInRequired(
                    FailureReason::MissingHaAuthorization,
                ));
            }
            credentials_changed.await;
            continue;
        };
        let tokens = authorization.tokens.clone();
        let key = (tokens.access_token.clone(), tokens.expires_at);
        let is_expired = tokens.expires_at <= context.clock.unix_now();
        if is_expired && observed_expiry.as_ref() != Some(&key) {
            observed_expiry = Some(key);
            token_expired(&context);
        }
        let paused = matches!(*context.ha_sync.borrow(), HaSync::Paused);
        let rejected = matches!(&*context.ha_sync.borrow(), HaSync::Rejected(token) if token == &tokens.access_token);
        let refresh_due = retry.unwrap_or_else(|| {
            if rejected {
                context.clock.now()
            } else {
                deadline(&context, tokens.refresh_at)
            }
        });
        if !paused
            && (rejected || context.clock.unix_now() >= tokens.refresh_at)
            && context.clock.now() >= refresh_due
        {
            context.ha_authentication(AuthenticationState::Refreshing);
            let refreshing = auth.refresh_bridge_tokens(&record, || context.clock.unix_now());
            let mut refreshing = std::pin::pin!(refreshing);
            let result = if !is_expired {
                let expiry = deadline(&context, tokens.expires_at);
                match future::or(async { Some(refreshing.as_mut().await) }, async {
                    context.clock.wait_until(expiry).await;
                    observed_expiry = Some((tokens.access_token.clone(), tokens.expires_at));
                    token_expired(&context);
                    None
                })
                .await
                {
                    Some(result) => result,
                    None => refreshing.await,
                }
            } else {
                refreshing.await
            };
            match result {
                Ok(replacement) => {
                    let Some(refreshed) = replacement.ha_authorization() else {
                        context.adopt_credentials(replacement)?;
                        continue;
                    };
                    let already_due = refreshed.tokens.refresh_at <= context.clock.unix_now();
                    *context.ha_sync.borrow_mut() =
                        HaSync::Refreshed(refreshed.tokens.access_token.clone());
                    context.adopt_credentials(replacement)?;
                    context.ha_authentication(AuthenticationState::Checking);
                    context.credentials_changed.notify(usize::MAX);
                    // Extremely short grants can already be due at completion; keep maintenance bounded.
                    retry = already_due.then(|| context.clock.now() + Duration::from_secs(1));
                    backoff = Backoff::new(1, 30);
                }
                Err(AuthError::Cloud(error)) => {
                    let delay = if error.is_temporary() {
                        backoff.failed()
                    } else {
                        super::cloud::MEMBER_INTERVAL
                    };
                    if error.is_unauthorized() {
                        *context.ha_sync.borrow_mut() = HaSync::Paused;
                        context.ha_authentication(AuthenticationState::SignInRequired(
                            FailureReason::Cloud(error),
                        ));
                    } else {
                        context.ha_authentication(AuthenticationState::Unavailable(
                            FailureReason::Cloud(error),
                        ));
                    }
                    // A maintenance network failure does not suspend a still-valid, unrejected token.
                    retry = Some(context.clock.now() + delay);
                    eligibility_changed(&context, "cloud token maintenance failed");
                }
                Err(AuthError::Missing(_)) => {
                    let current = saved(auth, &context)?;
                    if current.ha_authorization().is_some() {
                        *context.ha_sync.borrow_mut() = HaSync::Idle;
                        context.ha_authentication(AuthenticationState::Checking);
                        context.credentials_changed.notify(usize::MAX);
                    }
                    retry = None;
                }
                Err(error) => return Err(error.into()),
            }
            continue;
        }
        let next = (!paused)
            .then_some(refresh_due)
            .into_iter()
            .chain((!is_expired).then(|| deadline(&context, tokens.expires_at)))
            .min();
        future::or(credentials_changed, async {
            if let Some(next) = next {
                context.clock.wait_until(next).await;
            } else {
                future::pending::<()>().await;
            }
        })
        .await;
    }
}

async fn certificates(auth: &AuthService, context: Rc<Context>) -> Result<(), AdapterError> {
    let mut backoff = Backoff::new(1, 30);
    let mut due = context.clock.now();
    let mut last_expiry = None;
    loop {
        let changed = context.changed.listen();
        let record = saved(auth, &context)?;
        let report = auth.local_status()?;
        let Some(validity) = report.certificate else {
            context.certificate_unavailable(report.certificate_update);
            changed.await;
            continue;
        };
        if context.status.borrow().auth.certificate != Some(validity) {
            context.certificate(validity, CertificateUpdate::NotNeeded);
        }
        let now = context.clock.unix_now();
        if now >= validity.not_after && last_expiry != Some(validity.not_after) {
            last_expiry = Some(validity.not_after);
            eligibility_changed(&context, "hub certificate expired");
        }
        if validity.renewal_due(now) && context.token_usable() && context.clock.now() >= due {
            let renewal = auth.renew_bridge_certificate(&record, || context.clock.unix_now());
            let mut renewal = std::pin::pin!(renewal);
            let result = if validity.currently_valid(now) {
                match future::or(async { Some(renewal.as_mut().await) }, async {
                    context
                        .clock
                        .wait_until(deadline(&context, validity.not_after))
                        .await;
                    last_expiry = Some(validity.not_after);
                    eligibility_changed(&context, "hub certificate expired");
                    None
                })
                .await
                {
                    Some(result) => result,
                    None => renewal.await,
                }
            } else {
                renewal.await
            };
            match result {
                Ok(renewed) => {
                    context.adopt_credentials(renewed.credentials)?;
                    let failed = matches!(renewed.update, CertificateUpdate::Failed(_));
                    if let Some(validity) = renewed.validity {
                        context.certificate(validity, renewed.update);
                    } else {
                        context.certificate_unavailable(renewed.update);
                    }
                    due = context.clock.now() + super::cloud::MEMBER_INTERVAL;
                    if !failed {
                        backoff = Backoff::new(1, 30);
                    }
                }
                Err(AuthError::Cloud(error)) => {
                    let delay = if error.is_temporary() {
                        backoff.failed()
                    } else {
                        super::cloud::MEMBER_INTERVAL
                    };
                    let Some(authorization) = record.ha_authorization() else {
                        continue;
                    };
                    if error.is_unauthorized() {
                        context.token_rejected(
                            &authorization.tokens.access_token,
                            FailureReason::Cloud(error.clone()),
                        )?;
                    }
                    let stale = !context.current_token(&authorization.tokens.access_token);
                    context.certificate(
                        validity,
                        CertificateUpdate::Failed(FailureReason::Cloud(error)),
                    );
                    due = if stale {
                        context.clock.now()
                    } else {
                        context.clock.now() + delay
                    };
                }
                Err(error) => return Err(error.into()),
            }
            continue;
        }
        let expiry = (now < validity.not_after).then(|| deadline(&context, validity.not_after));
        let renewal = context
            .token_usable()
            .then(|| due.max(deadline(&context, validity.renewal_at())));
        let next = expiry.into_iter().chain(renewal).min();
        future::or(
            async {
                changed.await;
            },
            async {
                if let Some(next) = next {
                    context.clock.wait_until(next).await;
                } else {
                    future::pending::<()>().await;
                }
            },
        )
        .await;
    }
}

async fn ha_identity(auth: &AuthService, context: Rc<Context>) -> Result<(), AdapterError> {
    let mut checked = None;
    let mut backoff = Backoff::new(1, 30);
    loop {
        let changed = context.credentials_changed.listen();
        let record = saved(auth, &context)?;
        let Some(authorization) = record.ha_authorization() else {
            checked = None;
            changed.await;
            continue;
        };
        let token = authorization.tokens.access_token.clone();
        let key = (
            record.uid.clone(),
            token.clone(),
            authorization.tokens.expires_at,
            authorization.tokens.refresh_at,
        );
        if !context.token_usable() || checked.as_ref() == Some(&key) {
            changed.await;
            continue;
        }
        let result = auth.validate_bridge_ha(&record).await;
        if !context.current_token(&token)
            || context
                .live
                .borrow()
                .credentials
                .as_ref()
                .is_none_or(|current| current.uid != record.uid)
        {
            continue;
        }
        match result {
            Ok(()) => {
                checked = Some(key);
                backoff = Backoff::new(1, 30);
                if !matches!(
                    context.status.borrow().auth.ha,
                    AuthenticationState::Refreshing
                ) {
                    context.ha_authentication(AuthenticationState::Authenticated);
                }
            }
            Err(AuthError::Cloud(error)) if error.is_unauthorized() => {
                checked = None;
                context.token_rejected(&token, FailureReason::Cloud(error))?;
            }
            Err(AuthError::Cloud(error)) => {
                let temporary = error.is_temporary();
                context.ha_authentication(AuthenticationState::Unavailable(FailureReason::Cloud(
                    error,
                )));
                if temporary {
                    let due = context.clock.now() + backoff.failed();
                    future::or(changed, context.clock.wait_until(due)).await;
                } else {
                    checked = Some(key);
                }
            }
            Err(AuthError::Failure(reason)) => {
                checked = Some(key);
                context.ha_authentication(AuthenticationState::Unavailable(reason));
            }
            Err(AuthError::Missing(_)) => continue,
            Err(error) => return Err(error.into()),
        }
    }
}

async fn mijia(auth: &AuthService, context: Rc<Context>) -> Result<(), AdapterError> {
    let mut backoff = Backoff::new(1, 30);
    let mut maintained_generation = None;
    let mut retry_due = None;
    let mut blocked = false;
    loop {
        let record = saved(auth, &context)?;
        let generation = context
            .live
            .borrow()
            .mijia_generation
            .clone()
            .ok_or_else(|| {
                AdapterError::Unavailable("Xiaomi account changed; restart after sign-in".into())
            })?;
        if maintained_generation.as_ref() != Some(&generation) {
            maintained_generation = Some(generation.clone());
            backoff = Backoff::new(1, 30);
            retry_due = None;
            blocked = false;
        }
        let changed = context.mijia_changed.listen();
        let credentials_changed = context.credentials_changed.listen();
        let rejected = context.mijia_rejected.borrow().as_ref() == Some(&generation);
        if record.mijia.is_some() && !rejected {
            future::or(changed, credentials_changed).await;
            continue;
        }
        let Some(recovery) = &record.recovery else {
            context.mijia_authentication(AuthenticationState::SignInRequired(
                FailureReason::MissingMijiaSession,
            ));
            future::or(changed, credentials_changed).await;
            continue;
        };
        if let Some(stop) = recovery.stop {
            context.status.borrow_mut().auth.recovery_stop = Some(stop);
            context.mijia_authentication(AuthenticationState::SignInRequired(
                FailureReason::RecoveryStopped(stop),
            ));
            future::or(changed, credentials_changed).await;
            continue;
        }
        if blocked {
            future::or(changed, credentials_changed).await;
            continue;
        }
        if let Some(due) = retry_due
            && context.clock.now() < due
        {
            future::or(
                future::or(changed, credentials_changed),
                context.clock.wait_until(due),
            )
            .await;
            continue;
        }
        retry_due = None;
        context.mijia_authentication(AuthenticationState::Restoring);
        match auth.recover_mijia(&record, &generation).await {
            Ok(_) => {
                saved(auth, &context)?;
                context.status.borrow_mut().auth.recovery_stop = None;
                context.mijia_authentication(AuthenticationState::Authenticated);
                context.request_sync();
                context.refresh_changed.notify(usize::MAX);
                backoff = Backoff::new(1, 30);
            }
            Err(AuthError::Missing(_)) => {
                saved(auth, &context)?;
            }
            Err(AuthError::Storage(error)) => return Err(error.into()),
            Err(error) => {
                let current = saved(auth, &context)?;
                if context.live.borrow().mijia_generation.as_deref() != Some(&generation) {
                    continue;
                }
                let stop = current.recovery.as_ref().and_then(|recovery| recovery.stop);
                context.status.borrow_mut().auth.recovery_stop = stop;
                let (reason, retry) = match error {
                    AuthError::Account(error) => {
                        let retry = matches!(
                            error.kind(),
                            AccountErrorKind::Network | AccountErrorKind::Timeout
                        );
                        (FailureReason::Account(error), retry)
                    }
                    AuthError::Cloud(error) => {
                        let retry = error.is_temporary();
                        (FailureReason::Cloud(error), retry)
                    }
                    AuthError::Failure(reason) => (reason, false),
                    other => return Err(other.into()),
                };
                if let Some(stop) = stop {
                    context.mijia_authentication(AuthenticationState::SignInRequired(
                        FailureReason::RecoveryStopped(stop),
                    ));
                } else {
                    context.mijia_authentication(AuthenticationState::Unavailable(reason));
                }
                if retry && stop.is_none() {
                    retry_due = Some(context.clock.now() + backoff.failed());
                } else {
                    blocked = true;
                }
            }
        }
    }
}

pub(super) async fn run(auth: &AuthService, context: Rc<Context>) -> Result<(), AdapterError> {
    future::or(
        mijia(auth, context.clone()),
        future::or(
            ha_identity(auth, context.clone()),
            future::or(tokens(auth, context.clone()), certificates(auth, context)),
        ),
    )
    .await
}
