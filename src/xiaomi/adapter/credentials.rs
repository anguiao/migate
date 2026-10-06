use super::{
    cloud::CloudSync,
    model::AdapterError,
    state::{Backoff, Context},
};
use crate::{
    storage::XiaomiRecord,
    xiaomi::auth::{AuthError, AuthService, AuthenticationState, CertificateUpdate, FailureReason},
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
    context.adopt_credentials(record.clone());
    Ok(record)
}

fn eligibility_changed(context: &Context, reason: &str) {
    context
        .status
        .borrow_mut()
        .authentication
        .set_completed_at(context.clock.unix_now());
    context.update_all_availability();
    context.update_routing(reason);
    context.credentials_changed.notify(usize::MAX);
    context.changed.notify(usize::MAX);
    context.refresh_changed.notify(usize::MAX);
    context.status_changed.notify(usize::MAX);
}

fn token_expired(context: &Context) {
    if !matches!(*context.cloud_sync.borrow(), CloudSync::Paused) {
        context.authentication(AuthenticationState::Unavailable(
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
        let sync_changed = context.sync_changed.listen();
        let record = saved(auth, &context)?;
        let key = (record.tokens.access_token.clone(), record.tokens.expires_at);
        let is_expired = record.tokens.expires_at <= context.clock.unix_now();
        if is_expired && observed_expiry.as_ref() != Some(&key) {
            observed_expiry = Some(key);
            token_expired(&context);
        }
        let paused = matches!(*context.cloud_sync.borrow(), CloudSync::Paused);
        let rejected = matches!(&*context.cloud_sync.borrow(), CloudSync::Rejected(token) if token == &record.tokens.access_token);
        let refresh_due = retry.unwrap_or_else(|| {
            if rejected {
                context.clock.now()
            } else {
                deadline(&context, record.tokens.refresh_at)
            }
        });
        if !paused
            && (rejected || context.clock.unix_now() >= record.tokens.refresh_at)
            && context.clock.now() >= refresh_due
        {
            let refreshing = auth.refresh_bridge_tokens(&record, || context.clock.unix_now());
            let mut refreshing = std::pin::pin!(refreshing);
            let result = if !is_expired {
                let expiry = deadline(&context, record.tokens.expires_at);
                match future::or(async { Some(refreshing.as_mut().await) }, async {
                    context.clock.wait_until(expiry).await;
                    observed_expiry =
                        Some((record.tokens.access_token.clone(), record.tokens.expires_at));
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
                    let already_due = replacement.tokens.refresh_at <= context.clock.unix_now();
                    *context.cloud_sync.borrow_mut() =
                        CloudSync::Refreshed(replacement.tokens.access_token.clone());
                    context.adopt_credentials(replacement);
                    context.authentication(AuthenticationState::Checking);
                    context.request_sync();
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
                        *context.cloud_sync.borrow_mut() = CloudSync::Paused;
                        context.authentication(AuthenticationState::SignInRequired(
                            FailureReason::Cloud(error),
                        ));
                    } else {
                        context.authentication(AuthenticationState::Unavailable(
                            FailureReason::Cloud(error),
                        ));
                    }
                    // A maintenance network failure does not suspend a still-valid, unrejected token.
                    retry = Some(context.clock.now() + delay);
                    eligibility_changed(&context, "cloud token maintenance failed");
                }
                Err(error) => return Err(error.into()),
            }
            continue;
        }
        let next = (!paused)
            .then_some(refresh_due)
            .into_iter()
            .chain((!is_expired).then(|| deadline(&context, record.tokens.expires_at)))
            .min();
        future::or(
            async {
                credentials_changed.await;
            },
            future::or(
                async {
                    sync_changed.await;
                },
                async {
                    if let Some(next) = next {
                        context.clock.wait_until(next).await;
                    } else {
                        future::pending::<()>().await;
                    }
                },
            ),
        )
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
        let validity = auth.local_status()?.certificate.unwrap();
        let now = context.clock.unix_now();
        if now >= validity.not_after && last_expiry != Some(validity.not_after) {
            last_expiry = Some(validity.not_after);
            eligibility_changed(&context, "local gateway certificate expired");
        }
        if validity.renewal_due(now) && context.cloud_authenticated() && context.clock.now() >= due
        {
            let renewal = auth.renew_bridge_certificate(&record, || context.clock.unix_now());
            let mut renewal = std::pin::pin!(renewal);
            let result = if validity.currently_valid(now) {
                match future::or(async { Some(renewal.as_mut().await) }, async {
                    context
                        .clock
                        .wait_until(deadline(&context, validity.not_after))
                        .await;
                    last_expiry = Some(validity.not_after);
                    eligibility_changed(&context, "local gateway certificate expired");
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
                    context.adopt_credentials(renewed.credentials);
                    let failed = matches!(renewed.update, CertificateUpdate::Failed(_));
                    context.certificate(renewed.validity, renewed.update);
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
                    if error.is_unauthorized() {
                        context.token_rejected(
                            &record.tokens.access_token,
                            FailureReason::Cloud(error.clone()),
                        )?;
                    }
                    let stale = !context.current_token(&record.tokens.access_token);
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
            .cloud_authenticated()
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

pub(super) async fn run(auth: &AuthService, context: Rc<Context>) -> Result<(), AdapterError> {
    future::or(tokens(auth, context.clone()), certificates(auth, context)).await
}
