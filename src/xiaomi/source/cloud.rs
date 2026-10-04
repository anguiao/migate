use super::{
    model::SourceError,
    state::{Backoff, Context, Lifetime},
};
use crate::{
    storage::CachedScope,
    xiaomi::{
        auth::{AuthService, AuthenticationState, FailureReason},
        catalog::{self, CatalogError},
        cloud::{CloudClient, CloudError, CloudErrorKind},
        discovery::Discovery,
    },
};
use futures_lite::future;
use std::{collections::BTreeSet, rc::Rc, time::Duration};
pub(super) const MEMBER_INTERVAL: Duration = Duration::from_secs(300);
fn transient(error: &CloudError) -> bool {
    matches!(
        error.kind(),
        CloudErrorKind::Network
            | CloudErrorKind::Timeout
            | CloudErrorKind::HttpStatus(408 | 429 | 500..=599)
    )
}
pub(super) async fn run(
    auth: &AuthService,
    cloud: &CloudClient,
    discovery: &Discovery,
    context: Rc<Context>,
) -> Result<(), SourceError> {
    let mut backoff = Backoff::new();
    let mut due = context.clock.now();
    loop {
        let listener = context.sync_changed.listen();
        if context.cloud_paused.get() {
            future::pending::<()>().await;
        }
        if !context.sync_requested.replace(false) && context.clock.now() < due {
            future::or(context.clock.wait_until(due), async {
                listener.await;
            })
            .await;
            continue;
        }
        let stamp = {
            let live = context.live.borrow();
            (
                live.credentials.as_ref().map(|record| record.uid.clone()),
                live.scope.clone(),
                live.generation(),
            )
        };
        let rejected = context.rejected_token.borrow_mut().take();
        *context.refreshing_token.borrow_mut() = rejected.clone();
        let preparation = if let Some(token) = &rejected {
            auth.prepare_bridge_after_unauthorized(token).await?
        } else {
            auth.prepare_bridge().await?
        };
        context.refreshing_token.borrow_mut().take();
        let fresh = preparation
            .credentials
            .as_ref()
            .map(|record| record.tokens.access_token.clone());
        let late_rejected = context.rejected_token.borrow().clone();
        if matches!(
            preparation.report.authentication,
            AuthenticationState::Authenticated
        ) && (rejected.is_some()
            || fresh
                .as_ref()
                .is_some_and(|token| late_rejected.as_ref().is_some_and(|old| old != token)))
        {
            *context.refreshed_token.borrow_mut() = fresh.clone();
            if late_rejected
                .as_ref()
                .is_some_and(|old| Some(old) != fresh.as_ref())
            {
                context.rejected_token.borrow_mut().take();
            }
        } else if matches!(
            preparation.report.authentication,
            AuthenticationState::Unavailable(_)
        ) && rejected.is_some()
        {
            *context.rejected_token.borrow_mut() = rejected;
        }
        if context
            .refreshed_token
            .borrow()
            .as_ref()
            .is_some_and(|token| Some(token) != fresh.as_ref())
        {
            context.refreshed_token.borrow_mut().take();
        }
        if context.cloud_paused.get() {
            let renewed = matches!(
                preparation.report.authentication,
                AuthenticationState::Authenticated
            ) && preparation.credentials.as_ref().is_some_and(|fresh| {
                context
                    .live
                    .borrow()
                    .credentials
                    .as_ref()
                    .is_some_and(|old| {
                        old.uid == fresh.uid && old.tokens.access_token != fresh.tokens.access_token
                    })
            });
            if !renewed {
                continue;
            }
            context.cloud_paused.set(false);
        }
        let current = {
            let live = context.live.borrow();
            (
                live.credentials.as_ref().map(|record| record.uid.clone()),
                live.scope.clone(),
                live.generation(),
            )
        };
        if current != stamp {
            context.request_sync();
            continue;
        }
        super::state::update_auth(&context, &preparation);
        let Some(credentials) = preparation.credentials else {
            return Err(SourceError::Unavailable(
                "Xiaomi sign-in is required".into(),
            ));
        };
        context.ready.finish(Ok(context.registry.clone()));
        if matches!(
            preparation.report.authentication,
            AuthenticationState::SignInRequired(_)
        ) {
            context.cloud_paused.set(true);
            continue;
        }
        if !matches!(
            preparation.report.authentication,
            AuthenticationState::Authenticated
        ) {
            if let AuthenticationState::Unavailable(reason) = &preparation.report.authentication {
                context.set_catalog_problem(Some(reason.to_string()));
            }
            let retry = matches!(&preparation.report.authentication,AuthenticationState::Unavailable(FailureReason::Cloud(error)) if transient(error))
                || matches!(&preparation.report.certificate_update,crate::xiaomi::auth::CertificateUpdate::Failed(FailureReason::Cloud(error)) if transient(error));
            due = context.clock.now()
                + if retry {
                    backoff.failed(false)
                } else {
                    MEMBER_INTERVAL
                };
            continue;
        }
        let selected = {
            let live = context.live.borrow();
            if let Some(scope) = &live.scope {
                if scope.uid != credentials.uid {
                    Err(CatalogError::InvalidScope)
                } else {
                    catalog::current_home(&preparation.homes, scope)
                }
            } else {
                catalog::select_local_home(
                    &discovery.snapshot(),
                    &preparation.homes,
                    &credentials.uid,
                )
            }
        };
        let home = match selected {
            Ok(home) => home,
            Err(error) => {
                context.set_catalog_problem(Some(error.to_string()));
                due = context.clock.now() + backoff.failed(false);
                continue;
            }
        };
        let scope = CachedScope {
            uid: home.uid.clone(),
            home_id: home.home_id.clone(),
            group_id: home.group_id.clone(),
        };
        context.store.devices().save_scope(&scope)?;
        let members = home.dids.iter().cloned().collect::<BTreeSet<_>>();
        let removed = {
            let live = context.live.borrow();
            live.members
                .difference(&members)
                .cloned()
                .collect::<Vec<_>>()
        };
        context.live.borrow_mut().scope = Some(scope.clone());
        context.live.borrow_mut().members = members.clone();
        for did in removed {
            context.remove(&did)?;
        }
        context.set_catalog_problem(None);
        context.changed.notify(usize::MAX);
        super::local::refresh_access(&context);
        for did in members {
            let prepare = {
                let live = context.live.borrow();
                if live.loading.contains_key(&did) {
                    false
                } else if let Some(record) = live.records.get(&did) {
                    record.blocked.is_some()
                        || live
                            .access
                            .get(&did)
                            .and_then(|access| access.as_ref().ok())
                            .is_some_and(|access| {
                                access.model != record.cached.model
                                    || access.instance_type != record.cached.instance_type
                            })
                } else {
                    live.descriptions.get(&did).is_none_or(|cached| {
                        live.access
                            .get(&did)
                            .and_then(|access| access.as_ref().ok())
                            .is_some_and(|access| {
                                access.model != cached.model
                                    || access.instance_type != cached.instance_type
                            })
                    })
                }
            };
            if !prepare {
                context.reconcile(&did);
                continue;
            }
            let life = Rc::new(Lifetime::default());
            context
                .live
                .borrow_mut()
                .loading
                .insert(did.clone(), life.clone());
            let current = context.clone();
            let client = cloud.clone();
            let credentials = credentials.clone();
            let home = home.clone();
            let task = async move {
                let result = future::or(
                    async { Some(catalog::load_device(&client, &credentials, &home, &did).await) },
                    async {
                        life.ended().await;
                        None
                    },
                )
                .await;
                if life.closed.get() {
                    return Ok(());
                }
                let valid = {
                    let live = current.live.borrow();
                    live.scope.as_ref() == Some(&scope_for(&home))
                        && live.members.contains(&did)
                        && live
                            .credentials
                            .as_ref()
                            .is_some_and(|record| record.uid == home.uid)
                };
                current.live.borrow_mut().loading.remove(&did);
                if !valid {
                    return Ok(());
                }
                match result.unwrap() {
                    Ok(cached) => {
                        if let Some(record) = current.live.borrow_mut().records.get_mut(&did) {
                            record.blocked = None;
                        }
                        current
                            .live
                            .borrow_mut()
                            .descriptions
                            .insert(did.clone(), cached);
                        current.reconcile(&did);
                    }
                    Err(error) => {
                        match &error {
                            CatalogError::Cloud(_) => {
                                current.problem(&did, Some(error.to_string()))
                            }
                            _ => current.block_device(&did, error.to_string()),
                        }
                        if let CatalogError::Cloud(error) = &error
                            && error.is_unauthorized()
                            && current.store.xiaomi().load()?.is_some_and(|record| {
                                record.uid == credentials.uid
                                    && record.tokens.access_token == credentials.tokens.access_token
                            })
                        {
                            if current.refreshed_token.borrow().as_ref()
                                == Some(&credentials.tokens.access_token)
                            {
                                current.cloud_paused.set(true);
                                current.status.borrow_mut().authentication.authentication =
                                    AuthenticationState::SignInRequired(FailureReason::Cloud(
                                        error.clone(),
                                    ));
                                current.status_changed.notify(usize::MAX);
                            } else {
                                let token = &credentials.tokens.access_token;
                                if current.rejected_token.borrow().as_ref() != Some(token)
                                    && current.refreshing_token.borrow().as_ref() != Some(token)
                                {
                                    *current.rejected_token.borrow_mut() = Some(token.clone());
                                    current.request_sync();
                                }
                            }
                        }
                    }
                }
                Ok(())
            };
            context.tasks.borrow_mut().push(Box::pin(task));
            context.changed.notify(usize::MAX);
        }
        let retry_certificate = matches!(&preparation.report.certificate_update,crate::xiaomi::auth::CertificateUpdate::Failed(FailureReason::Cloud(error)) if transient(error));
        due = context.clock.now()
            + if retry_certificate {
                backoff.failed(false)
            } else {
                backoff = Backoff::new();
                MEMBER_INTERVAL
            };
    }
}
fn scope_for(home: &crate::xiaomi::cloud::OwnedHome) -> CachedScope {
    CachedScope {
        uid: home.uid.clone(),
        home_id: home.home_id.clone(),
        group_id: home.group_id.clone(),
    }
}
