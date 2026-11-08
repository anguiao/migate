#[derive(Default)]
pub(super) enum HaSync {
    #[default]
    Idle,
    // Retained during refresh so concurrent failures coalesce into the same attempt.
    Rejected(u64),
    Refreshed(u64),
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
    context.adopt_credentials(record.clone());
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

async fn ha_retired(context: &Context, key: &super::routing::HaKey) {
    loop {
        let changed = context.ha_changed.listen();
        if !context.current_ha(key) {
            return;
        }
        changed.await;
    }
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
        let changed = context.ha_changed.listen();
        let record = saved(auth, &context)?;
        let Some(key) = context.ha_key() else {
            if !matches!(
                context.status.borrow().auth.ha,
                AuthenticationState::SignInRequired(_)
            ) {
                context.ha_authentication(AuthenticationState::SignInRequired(
                    FailureReason::MissingHaAuthorization,
                ));
            }
            changed.await;
            continue;
        };
        let tokens = &record.ha_authorization().unwrap().tokens;
        let is_expired = tokens.expires_at <= context.clock.unix_now();
        if is_expired && observed_expiry != Some(tokens.expires_at) {
            observed_expiry = Some(tokens.expires_at);
            token_expired(&context);
        }
        let paused = matches!(*context.ha_sync.borrow(), HaSync::Paused);
        let rejected = matches!(&*context.ha_sync.borrow(), HaSync::Rejected(generation) if generation == &key.generation);
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
            context.ha_refresh_in_flight.set(true);
            context.ha_authentication(AuthenticationState::Refreshing);
            if matches!(&*context.ha_sync.borrow(), HaSync::Refreshed(generation) if generation == &key.generation)
            {
                *context.ha_sync.borrow_mut() = HaSync::Idle;
            }
            let refreshing = auth.refresh_bridge_tokens(&record, || context.clock.unix_now());
            let mut refreshing = std::pin::pin!(refreshing);
            let result = if !is_expired {
                let expiry = deadline(&context, tokens.expires_at);
                match future::or(async { Some(refreshing.as_mut().await) }, async {
                    context.clock.wait_until(expiry).await;
                    None
                })
                .await
                {
                    Some(result) => result,
                    None => {
                        observed_expiry = Some(tokens.expires_at);
                        token_expired(&context);
                        refreshing.await
                    }
                }
            } else {
                refreshing.await
            };
            context.ha_refresh_in_flight.set(false);
            match result {
                Ok(replacement) => {
                    let already_due = replacement
                        .ha_authorization()
                        .is_some_and(|ha| ha.tokens.refresh_at <= context.clock.unix_now());
                    context.adopt_credentials(replacement);
                    *context.ha_refresh_failure.borrow_mut() = None;
                    if let Some(fresh) = context.ha_key() {
                        *context.ha_sync.borrow_mut() = HaSync::Refreshed(fresh.generation);
                    }
                    context.ha_authentication(AuthenticationState::Checking);
                    context.ha_changed.notify(usize::MAX);
                    retry = already_due.then(|| context.clock.now() + Duration::from_secs(1));
                    backoff = Backoff::new(1, 30);
                }
                Err(AuthError::Cloud(error)) => {
                    if error.is_unauthorized() {
                        saved(auth, &context)?;
                        *context.ha_sync.borrow_mut() = HaSync::Paused;
                        context.ha_authentication(AuthenticationState::SignInRequired(
                            FailureReason::Cloud(error),
                        ));
                    } else {
                        let delay = if error.is_temporary() {
                            backoff.failed()
                        } else {
                            super::cloud::MEMBER_INTERVAL
                        };
                        let reason = FailureReason::Cloud(error);
                        *context.ha_refresh_failure.borrow_mut() = Some(reason.clone());
                        context.ha_authentication(AuthenticationState::Unavailable(reason));
                        retry = Some(context.clock.now() + delay);
                    }
                    eligibility_changed(&context, "cloud token maintenance failed");
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
        future::or(changed, async {
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
    let mut last_hub = None;
    let mut validity = None;
    let mut maintained = None;
    loop {
        let changed = context.changed.listen();
        let record = saved(auth, &context)?;
        if last_hub.as_ref() != Some(&record.hub) {
            let report = auth.local_status()?;
            validity = report.certificate;
            last_hub = Some(record.hub.clone());
            if let Some(validity) = validity {
                context.certificate(validity, CertificateUpdate::NotNeeded);
            } else {
                context.certificate_unavailable(report.certificate_update);
            }
        }
        let key = context.ha_key();
        if maintained != key {
            maintained = key.clone();
            due = context.clock.now();
            backoff = Backoff::new(1, 30);
        }
        let now = context.clock.unix_now();
        if let Some(validity) = validity
            && now >= validity.not_after
            && last_expiry != Some(validity.not_after)
        {
            last_expiry = Some(validity.not_after);
            eligibility_changed(&context, "hub certificate expired");
        }
        let needs_renewal = validity.is_none_or(|validity| validity.renewal_due(now));
        if needs_renewal && context.token_usable() && context.clock.now() >= due {
            let key = key.unwrap();
            let attempt = async {
                let renewal = auth
                    .renew_bridge_certificate(&record, key.generation, || context.clock.unix_now());
                let mut renewal = std::pin::pin!(renewal);
                let result = if let Some(validity) =
                    validity.filter(|validity| validity.currently_valid(now))
                {
                    match future::or(async { Some(renewal.as_mut().await) }, async {
                        context
                            .clock
                            .wait_until(deadline(&context, validity.not_after))
                            .await;
                        None
                    })
                    .await
                    {
                        Some(result) => result,
                        None => {
                            let current = saved(auth, &context)?;
                            if current.uid == record.uid && current.hub == record.hub {
                                last_expiry = Some(validity.not_after);
                                eligibility_changed(&context, "hub certificate expired");
                            }
                            renewal.await
                        }
                    }
                } else {
                    renewal.await
                };
                Ok::<_, AdapterError>(Some(result))
            };
            let result = future::or(attempt, async {
                ha_retired(&context, &key).await;
                Ok(None)
            })
            .await?;
            let Some(result) = result else {
                saved(auth, &context)?;
                continue;
            };
            match result {
                Ok(renewed) => {
                    if !context.current_ha(&key) {
                        saved(auth, &context)?;
                        continue;
                    }
                    context.adopt_credentials(renewed.credentials);
                    last_hub = Some(
                        context
                            .live
                            .borrow()
                            .credentials
                            .as_ref()
                            .unwrap()
                            .hub
                            .clone(),
                    );
                    let failed = matches!(renewed.update, CertificateUpdate::Failed(_));
                    validity = renewed.validity;
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
                Err(AuthError::Missing(_)) => {
                    saved(auth, &context)?;
                }
                Err(AuthError::Cloud(error)) => {
                    let current = saved(auth, &context)?;
                    if !context.current_ha(&key) {
                        continue;
                    }
                    last_hub = Some(current.hub);
                    let delay = if error.is_temporary() {
                        backoff.failed()
                    } else {
                        super::cloud::MEMBER_INTERVAL
                    };
                    if error.is_unauthorized() {
                        context.token_rejected(&key, FailureReason::Cloud(error.clone()))?;
                    }
                    let update = CertificateUpdate::Failed(FailureReason::Cloud(error));
                    if let Some(validity) = validity {
                        context.certificate(validity, update);
                    } else {
                        context.certificate_unavailable(update);
                    }
                    due = context.clock.now() + delay;
                }
                Err(AuthError::Failure(reason)) => {
                    if !context.current_ha(&key) {
                        continue;
                    }
                    let update = CertificateUpdate::Failed(reason);
                    if let Some(validity) = validity {
                        context.certificate(validity, update);
                    } else {
                        context.certificate_unavailable(update);
                    }
                    due = context.clock.now() + super::cloud::MEMBER_INTERVAL;
                }
                Err(error) => return Err(error.into()),
            }
            continue;
        }
        let expiry = validity
            .filter(|validity| now < validity.not_after)
            .map(|validity| deadline(&context, validity.not_after));
        let renewal = context.token_usable().then(|| match validity {
            Some(validity) => due.max(deadline(&context, validity.renewal_at())),
            None => due,
        });
        let next = expiry.into_iter().chain(renewal).min();
        future::or(changed, async {
            if let Some(next) = next {
                context.clock.wait_until(next).await;
            } else {
                future::pending::<()>().await;
            }
        })
        .await;
    }
}

async fn ha_identity(auth: &AuthService, context: Rc<Context>) -> Result<(), AdapterError> {
    let mut checked = None;
    let mut maintained = None;
    let mut backoff = Backoff::new(1, 30);
    let mut retry = None;
    let mut identity_failure: Option<FailureReason> = None;
    loop {
        let changed = context.ha_changed.listen();
        let record = saved(auth, &context)?;
        let Some(key) = context.ha_key() else {
            maintained = None;
            checked = None;
            changed.await;
            continue;
        };
        if maintained.as_ref() != Some(&key) {
            maintained = Some(key.clone());
            checked = None;
            retry = None;
            identity_failure = None;
            backoff = Backoff::new(1, 30);
        }
        if !context.token_available() || checked.as_ref() == Some(&key) {
            changed.await;
            continue;
        }
        if let Some(due) = retry
            && context.clock.now() < due
        {
            future::or(changed, context.clock.wait_until(due)).await;
            continue;
        }
        retry = None;
        let result = future::or(
            async { Some(auth.validate_bridge_ha(&record).await) },
            async {
                ha_retired(&context, &key).await;
                None
            },
        )
        .await;
        let Some(result) = result else {
            saved(auth, &context)?;
            continue;
        };
        if !context.current_ha(&key) {
            saved(auth, &context)?;
            continue;
        }
        if !context.token_available() {
            continue;
        }
        match result {
            Ok(()) => {
                checked = Some(key.clone());
                backoff = Backoff::new(1, 30);
                *context.ha_verified.borrow_mut() = Some(key);
                let should_report = {
                    let status = context.status.borrow();
                    matches!(status.auth.ha, AuthenticationState::Checking)
                        || identity_failure.as_ref().is_some_and(|reason| {
                            status.auth.ha == AuthenticationState::Unavailable(reason.clone())
                        })
                };
                if should_report {
                    let authentication = if context.ha_refresh_in_flight.get() {
                        AuthenticationState::Refreshing
                    } else {
                        match context.ha_refresh_failure.borrow().clone() {
                            Some(reason) => AuthenticationState::Unavailable(reason),
                            None => AuthenticationState::Authenticated,
                        }
                    };
                    context.ha_authentication(authentication);
                }
                identity_failure = None;
                context.changed.notify(usize::MAX);
                context.ha_changed.notify(usize::MAX);
            }
            Err(AuthError::Cloud(error)) if error.is_unauthorized() => {
                context.token_rejected(&key, FailureReason::Cloud(error))?;
            }
            Err(AuthError::Cloud(error)) => {
                let temporary = error.is_temporary();
                let reason = FailureReason::Cloud(error);
                identity_failure = Some(reason.clone());
                context.ha_authentication(AuthenticationState::Unavailable(reason));
                if temporary {
                    retry = Some(context.clock.now() + backoff.failed());
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
    let mut retry_due = None;
    let mut blocked = false;
    loop {
        let record = saved(auth, &context)?;
        let generation = context
            .live
            .borrow()
            .mijia_generation
            .expect("active account");
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
        match auth.recover_mijia(&record).await {
            Ok(_) => {
                saved(auth, &context)?;
                context.status.borrow_mut().auth.recovery_stop = None;
                context.mijia_authentication(AuthenticationState::Authenticated);
                context.request_sync();
                context.refresh_changed.notify(usize::MAX);
                backoff = Backoff::new(1, 30);
            }
            Err(AuthError::Storage(error)) => return Err(error.into()),
            Err(error) => {
                let current = saved(auth, &context)?;
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
