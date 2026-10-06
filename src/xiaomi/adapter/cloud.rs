use super::{
    model::AdapterError,
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
use std::{
    collections::{BTreeMap, BTreeSet},
    rc::Rc,
    time::Duration,
};

#[derive(Default)]
pub(super) enum CloudSync {
    #[default]
    Idle,
    // Retained during refresh so concurrent failures coalesce into the same attempt.
    Rejected(String),
    Refreshed(String),
    Paused,
}
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
) -> Result<(), AdapterError> {
    let sync = &context.cloud_sync;
    let mut due = context.clock.now();
    loop {
        let listener = context.sync_changed.listen();
        if matches!(*sync.borrow(), CloudSync::Paused) {
            future::pending::<()>().await;
        }
        if let Some(floor) = context.sync_not_before.get()
            && context.clock.now() < floor
        {
            context.clock.wait_until(floor).await;
            continue;
        }
        context.sync_not_before.set(None);
        if !context.sync_requested.replace(false) && context.clock.now() < due {
            future::or(context.clock.wait_until(due), async {
                listener.await;
            })
            .await;
            continue;
        }
        let rejected = match &*sync.borrow() {
            CloudSync::Rejected(token) => Some(token.clone()),
            _ => None,
        };
        let preparation = if let Some(token) = &rejected {
            auth.prepare_bridge_after_unauthorized(token).await?
        } else {
            auth.prepare_bridge().await?
        };
        let fresh = preparation
            .credentials
            .as_ref()
            .map(|record| &record.tokens.access_token);
        let authenticated = matches!(
            preparation.report.authentication,
            AuthenticationState::Authenticated
        );
        {
            let mut state = sync.borrow_mut();
            match &*state {
                CloudSync::Rejected(token)
                    if authenticated && (rejected.is_some() || Some(token) != fresh) =>
                {
                    *state = CloudSync::Refreshed(fresh.unwrap().clone());
                }
                CloudSync::Refreshed(token) if Some(token) != fresh => {
                    *state = CloudSync::Idle;
                }
                CloudSync::Paused => {
                    let renewed = authenticated
                        && context
                            .live
                            .borrow()
                            .credentials
                            .as_ref()
                            .is_some_and(|old| Some(&old.tokens.access_token) != fresh);
                    if !renewed {
                        continue;
                    }
                    *state = CloudSync::Idle;
                }
                _ => {}
            }
        }
        super::state::update_auth(&context, &preparation);
        let Some(credentials) = preparation.credentials else {
            return Err(AdapterError::Unavailable(
                "Xiaomi sign-in is required".into(),
            ));
        };
        context.ready.finish(Ok(context.registry.clone()));
        if matches!(
            preparation.report.authentication,
            AuthenticationState::SignInRequired(_)
        ) {
            *sync.borrow_mut() = CloudSync::Paused;
            context.update_all_availability();
            continue;
        }
        if !authenticated {
            if let AuthenticationState::Unavailable(reason) = &preparation.report.authentication {
                context.set_catalog_problem(Some(reason.to_string()));
                match reason {
                    FailureReason::AccountIdentityMismatch
                    | FailureReason::CannotDetermineAccountIdentity => {
                        context.cloud_scope_valid.set(false);
                        context.update_all_availability();
                    }
                    FailureReason::Cloud(error) if error.is_temporary() => {
                        context.cloud_error(&credentials.tokens.access_token, None, error)?;
                    }
                    _ => {}
                }
            }
            let retry = matches!(&preparation.report.authentication,AuthenticationState::Unavailable(FailureReason::Cloud(error)) if transient(error))
                || matches!(&preparation.report.certificate_update,crate::xiaomi::auth::CertificateUpdate::Failed(FailureReason::Cloud(error)) if transient(error));
            due = if retry {
                context
                    .sync_not_before
                    .get()
                    .unwrap_or_else(|| context.sync_failed())
            } else {
                context.clock.now() + MEMBER_INTERVAL
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
                context.cloud_scope_valid.set(false);
                context.update_all_availability();
                due = context.sync_failed();
                continue;
            }
        };
        let requested = {
            let live = context.live.borrow();
            live.devices
                .iter()
                .filter(|(did, slot)| {
                    home.dids.contains(did) && (slot.admitted.is_some() || slot.candidate.is_some())
                })
                .map(|(did, _)| did.clone())
                .collect::<Vec<_>>()
        };
        let infos = match cloud
            .get_home_devices(&credentials.tokens.access_token, &home, &requested)
            .await
        {
            Ok(infos) => infos
                .into_iter()
                .map(|info| (info.did.clone(), info))
                .collect::<BTreeMap<_, _>>(),
            Err(error) => {
                context.set_catalog_problem(Some(error.to_string()));
                context.cloud_error(&credentials.tokens.access_token, None, &error)?;
                due = context
                    .sync_not_before
                    .get()
                    .unwrap_or_else(|| context.sync_failed());
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
        let membership_changed = context.live.borrow().devices.keys().ne(members.iter());
        let removed = {
            let live = context.live.borrow();
            live.devices
                .keys()
                .filter(|did| !members.contains(*did))
                .cloned()
                .collect::<Vec<_>>()
        };
        context.live.borrow_mut().scope = Some(scope.clone());
        for did in removed {
            context.remove(&did)?;
        }
        for did in &members {
            context
                .live
                .borrow_mut()
                .devices
                .entry(did.clone())
                .or_default();
        }
        {
            let mut live = context.live.borrow_mut();
            for (did, slot) in &mut live.devices {
                slot.cloud_access = infos.get(did).cloned();
                slot.cloud_forbidden = false;
            }
        }
        context.cloud_scope_valid.set(true);
        context.http_paused.set(false);
        context.sync_not_before.set(None);
        *context.sync_backoff.borrow_mut() = Backoff::new(1, 30);
        context.update_all_availability();
        context.set_catalog_problem(None);
        context.changed.notify(usize::MAX);
        if membership_changed {
            super::local::refresh_access(&context);
        }
        for did in members {
            let prepare = context.live.borrow().devices[&did].needs_description();
            if !prepare {
                context.reconcile(&did)?;
                continue;
            }
            let life = Rc::new(Lifetime::default());
            context
                .live
                .borrow_mut()
                .devices
                .get_mut(&did)
                .unwrap()
                .loading = Some(life.clone());
            let current = context.clone();
            let client = cloud.clone();
            let credentials = credentials.clone();
            let home = home.clone();
            let task = async move {
                let Some(result) = future::or(
                    async {
                        life.ended().await;
                        None
                    },
                    async { Some(catalog::load_device(&client, &credentials, &home, &did).await) },
                )
                .await
                else {
                    return Ok(());
                };
                current
                    .live
                    .borrow_mut()
                    .devices
                    .get_mut(&did)
                    .unwrap()
                    .loading = None;
                match result {
                    Ok(cached) => {
                        {
                            let mut live = current.live.borrow_mut();
                            let slot = live.devices.get_mut(&did).unwrap();
                            slot.blocked = None;
                            slot.candidate = Some(cached);
                        }
                        current.reconcile(&did)?;
                        current.request_sync();
                    }
                    Err(error) => {
                        match &error {
                            CatalogError::Cloud(_) => {
                                current.problem(&did, Some(error.to_string()))
                            }
                            _ => current.block_device(&did, error.to_string()),
                        }
                        if let CatalogError::Cloud(error) = &error {
                            current.cloud_error(
                                &credentials.tokens.access_token,
                                Some(&did),
                                error,
                            )?;
                        }
                    }
                }
                Ok(())
            };
            context.tasks.spawn(task);
            context.changed.notify(usize::MAX);
        }
        let retry_certificate = matches!(&preparation.report.certificate_update,crate::xiaomi::auth::CertificateUpdate::Failed(FailureReason::Cloud(error)) if transient(error));
        due = context.clock.now()
            + if retry_certificate {
                context.sync_backoff.borrow_mut().failed()
            } else {
                *context.sync_backoff.borrow_mut() = Backoff::new(1, 30);
                MEMBER_INTERVAL
            };
    }
}
