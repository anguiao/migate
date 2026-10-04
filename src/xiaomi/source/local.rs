use super::{
    model::{GatewayStatus, PreparedDevice, SourceError},
    state::{Backoff, Context, MAX_BACKOFF, Session},
};
use crate::{
    device::{Availability, CommandError, CommandReceiver, CommandRequest},
    storage::{CachedDevice, Store},
    xiaomi::{
        central::{self, CentralError, CentralEvent, DeviceAccess, SessionHandle},
        discovery::{Discovery, DiscoveryChanges, GatewayRoute},
    },
};
use futures_lite::future;
use std::{cell::RefCell, rc::Rc, time::Duration};

pub(super) async fn local_run(
    receiver: &mut CommandReceiver,
    context: Rc<Context>,
    store: Store,
    discovery: Discovery,
    cached: bool,
    #[cfg(test)] test: (Option<String>, Duration),
) -> Result<(), SourceError> {
    let mut backoff = Backoff::new();
    let mut route: Option<GatewayRoute> = None;
    let mut changes = discovery.subscribe();
    loop {
        if context.live.borrow().blocked.is_some() {
            let listener = context.changed.listen();
            listener.await;
            continue;
        }
        let metadata = context.metadata();
        let snapshot = discovery.snapshot();
        let selected = snapshot.route(&metadata.group_id, route.as_ref());
        let Some(selected) = selected else {
            context.disconnect(SourceError::NoLocalGateway.to_string());
            if !cached && !context.ready.completed() {
                return Err(SourceError::NoLocalGateway);
            }
            let listener = context.changed.listen();
            future::or(
                async {
                    changes
                        .changed()
                        .await
                        .map(|_| ())
                        .map_err(SourceError::Discovery)
                },
                async {
                    listener.await;
                    Ok(())
                },
            )
            .await?;
            continue;
        };
        let credentials = context.live.borrow().credentials.clone();
        let Some(credentials) = credentials else {
            context.block("Xiaomi sign-in is required".into());
            continue;
        };
        let addresses = snapshot
            .gateways
            .iter()
            .find(|gateway| gateway.did == selected.did && gateway.group_id == selected.group_id)
            .unwrap()
            .addresses
            .clone();
        context.set_gateway(GatewayStatus::Connecting(selected.did.clone()));
        let mut connected = None;
        let mut failure = SourceError::NoLocalGateway;
        for address in std::iter::once(selected.address).chain(
            addresses
                .into_iter()
                .filter(|address| *address != selected.address),
        ) {
            if !discovery.snapshot().gateways.iter().any(|gateway| {
                gateway.did == selected.did
                    && gateway.group_id == selected.group_id
                    && gateway.addresses.contains(&address)
            }) {
                continue;
            }
            #[cfg(test)]
            let result = if let Some(ca) = &test.0 {
                central::test_support::connect(address, &credentials, ca, test.1).await
            } else {
                central::connect(address, &credentials).await
            };
            #[cfg(not(test))]
            let result = central::connect(address, &credentials).await;
            match result {
                Ok(session) => {
                    let live = context.live.borrow();
                    let valid = live.blocked.is_none()
                        && live.topology.as_ref().map(|topology| &topology.metadata)
                            == Some(&metadata)
                        && credentials.uid == metadata.uid
                        && discovery.snapshot().gateways.iter().any(|gateway| {
                            gateway.did == selected.did
                                && gateway.group_id == selected.group_id
                                && gateway.addresses.contains(&address)
                        });
                    drop(live);
                    if !valid {
                        session.0.close();
                        failure = SourceError::StalePreparation;
                        break;
                    }
                    let mut selected = selected.clone();
                    selected.address = address;
                    connected = Some((selected, session));
                    break;
                }
                Err(error) => failure = error.into(),
            }
        }
        let result = if let Some((selected, (handle, driver))) = connected {
            route = Some(selected.clone());
            let generation = handle.generation();
            {
                let mut live = context.live.borrow_mut();
                live.session = Some(Session::new(handle.clone()));
                live.preparation.invalidate(context.clock.now());
            }
            context.set_gateway(GatewayStatus::Connected(selected.did.clone()));
            let events = context.clone();
            let event_handle = handle.clone();
            let driving = async {
                driver
                    .run(move |event| match event {
                        CentralEvent::Property {
                            did,
                            binding,
                            value,
                        } => {
                            if events.session_current(generation) {
                                let metadata = events.metadata();
                                if did == metadata.outlet.description.key.id
                                    && binding == metadata.outlet.main_power
                                {
                                    events.device().observe(value);
                                }
                            }
                        }
                        CentralEvent::DeviceListChanged { dids } => {
                            if dids.contains(&events.metadata().outlet.description.key.id) {
                                refresh_access(&event_handle, &events, generation);
                            }
                        }
                    })
                    .await
                    .map_err(SourceError::Central)
            };
            let working = session_work(receiver, &handle, &context, &store, generation);
            let routing = route_watch(&discovery, &mut changes, &context, &selected, generation);
            // Invalidate a removed route before polling any old session reply.
            future::or(routing, future::or(driving, working)).await
        } else {
            Err(failure)
        };
        let error = result.err().unwrap_or(SourceError::Cancelled);
        let reason = context
            .live
            .borrow()
            .blocked
            .clone()
            .unwrap_or_else(|| error.to_string());
        context.disconnect(reason);
        let stable = std::mem::take(&mut context.live.borrow_mut().reset_backoff);
        if matches!(error, SourceError::Storage(_) | SourceError::Discovery(_))
            || !cached && !context.ready.completed()
        {
            return Err(error);
        }
        wait_retry(
            &context,
            &discovery,
            &mut changes,
            route.as_ref(),
            backoff.failed(stable),
        )
        .await?;
    }
}
pub(super) async fn wait_retry(
    context: &Context,
    discovery: &Discovery,
    changes: &mut DiscoveryChanges,
    previous_route: Option<&GatewayRoute>,
    delay: Duration,
) -> Result<(), SourceError> {
    let listener = context.changed.listen();
    let group_id = context.metadata().group_id;
    let route = discovery.snapshot().route(&group_id, previous_route);
    future::or(
        async {
            context.clock.wait_until(context.clock.now() + delay).await;
            Ok(())
        },
        future::or(
            async {
                loop {
                    changes.changed().await?;
                    if discovery.snapshot().route(&group_id, route.as_ref()) != route {
                        return Ok(());
                    }
                }
            },
            async {
                listener.await;
                Ok(())
            },
        ),
    )
    .await
}
async fn route_watch(
    discovery: &Discovery,
    changes: &mut DiscoveryChanges,
    context: &Context,
    route: &GatewayRoute,
    generation: u64,
) -> Result<(), SourceError> {
    loop {
        let listener = context.changed.listen();
        if !context.session_current(generation) {
            return Err(SourceError::StalePreparation);
        }
        if discovery
            .snapshot()
            .route(&route.group_id, Some(route))
            .as_ref()
            != Some(route)
        {
            context.disconnect("current gateway route changed".into());
            return Err(SourceError::StalePreparation);
        }
        future::or(
            async { changes.changed().await.map(|_| ()).map_err(Into::into) },
            async {
                listener.await;
                Ok::<(), SourceError>(())
            },
        )
        .await?;
    }
}
fn access(
    result: Result<Option<DeviceAccess>, CentralError>,
    metadata: &CachedDevice,
) -> Result<(), SourceError> {
    let access = result?.ok_or_else(|| {
        SourceError::Unavailable("target is absent from the local gateway".into())
    })?;
    if access.model != metadata.model || access.instance_type != metadata.instance_type {
        return Err(SourceError::Unavailable(
            "local device description differs from the current static description".into(),
        ));
    }
    if !access.online || !access.spec_v2_access || !access.push_available {
        return Err(SourceError::Unavailable(
            "local device is offline or lacks property/push access".into(),
        ));
    }
    Ok(())
}
fn refresh_access(handle: &SessionHandle, context: &Rc<Context>, generation: u64) {
    if !context.session_current(generation) {
        return;
    }
    {
        let mut live = context.live.borrow_mut();
        if !live.session.as_mut().unwrap().refresh.request() {
            return;
        }
    }
    let metadata = context.metadata();
    let did = metadata.outlet.description.key.id.clone();
    let updated = context.clone();
    let next_handle = handle.clone();
    drop(handle.get_device_list(&did, move |result| {
        if !updated.session_current(generation) {
            return;
        }
        let outcome = access(result, &metadata);
        let again = {
            let mut live = updated.live.borrow_mut();
            live.session.as_mut().unwrap().refresh.finish()
        };
        match outcome {
            Err(error) => updated.unavailable(error.to_string()),
            Ok(()) => {
                if !matches!(
                    updated.device().snapshot().availability,
                    Availability::Available
                ) {
                    updated.changed.notify(usize::MAX);
                }
            }
        }
        if again {
            refresh_access(&next_handle, &updated, generation);
        }
    }));
}
async fn prepare(
    handle: &SessionHandle,
    context: &Rc<Context>,
    generation: u64,
) -> Result<(), SourceError> {
    let (version, metadata) = {
        let live = context.live.borrow();
        (
            live.preparation.version,
            live.topology.as_ref().unwrap().metadata.clone(),
        )
    };
    let did = metadata.outlet.description.key.id.clone();
    let binding = metadata.outlet.main_power;
    let checked = Rc::new(RefCell::new(None));
    let outcome = checked.clone();
    let current = context.clone();
    let _ = handle
        .get_device_list(&did, move |result| {
            let result = access(result, &metadata);
            if current.current(generation, version)
                && let Err(error) = &result
            {
                current.unavailable(error.to_string());
            }
            *outcome.borrow_mut() = Some(result);
        })
        .await;
    checked.borrow_mut().take().unwrap()?;
    if !context.current(generation, version) {
        return Err(SourceError::StalePreparation);
    }
    handle.subscribe_property(&did, binding).await?;
    if !context.current(generation, version) {
        return Err(SourceError::StalePreparation);
    }
    let current = context.clone();
    handle
        .read_property(&did, binding, move |result| {
            if current.current(generation, version) {
                match result {
                    Ok(value) => current.prepared(generation, version, value),
                    Err(error) => current.unavailable(error.to_string()),
                }
            }
        })
        .await?;
    if !context.current(generation, version)
        || !matches!(
            context.device().snapshot().availability,
            Availability::Available
        )
    {
        return Err(SourceError::StalePreparation);
    }
    Ok(())
}
fn command_error(error: CentralError) -> CommandError {
    match error {
        CentralError::Timeout => CommandError::Timeout,
        CentralError::Rejected(code) => CommandError::Rejected(format!("gateway code {code}")),
        CentralError::Cancelled => CommandError::Cancelled,
        error => CommandError::SendFailed(error.to_string()),
    }
}
async fn session_work(
    receiver: &mut CommandReceiver,
    handle: &SessionHandle,
    context: &Rc<Context>,
    store: &Store,
    generation: u64,
) -> Result<(), SourceError> {
    let mut backoff = Backoff::new();
    loop {
        if let Err(error) = prepare(handle, context, generation).await {
            if !context.ready.completed() {
                return Err(error);
            }
            context.unavailable(error.to_string());
            let listener = context.changed.listen();
            future::or(
                context
                    .clock
                    .wait_until(context.clock.now() + backoff.failed(false)),
                async {
                    listener.await;
                },
            )
            .await;
            continue;
        }
        context.persist_topology(store)?;
        context.ready.finish(Ok(PreparedDevice {
            device: context.device(),
            metadata: context.metadata(),
        }));
        let version = context.live.borrow().preparation.version;
        let since = context.live.borrow().preparation.available_since.unwrap();
        loop {
            if !context.current(generation, version) {
                break;
            }
            enum Wake {
                Command(Option<CommandRequest>),
                Prepare,
            }
            let wake = future::or(async { Wake::Command(receiver.next().await) }, async {
                context.wait_reprepare(generation, version).await;
                Wake::Prepare
            })
            .await;
            match wake {
                Wake::Prepare => break,
                Wake::Command(Some(request)) => {
                    let remaining = request.remaining();
                    if remaining.is_zero() {
                        request.reject(CommandError::Timeout);
                        continue;
                    }
                    let metadata = context.metadata();
                    let current = context.clone();
                    let target = request.target;
                    let writing = handle.set_property(
                        &metadata.outlet.description.key.id,
                        metadata.outlet.main_power,
                        target,
                        remaining,
                        move |result| {
                            if !current.current(generation, version) {
                                request.reject(CommandError::Unavailable(
                                    "gateway preparation changed".into(),
                                ));
                            } else {
                                match result {
                                    Ok(()) => {
                                        let _ = request.accept();
                                    }
                                    Err(error) => request.reject(command_error(error)),
                                }
                            }
                        },
                    );
                    future::or(
                        async {
                            let _ = writing.await;
                        },
                        context.wait_reprepare(generation, version),
                    )
                    .await;
                }
                Wake::Command(None) => return Err(SourceError::Cancelled),
            }
        }
        if context.clock.now().saturating_sub(since) >= MAX_BACKOFF {
            backoff = Backoff::new();
        }
    }
}
