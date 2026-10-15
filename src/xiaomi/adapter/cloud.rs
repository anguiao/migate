use super::{
    model::AdapterError,
    state::{Backoff, Context, Lifetime},
};
use crate::{
    storage::CachedScope,
    xiaomi::{
        auth::{AuthenticationState, FailureReason},
        catalog::{self, CatalogError},
        cloud::CloudClient,
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
pub(super) async fn run(
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
        if !context.token_usable() {
            let credentials = context.credentials_changed.listen();
            future::or(
                async {
                    listener.await;
                },
                async {
                    credentials.await;
                },
            )
            .await;
            continue;
        }
        let credentials = context.live.borrow().credentials.as_ref().unwrap().clone();
        context.ready.finish(Ok(context.registry.clone()));
        let token = &credentials.tokens.access_token;
        let Some(homes) = future::or(async { Some(cloud.get_owned_homes(token).await) }, async {
            context.token_retired(token).await;
            None
        })
        .await
        else {
            continue;
        };
        if !context.current_token(token) {
            continue;
        }
        let homes = match homes {
            Ok(homes) => homes,
            Err(error) => {
                context.set_catalog_problem(Some(error.to_string()));
                context.cloud_error(token, None, &error)?;
                if !matches!(*context.cloud_sync.borrow(), CloudSync::Paused) {
                    context.authentication(if error.is_unauthorized() {
                        AuthenticationState::Checking
                    } else {
                        AuthenticationState::Unavailable(FailureReason::Cloud(error))
                    });
                }
                due = context
                    .sync_not_before
                    .get()
                    .unwrap_or_else(|| context.sync_failed());
                continue;
            }
        };
        let identity_failure = match homes.uid.as_deref() {
            None => Some(FailureReason::CannotDetermineAccountIdentity),
            Some(uid) if uid != credentials.uid => Some(FailureReason::AccountIdentityMismatch),
            _ => None,
        };
        if let Some(reason) = identity_failure {
            context.set_catalog_problem(Some(reason.to_string()));
            context.authentication(AuthenticationState::Unavailable(reason));
            context.cloud_scope_valid.set(false);
            context.update_all_availability();
            context.update_routing("cloud account identity invalidated");
            context.changed.notify(usize::MAX);
            due = context.clock.now() + MEMBER_INTERVAL;
            continue;
        }
        let expected_scope = context.live.borrow().scope.clone();
        let selected = {
            let live = context.live.borrow();
            if let Some(scope) = &live.scope {
                if scope.uid != credentials.uid {
                    Err(CatalogError::InvalidScope)
                } else {
                    catalog::current_home(&homes.homes, scope)
                }
            } else {
                catalog::select_local_home(&discovery.snapshot(), &homes.homes, &credentials.uid)
            }
        };
        let home = match selected {
            Ok(home) => home,
            Err(error) => {
                context.set_catalog_problem(Some(error.to_string()));
                context.cloud_scope_valid.set(false);
                context.update_all_availability();
                context.update_routing("cloud home scope invalidated");
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
        let Some(infos) = future::or(
            async { Some(cloud.get_home_devices(token, &home, &requested).await) },
            async {
                context.token_retired(token).await;
                None
            },
        )
        .await
        else {
            continue;
        };
        if !context.current_token(token) || context.live.borrow().scope != expected_scope {
            continue;
        }
        let infos = match infos {
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
        let membership_changed = context
            .live
            .borrow()
            .devices
            .iter()
            .filter(|(_, slot)| slot.group.is_none())
            .map(|(did, _)| did)
            .ne(members.iter());
        let removed = {
            let live = context.live.borrow();
            live.devices
                .iter()
                .filter(|(did, slot)| slot.group.is_none() && !members.contains(*did))
                .map(|(did, _)| did.clone())
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
                // Native groups keep their own catalog access metadata; an
                // ordinary home detail absence must never clear it.
                if slot.group.is_some() {
                    continue;
                }
                slot.cloud_access = infos.get(did).cloned();
                slot.cloud_forbidden = false;
            }
        }
        context.authentication(AuthenticationState::Authenticated);
        context.cloud_scope_valid.set(true);
        context.http_paused.set(false);
        context.sync_not_before.set(None);
        *context.sync_backoff.borrow_mut() = Backoff::new(1, 30);
        context.update_all_availability();
        context.update_routing("complete cloud directory and device information synchronized");
        context.set_catalog_problem(None);
        context.changed.notify(usize::MAX);
        if membership_changed {
            super::local::refresh_access(&context);
        }
        let catalog_members = members.clone();
        for did in members {
            // Group-like records are admitted only by a confirmed catalog
            // relation, never through the ordinary home description path.
            let group_like = {
                let live = context.live.borrow();
                live.devices[&did].group.is_some()
                    || crate::xiaomi::capability::is_light_group(
                        &crate::device::DeviceKey::new("xiaomi", &did),
                        live.devices[&did]
                            .cloud_access
                            .as_ref()
                            .map(|access| access.model.as_str())
                            .unwrap_or_default(),
                    )
            };
            if group_like {
                continue;
            }
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
                    Ok(candidate) => {
                        {
                            let mut live = current.live.borrow_mut();
                            let slot = live.devices.get_mut(&did).unwrap();
                            slot.blocked = None;
                            slot.candidate = Some(candidate);
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
        // Native group relations come from the same authoritative account
        // catalog; a failed read keeps the previous confirmed relations.
        // The selected scope is captured at request start so the first home
        // binding (previously `None`) cannot discard the first catalog.
        let catalog_scope = context.live.borrow().scope.clone();
        let catalog = future::or(
            async { Some(cloud.get_device_catalog(token).await) },
            async {
                context.token_retired(token).await;
                None
            },
        )
        .await;
        if context.current_token(token) && context.live.borrow().scope == catalog_scope {
            match catalog {
                Some(Ok(catalog)) => {
                    let update =
                        context
                            .relations
                            .borrow_mut()
                            .apply(&catalog, &scope, &catalog_members);
                    for did in &update.withdrawn {
                        if !catalog_members.contains(did)
                            && context.live.borrow().devices.contains_key(did)
                        {
                            context.remove(did)?;
                        }
                    }
                    for (did, reason) in &update.errors {
                        log::warn!("Native group {did} relation not confirmed: {reason}");
                    }
                    for group in &update.active {
                        log::debug!(
                            "Native group {} model {} instance {} confirmed with {} members",
                            group.membership.group_did,
                            group.model,
                            group.instance_type.as_deref().unwrap_or("missing"),
                            group.membership.members.len()
                        );
                    }
                    context.sync_member_watches(false);
                    context.sync_groups(cloud, token, &update.active);
                }
                Some(Err(error)) => {
                    log::warn!(
                        "Native group catalog read failed; keeping confirmed relations: {error}"
                    );
                    if error.is_unauthorized() {
                        context.cloud_error(token, None, &error)?;
                    }
                }
                None => {}
            }
        }
        due = context.clock.now() + MEMBER_INTERVAL;
    }
}
