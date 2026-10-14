use super::{
    model::AdapterError,
    state::{Backoff, Context, GroupMeta, Lifetime},
};
use crate::{
    storage::{CachedScope, XiaomiRecord},
    xiaomi::{
        auth::{AuthenticationState, FailureReason},
        catalog::{self, CatalogEntry, CatalogError},
        cloud::{CloudClient, OwnedHome},
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

/// Group identities are known from the DID convention, a classified relation,
/// or an authoritative published composition restored from storage.
fn known_group_identity(context: &Context, did: &str, published: &BTreeSet<String>) -> bool {
    did.starts_with("group.")
        || published.contains(did)
        || context.live.borrow().devices[did].group.is_some()
}

fn published_group_identities(context: &Context) -> BTreeSet<String> {
    context
        .registry
        .snapshot()
        .applied
        .groups
        .into_iter()
        .map(|group| group.key.id)
        .collect()
}
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
        // A full sync invalidates relation freshness for every known group;
        // only a successful read in this cycle may count as a fresh confirmed
        // replacement, while the last confirmed relation stays authoritative.
        {
            let mut live = context.live.borrow_mut();
            for slot in live.devices.values_mut() {
                if let Some(meta) = &mut slot.group {
                    meta.relation = None;
                }
            }
        }
        let scope = CachedScope {
            uid: home.uid.clone(),
            home_id: home.home_id.clone(),
            group_id: home.group_id.clone(),
        };
        context.store.devices().save_scope(&scope)?;
        let members = home.dids.iter().cloned().collect::<BTreeSet<_>>();
        let membership_changed = context.live.borrow().devices.keys().ne(members.iter());
        let removed_keys = {
            let live = context.live.borrow();
            live.devices
                .keys()
                .filter(|did| !members.contains(*did))
                .map(|did| crate::device::DeviceKey::new("xiaomi", did.clone()))
                .collect::<Vec<_>>()
        };
        context.live.borrow_mut().scope = Some(scope.clone());
        context.live.borrow_mut().complete = true;
        // Revoke confirmed whole-home deletions before device-detail reads.
        // Their durable removal is committed with the pending configuration.
        if !removed_keys.is_empty() {
            context.reconcile_groups_removed(&removed_keys)?;
        }
        for did in &members {
            context
                .live
                .borrow_mut()
                .devices
                .entry(did.clone())
                .or_default();
        }
        // Group metadata refreshes run before the ordinary access batch: one
        // malformed or failed device-detail batch must not block unrelated
        // group relations from being refreshed on their normal schedule. A
        // restored published composition is authoritative for numeric group
        // identities before their slot metadata is populated.
        let published_groups = published_group_identities(&context);
        for did in &members {
            let group_identity = known_group_identity(&context, did, &published_groups);
            if !group_identity {
                continue;
            }
            if context.live.borrow().devices[did].loading.is_some() {
                // A read from a previous cycle is still outstanding; its
                // completion owns the loading marker and freshness result.
                continue;
            }
            start_entry_load(&context, cloud, &credentials, &home, did, true);
        }
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
        if !context.current_token(token) || context.live.borrow().scope.as_ref() != Some(&scope) {
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
        {
            let mut live = context.live.borrow_mut();
            for (did, slot) in &mut live.devices {
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
        let published_groups = published_group_identities(&context);
        for did in members {
            let group_identity = known_group_identity(&context, &did, &published_groups);
            if group_identity {
                continue;
            }
            let prepare = context.live.borrow().devices[&did].needs_description();
            if !prepare {
                context.reconcile(&did)?;
                continue;
            }
            start_entry_load(&context, cloud, &credentials, &home, &did, false);
        }
        due = context.clock.now() + MEMBER_INTERVAL;
    }
}

/// Both refresh phases share one cancellable catalog task. Known groups keep
/// their identity; ordinary cloud failures preserve an admitted control path.
fn start_entry_load(
    context: &Rc<Context>,
    cloud: &CloudClient,
    credentials: &XiaomiRecord,
    home: &OwnedHome,
    did: &str,
    known_group: bool,
) {
    let life = Rc::new(Lifetime::default());
    context
        .live
        .borrow_mut()
        .devices
        .get_mut(did)
        .unwrap()
        .loading = Some(life.clone());
    let current = context.clone();
    let client = cloud.clone();
    let credentials = credentials.clone();
    let home = home.clone();
    let did = did.to_owned();
    context.tasks.spawn(async move {
        let Some(result) = future::or(
            async {
                life.ended().await;
                None
            },
            async { Some(catalog::load_entry(&client, &credentials, &home, &did).await) },
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
            Ok(CatalogEntry::Group { model, group, .. }) => {
                {
                    let mut live = current.live.borrow_mut();
                    let slot = live.devices.get_mut(&did).unwrap();
                    slot.blocked = None;
                    slot.candidate = None;
                    slot.group = Some(GroupMeta {
                        model,
                        relation: Some(group),
                    });
                }
                current.reconcile_groups()?;
            }
            Ok(CatalogEntry::Device { .. }) if known_group => current.problem(
                &did,
                Some("known group identity is no longer a confirmed group".into()),
            ),
            Ok(CatalogEntry::Device { cached, name }) => {
                {
                    let mut live = current.live.borrow_mut();
                    let slot = live.devices.get_mut(&did).unwrap();
                    slot.blocked = None;
                    slot.candidate = Some((cached, name));
                }
                current.reconcile(&did)?;
                current.request_sync();
            }
            Err(error) => {
                if !known_group && matches!(&error, CatalogError::Cloud(_)) {
                    current.problem(&did, Some(error.to_string()));
                } else {
                    // Keep the last valid description and confirmed relation.
                    current.block_device(&did, error.to_string());
                }
                if let CatalogError::Cloud(error) = &error {
                    current.cloud_error(&credentials.tokens.access_token, Some(&did), error)?;
                }
            }
        }
        Ok(())
    });
    context.changed.notify(usize::MAX);
}
