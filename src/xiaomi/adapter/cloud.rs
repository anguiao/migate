use super::{model::AdapterError, state::Context};
use crate::{
    configuration::Scope,
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

pub(super) const MEMBER_INTERVAL: Duration = Duration::from_secs(300);
pub(super) async fn run(
    cloud: &CloudClient,
    discovery: &Discovery,
    context: Rc<Context>,
) -> Result<(), AdapterError> {
    let mut due = context.clock.now();
    loop {
        let listener = context.sync_changed.listen();
        let floor = context.directory.borrow().sync_deadline();
        if let Some(floor) = floor
            && context.clock.now() < floor
        {
            future::or(context.clock.wait_until(floor), async {
                listener.await;
            })
            .await;
            continue;
        }
        context.directory.borrow_mut().begin_sync();
        if !context.sync_requested.replace(false) && context.clock.now() < due {
            future::or(context.clock.wait_until(due), async {
                listener.await;
            })
            .await;
            continue;
        }
        if context.mijia_path().is_none() {
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
        let credentials = context.credentials.borrow().record().cloned();
        let Some(credentials) = credentials else {
            continue;
        };
        let Some(path) = context.mijia_path() else {
            continue;
        };
        context.ready.finish(Ok(()));
        let token = &path;
        if !context.current_mijia(token) {
            context.reload_credentials();
            continue;
        }
        let Some(homes) = future::or(
            async {
                context.mijia_retired(token).await;
                None
            },
            async { Some(context.mijia.get_owned_homes(&token.session).await) },
        )
        .await
        else {
            context.reload_credentials();
            context.request_sync();
            continue;
        };
        if !context.current_mijia(token) {
            continue;
        }
        let homes = match homes {
            Ok(homes) => homes,
            Err(error) => {
                context.set_catalog_problem(Some(error.to_string()));
                context.cloud_error(token, None, &error);
                if !error.is_unauthorized() {
                    context.mijia_authentication(AuthenticationState::Unavailable(
                        FailureReason::Cloud(error),
                    ));
                }
                due = context
                    .directory
                    .borrow_mut()
                    .retry_deadline(context.clock.now());
                continue;
            }
        };
        let identity_failure = match homes.uid.as_deref() {
            None => Some(FailureReason::CannotDetermineAccountIdentity),
            Some(uid) if uid != credentials.uid => Some(FailureReason::AccountIdentityMismatch),
            _ => None,
        };
        if let Some(reason) = identity_failure {
            if matches!(reason, FailureReason::AccountIdentityMismatch) {
                context.invalidate_lan_scope()?;
            }
            context.set_catalog_problem(Some(reason.to_string()));
            context.mijia_authentication(AuthenticationState::Unavailable(reason));
            context.directory.borrow_mut().invalidate_scope();
            context.update_all_availability();
            context.update_routing("cloud account identity invalidated");
            context.changed.notify(usize::MAX);
            context.stage();
            due = context.clock.now() + MEMBER_INTERVAL;
            continue;
        }
        context.mijia_authentication(AuthenticationState::Authenticated);
        let expected_scope = context.directory.borrow().scope.clone();
        let selected = {
            let live = context.directory.borrow();
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
                if expected_scope.is_some() {
                    context.invalidate_lan_scope()?;
                }
                context.set_catalog_problem(Some(error.to_string()));
                context.directory.borrow_mut().invalidate_scope();
                context.update_all_availability();
                context.update_routing("cloud home scope invalidated");
                due = context.sync_failed();
                continue;
            }
        };
        let scope = Scope {
            uid: home.uid.clone(),
            home_id: home.home_id.clone(),
        };
        let home_members = home.dids.iter().cloned().collect::<BTreeSet<_>>();
        // Classify split aliases before creating slots or requesting any
        // device description. A failed catalog retains the last known split
        // relations and only previously discovered physical candidates.
        let Some(result) = future::or(
            async {
                context.mijia_retired(token).await;
                None
            },
            async { Some(context.mijia.get_device_catalog(&token.session).await) },
        )
        .await
        else {
            context.reload_credentials();
            context.request_sync();
            continue;
        };
        if !context.current_mijia(token) || context.directory.borrow().scope != expected_scope {
            continue;
        }
        let mut catalog_problem = None;
        let catalog = match result {
            Ok(catalog) => match super::splits::selected(&catalog, &home_members) {
                Ok(splits) => Some((catalog, splits)),
                Err(reason) => {
                    log::warn!(
                        "Panel split catalog read failed; keeping confirmed relations: {reason}"
                    );
                    catalog_problem = Some(reason.to_owned());
                    None
                }
            },
            Err(error) => {
                log::warn!("Device catalog read failed; keeping confirmed relations: {error}");
                catalog_problem = Some(error.to_string());
                if error.is_unauthorized() {
                    context.cloud_error(token, None, &error);
                }
                None
            }
        };
        let splits = if let Some((_, splits)) = &catalog {
            splits.clone()
        } else if expected_scope.as_ref() == Some(&scope) {
            context
                .directory
                .borrow()
                .split_devices()
                .iter()
                .filter(|split| home_members.contains(&split.did))
                .cloned()
                .map(|mut split| {
                    if !home_members.contains(&split.parent_did) {
                        split.name = None;
                    }
                    split
                })
                .collect()
        } else {
            Vec::new()
        };
        let aliases = splits
            .iter()
            .map(|split| split.did.as_str())
            .collect::<BTreeSet<_>>();
        let members = {
            let live = context.directory.borrow();
            home_members
                .iter()
                .filter(|did| {
                    !aliases.contains(did.as_str())
                        && (live.devices.contains_key(*did) && live.group(did).is_none()
                            || catalog.is_some())
                })
                .cloned()
                .collect::<BTreeSet<_>>()
        };
        let requested = {
            let live = context.directory.borrow();
            live.devices
                .iter()
                .filter(|(did, slot)| {
                    members.contains(*did)
                        && (slot.admitted.is_some() || slot.latest_description.is_some())
                })
                .map(|(did, _)| did.clone())
                .collect::<Vec<_>>()
        };
        let Some(infos) = future::or(
            async {
                context.mijia_retired(token).await;
                None
            },
            async {
                Some(
                    context
                        .mijia
                        .get_home_devices(&token.session, &home, &requested)
                        .await,
                )
            },
        )
        .await
        else {
            context.reload_credentials();
            context.request_sync();
            continue;
        };
        if !context.current_mijia(token) || context.directory.borrow().scope != expected_scope {
            continue;
        }
        let infos = match infos {
            Ok(infos) => infos
                .into_iter()
                .map(|info| (info.did.clone(), info))
                .collect::<BTreeMap<_, _>>(),
            Err(error) => {
                context.set_catalog_problem(Some(error.to_string()));
                context.cloud_error(token, None, &error);
                due = context
                    .directory
                    .borrow_mut()
                    .retry_deadline(context.clock.now());
                continue;
            }
        };
        // The complete publication transaction persists the scope.
        let update = context
            .directory
            .borrow_mut()
            .select_home(scope.clone(), splits, &members);
        context.lan_scope_invalid.set(false);
        context.stage();
        for did in update.removed {
            context.remove(&did)?;
        }
        context.directory.borrow_mut().confirm_members(
            &members,
            infos,
            catalog.as_ref().map(|(catalog, _)| catalog),
        );
        context.mijia_authentication(AuthenticationState::Authenticated);
        context.credentials.borrow_mut().resume_http();
        if catalog.is_some() {
            context.directory.borrow_mut().reset_sync();
        }
        context.update_all_availability();
        context.update_routing("complete cloud directory and device information synchronized");
        context.set_catalog_problem(catalog_problem);
        context.changed.notify(usize::MAX);
        if update.membership_changed {
            super::local::refresh_access(&context);
        }
        let path = context
            .mijia_path()
            .expect("the selected Mijia session is current");
        let token = &path;
        let catalog_members = members.clone();
        for did in members {
            // A home member missing from a complete account catalog remains
            // visible to group membership, but cannot be described until its
            // directory record establishes whether it is a split alias.
            let known = context.directory.borrow().devices[&did].admitted.is_some()
                || context.directory.borrow().devices[&did]
                    .latest_description
                    .is_some();
            if !known
                && catalog.as_ref().is_none_or(|(catalog, _)| {
                    !catalog.devices.iter().any(|record| record.did == did)
                })
            {
                continue;
            }
            // Group-like records are admitted only by a confirmed catalog
            // relation, never through the ordinary home description path.
            let group_like = {
                let live = context.directory.borrow();
                live.group(&did).is_some()
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
            // Capture the target metadata this load is allowed to apply.
            let target = context.directory.borrow().devices[&did]
                .cloud_access
                .as_ref()
                .map(|access| (access.model.clone(), access.instance_type.clone()));
            // A directory target change retires the in-flight load so a new
            // target can start instead of waiting for a result that will be
            // discarded. Online changes keep the same target.
            let retired = context
                .directory
                .borrow_mut()
                .devices
                .get_mut(&did)
                .and_then(|slot| slot.retarget_loading(&target));
            if let Some(life) = retired {
                life.close();
            }
            let prepare = context.directory.borrow().devices[&did].needs_description();
            if !prepare {
                context.resolved_description_target(&did);
                context.reconcile(&did)?;
                continue;
            }
            let life = {
                let mut live = context.directory.borrow_mut();
                live.devices
                    .get_mut(&did)
                    .unwrap()
                    .start_loading(target.clone())
            };
            let current = context.clone();
            let client = cloud.clone();
            let credentials = credentials.clone();
            let captured_token = token.clone();
            let device = context.mijia.clone();
            let home = home.clone();
            let captured_scope = scope.clone();
            let task = async move {
                if life.is_closed()
                    || !current.current_mijia(&captured_token)
                    || current.directory.borrow().scope.as_ref() != Some(&captured_scope)
                {
                    let cleared = current
                        .directory
                        .borrow_mut()
                        .devices
                        .get_mut(&did)
                        .is_some_and(|slot| slot.finish_loading(&life));
                    if cleared {
                        current.request_sync();
                    }
                    return Ok(());
                }
                let Some(result) = future::or(
                    async {
                        current.mijia_retired(&captured_token).await;
                        None
                    },
                    future::or(
                        async {
                            life.ended().await;
                            None
                        },
                        async {
                            Some(
                                catalog::load_device(&client, &device, &credentials, &home, &did)
                                    .await,
                            )
                        },
                    ),
                )
                .await
                else {
                    let cleared = current
                        .directory
                        .borrow_mut()
                        .devices
                        .get_mut(&did)
                        .is_some_and(|slot| slot.finish_loading(&life));
                    if cleared {
                        current.request_sync();
                    }
                    return Ok(());
                };
                // A retired/removed slot, a superseding load or a changed
                // directory target must discard this older result.
                let current_loading = {
                    let live = current.directory.borrow();
                    let Some(slot) = live.devices.get(&did) else {
                        return Ok(());
                    };
                    slot.loading_is(&life)
                        && slot
                            .cloud_access
                            .as_ref()
                            .map(|access| (access.model.clone(), access.instance_type.clone()))
                            == target
                };
                if !current_loading
                    || !current.current_mijia(&captured_token)
                    || current.directory.borrow().scope.as_ref() != Some(&captured_scope)
                {
                    let cleared = current
                        .directory
                        .borrow_mut()
                        .devices
                        .get_mut(&did)
                        .is_some_and(|slot| slot.finish_loading(&life));
                    if cleared {
                        current.request_sync();
                    }
                    return Ok(());
                }
                if let Some(slot) = current.directory.borrow_mut().devices.get_mut(&did) {
                    slot.finish_loading(&life);
                }
                match result {
                    Ok(description) => {
                        let matches_target = description.raw.key
                            == crate::device::DeviceKey::new("xiaomi", &did)
                            && target.as_ref().is_none_or(|(model, urn)| {
                                *model == description.raw.model
                                    && *urn == description.raw.instance_type
                            });
                        if matches_target {
                            current
                                .directory
                                .borrow_mut()
                                .accept_description(&did, description);
                            current.stage();
                            // The new latest fact must enter the next
                            // directory pass so its access facts are read
                            // without another description load.
                            current.request_sync();
                        } else {
                            current.pending_description(
                                &did,
                                "device details no longer match the requested description".into(),
                            );
                        }
                    }
                    Err(error) => {
                        // A failed new description is diagnostic only; the
                        // committed runtime keeps serving and the latest fact
                        // is retained for the next retry. Temporary fetch
                        // failures must not pause account-wide HTTP control;
                        // real access rejections keep their source rule.
                        current.pending_description(&did, error.to_string());
                        if let CatalogError::Cloud(error) = &error
                            && !error.is_temporary()
                        {
                            current.cloud_error(&captured_token, Some(&did), error);
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
        if context.current_mijia(token)
            && context.directory.borrow().scope.as_ref() == Some(&scope)
            && let Some((catalog, _)) = &catalog
        {
            let update =
                context
                    .directory
                    .borrow_mut()
                    .confirm_catalog(catalog, &scope, &catalog_members);
            for did in &update.withdrawn {
                if context.directory.borrow().devices.contains_key(did) {
                    context.remove_group(did)?;
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
            context.sync_groups(cloud, token, &update.active)?;
        }
        context.stage();
        due = if catalog.is_some() {
            context.clock.now() + MEMBER_INTERVAL
        } else {
            context
                .directory
                .borrow_mut()
                .retry_deadline(context.clock.now())
        };
    }
}
