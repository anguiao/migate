use super::{
    model::SourceError,
    state::{Backoff, Context, update_auth},
};
use crate::{
    device::{Availability, DeviceKey},
    storage::Store,
    xiaomi::{
        auth::{AuthService, AuthenticationState, CertificateUpdate, FailureReason},
        catalog::{self, CatalogError},
        cloud::{CloudClient, CloudError, CloudErrorKind},
        discovery::Discovery,
    },
};
use futures_lite::future;
use std::rc::Rc;

fn transient_cloud_error(error: &CloudError) -> bool {
    matches!(
        error.kind(),
        CloudErrorKind::Network
            | CloudErrorKind::Timeout
            | CloudErrorKind::HttpStatus(408 | 429 | 500..=599)
    )
}

pub(super) async fn cached_auth(
    auth: &AuthService,
    cloud: &CloudClient,
    key: &DeviceKey,
    discovery: &Discovery,
    context: &Rc<Context>,
    store: &Store,
) -> Result<(), SourceError> {
    let mut backoff = Backoff::new();
    let mut topology_refreshed = false;
    while refresh_cached_topology(
        auth,
        cloud,
        key,
        discovery,
        context,
        store,
        &mut topology_refreshed,
    )
    .await?
    {
        let delay = backoff.failed(false);
        log::warn!(
            "Xiaomi startup cloud preparation will retry in {}s",
            delay.as_secs()
        );
        context.clock.wait_until(context.clock.now() + delay).await;
    }
    // This only completes startup preparation; successful runs do not schedule
    // periodic authentication or make the local session depend on the cloud.
    future::pending().await
}

/// Return true only when this startup preparation needs a transient retry.
async fn refresh_cached_topology(
    auth: &AuthService,
    cloud: &CloudClient,
    key: &DeviceKey,
    discovery: &Discovery,
    context: &Rc<Context>,
    store: &Store,
    topology_refreshed: &mut bool,
) -> Result<bool, SourceError> {
    let preparation = auth.prepare_bridge(key).await?;
    update_auth(context, &preparation);
    let retry_allowed = !matches!(
        preparation.report.authentication,
        AuthenticationState::SignInRequired(_)
    );
    let retry_certificate = retry_allowed
        && matches!(
            &preparation.report.certificate_update,
            CertificateUpdate::Failed(FailureReason::Cloud(error))
                if transient_cloud_error(error)
        );
    let metadata = context.metadata();
    let authoritative = !preparation.homes.is_empty()
        || matches!(
            preparation.report.authentication,
            AuthenticationState::Authenticated
        );
    if authoritative
        && !preparation
            .homes
            .iter()
            .any(|home| home.uid == metadata.uid && home.dids.contains(&key.id))
    {
        context.block("target device is missing from the current owned home directory".into());
        return Ok(false);
    }
    let Some(credentials) = preparation.credentials else {
        context.block("Xiaomi sign-in is required".into());
        return Ok(false);
    };
    if preparation.homes.is_empty() {
        return Ok(matches!(
            &preparation.report.authentication,
            AuthenticationState::Unavailable(FailureReason::Cloud(error))
                if transient_cloud_error(error)
        ));
    }
    let target_homes = preparation
        .homes
        .iter()
        .filter(|home| home.uid == credentials.uid && home.dids.contains(&key.id))
        .cloned()
        .collect::<Vec<_>>();
    let moved = !target_homes
        .iter()
        .any(|home| home.home_id == metadata.home_id);
    if moved {
        context.block("waiting for the device's current owned home gateway".into());
    } else if *topology_refreshed {
        return Ok(retry_certificate);
    }
    let mut changes = discovery.subscribe();
    loop {
        match catalog::load_scoped(
            &discovery.snapshot(),
            &target_homes,
            &credentials,
            key,
            cloud,
        )
        .await
        {
            Ok(fresh) => {
                let changed = context.metadata() != fresh;
                let unblocked = {
                    let mut live = context.live.borrow_mut();
                    let topology = live.topology.as_mut().unwrap();
                    topology.metadata = fresh;
                    topology.dirty = true;
                    live.blocked.take().is_some()
                };
                context.set_catalog_problem(None);
                if changed {
                    context.disconnect("device description updated".into());
                } else if matches!(
                    context.device().snapshot().availability,
                    Availability::Available
                ) {
                    context.persist_topology(store)?;
                }
                if unblocked && !changed {
                    context.changed.notify(usize::MAX);
                }
                *topology_refreshed = true;
                return Ok(retry_certificate);
            }
            Err(CatalogError::NoLocalHome) => {
                changes.changed().await?;
            }
            Err(CatalogError::Cloud(error)) => {
                context.set_catalog_problem(Some(error.to_string()));
                if error.is_unauthorized() {
                    context.status.borrow_mut().authentication.authentication =
                        AuthenticationState::SignInRequired(FailureReason::Cloud(error.clone()));
                    context.status_changed.notify(usize::MAX);
                }
                log::warn!("Xiaomi static description refresh failed: {error}");
                return Ok(retry_allowed && transient_cloud_error(&error));
            }
            Err(error) => {
                context.set_catalog_problem(Some(error.to_string()));
                context.block(error.to_string());
                return Ok(false);
            }
        }
    }
}
