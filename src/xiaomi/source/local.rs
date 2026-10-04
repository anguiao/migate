use super::{
    model::{GatewayStatus, SourceError},
    state::{AccessRefresh, Backoff, Context, MAX_BACKOFF, Session},
};
use crate::xiaomi::{
    central::{self, CentralError, CentralEvent, SessionHandle},
    discovery::{Discovery, DiscoveryChanges, GatewayRoute},
};
use futures_lite::future;
use std::{rc::Rc, time::Duration};
pub(super) async fn run(
    context: Rc<Context>,
    discovery: Discovery,
    #[cfg(test)] test: (Option<String>, Duration),
) -> Result<(), SourceError> {
    let mut changes = discovery.subscribe();
    let mut route = None;
    let mut backoff = Backoff::new();
    let mut first_deadline = Some(context.clock.now() + Duration::from_secs(30));
    loop {
        let scope = context.live.borrow().scope.clone();
        let selected = scope
            .as_ref()
            .and_then(|scope| discovery.snapshot().route(&scope.group_id, route.as_ref()));
        let Some(selected) = selected else {
            if first_deadline.is_some() {
                context.set_gateway(GatewayStatus::Discovering);
            }
            let listener = context.changed.listen();
            let deadline =
                first_deadline.unwrap_or_else(|| context.clock.now() + backoff.failed(false));
            future::or(
                async {
                    changes.changed().await?;
                    if context.live.borrow().scope.is_none() {
                        context.request_sync();
                    }
                    Ok::<(), SourceError>(())
                },
                future::or(
                    async {
                        listener.await;
                        Ok(())
                    },
                    async {
                        context.clock.wait_until(deadline).await;
                        context.set_gateway(GatewayStatus::Unavailable(
                            SourceError::NoLocalGateway.to_string(),
                        ));
                        first_deadline = None;
                        Ok(())
                    },
                ),
            )
            .await?;
            continue;
        };
        let Some(credentials) = context.live.borrow().credentials.clone() else {
            let listener = context.changed.listen();
            listener.await;
            continue;
        };
        context.set_gateway(GatewayStatus::Connecting(selected.did.clone()));
        let snapshot = discovery.snapshot();
        let addresses = snapshot
            .gateways
            .iter()
            .find(|gateway| gateway.did == selected.did && gateway.group_id == selected.group_id)
            .unwrap()
            .addresses
            .clone();
        let mut connected = None;
        let mut failure = SourceError::NoLocalGateway;
        for address in std::iter::once(selected.address).chain(
            addresses
                .into_iter()
                .filter(|address| *address != selected.address),
        ) {
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
                    if discovery.snapshot().gateways.iter().any(|gateway| {
                        gateway.did == selected.did
                            && gateway.group_id == selected.group_id
                            && gateway.addresses.contains(&address)
                    }) && context.live.borrow().scope == scope
                    {
                        let mut selected = selected.clone();
                        selected.address = address;
                        connected = Some((selected, session));
                        break;
                    }
                    session.0.close();
                }
                Err(error) => failure = error.into(),
            }
        }
        let connected_since = context.clock.now();
        let result = if let Some((selected, (handle, driver))) = connected {
            let reconnecting = route.is_some();
            route = Some(selected.clone());
            first_deadline = None;
            let session = Session::new(handle.clone());
            let access = session.access.clone();
            let tasks = session.tasks.clone();
            context.live.borrow_mut().session = Some(session);

            context.set_gateway(GatewayStatus::Connected(selected.did.clone()));
            refresh_access(&context);
            if reconnecting {
                context.request_sync();
            }
            context.changed.notify(usize::MAX);
            let events = context.clone();
            let driving = async {
                driver
                    .run(move |event| match event {
                        CentralEvent::Property {
                            did,
                            binding,
                            value,
                        } => {
                            let work = events
                                .live
                                .borrow()
                                .session
                                .as_ref()
                                .unwrap()
                                .devices
                                .get(&did)
                                .cloned();
                            if let Some(work) = work {
                                let cached = work.record.cached.borrow();
                                if cached.device.refresh_trigger == Some(binding) {
                                    work.refresh();
                                } else if let Some(function) = cached
                                    .device
                                    .functions
                                    .iter()
                                    .find(|function| function.power == binding)
                                {
                                    work.record
                                        .physical
                                        .function(&function.description.id)
                                        .unwrap()
                                        .observe(value);
                                }
                            }
                        }
                        CentralEvent::DeviceListChanged { .. } => {
                            refresh_access(&events);
                            events.request_sync();
                        }
                    })
                    .await
                    .map_err(SourceError::Central)
            };
            future::or(
                route_watch(&discovery, &mut changes, &context, &selected),
                future::or(
                    driving,
                    future::or(
                        access_refreshes(context.clone(), handle, access),
                        tasks.run(),
                    ),
                ),
            )
            .await
        } else {
            Err(failure)
        };
        let error = result.err().unwrap_or(SourceError::Cancelled);
        context.disconnect(error.to_string());
        context.set_gateway(GatewayStatus::Unavailable(error.to_string()));
        if matches!(error, SourceError::Storage(_) | SourceError::Discovery(_)) {
            return Err(error);
        }
        let delay =
            backoff.failed(context.clock.now().saturating_sub(connected_since) >= MAX_BACKOFF);
        wait_retry(&context, &discovery, &mut changes, route.as_ref(), delay).await?;
    }
}
pub(super) fn refresh_access(context: &Rc<Context>) {
    if let Some(session) = &context.live.borrow().session {
        session.access.request();
    }
}
async fn access_refreshes(
    context: Rc<Context>,
    handle: SessionHandle,
    access: Rc<AccessRefresh>,
) -> Result<(), SourceError> {
    loop {
        access.next().await;
        let mut backoff = Backoff::new();
        loop {
            let current = context.clone();
            let result = handle
                .get_devices(move |result| match result {
                    Ok(entries) => {
                        current.set_access_problem(None);
                        current.observe_access(entries);
                    }
                    Err(CentralError::Cancelled) => {}
                    Err(error) => current.set_access_problem(Some(error.to_string())),
                })
                .await;
            match result {
                Ok(_) => {
                    context.reconcile_all()?;
                    break;
                }
                Err(CentralError::Cancelled) => return Ok(()),
                Err(_) => {
                    context
                        .clock
                        .wait_until(context.clock.now() + backoff.failed(false))
                        .await;
                    // The retry also covers refresh requests received during backoff.
                    access.requested.set(false);
                }
            }
        }
    }
}
async fn route_watch(
    discovery: &Discovery,
    changes: &mut DiscoveryChanges,
    context: &Context,
    route: &GatewayRoute,
) -> Result<(), SourceError> {
    loop {
        let listener = context.changed.listen();
        if discovery
            .snapshot()
            .route(&route.group_id, Some(route))
            .as_ref()
            != Some(route)
        {
            return Err(SourceError::NoLocalGateway);
        }
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
    }
}
pub(super) async fn wait_retry(
    context: &Context,
    discovery: &Discovery,
    changes: &mut DiscoveryChanges,
    previous: Option<&GatewayRoute>,
    delay: Duration,
) -> Result<(), SourceError> {
    let group = context
        .live
        .borrow()
        .scope
        .as_ref()
        .map(|scope| scope.group_id.clone());
    let route = group
        .as_ref()
        .and_then(|group| discovery.snapshot().route(group, previous));
    let credentials = context.live.borrow().credentials.as_ref().map(|record| {
        (
            record.uid.clone(),
            record.virtual_did.clone(),
            record.private_key_pem.clone(),
            record.certificate_pem.clone(),
        )
    });
    let deadline = context.clock.now() + delay;
    future::or(
        async {
            context.clock.wait_until(deadline).await;
            Ok(())
        },
        future::or(
            async {
                loop {
                    changes.changed().await?;
                    if group
                        .as_ref()
                        .and_then(|group| discovery.snapshot().route(group, route.as_ref()))
                        != route
                    {
                        return Ok(());
                    }
                }
            },
            async {
                loop {
                    let listener = context.changed.listen();
                    let current = context.live.borrow().credentials.as_ref().map(|record| {
                        (
                            record.uid.clone(),
                            record.virtual_did.clone(),
                            record.private_key_pem.clone(),
                            record.certificate_pem.clone(),
                        )
                    });
                    if current != credentials {
                        return Ok(());
                    }
                    listener.await;
                }
            },
        ),
    )
    .await
}
