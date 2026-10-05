use super::{
    model::{GatewayStatus, SourceError},
    state::{AccessRefresh, AccessRequest, Backoff, Context, Session},
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
    let mut backoff = Backoff::new(1, 30);
    let mut reconnect = Backoff::new(6, 60);
    let mut first_deadline = Some(context.clock.now() + Duration::from_secs(30));
    loop {
        let scope = context.live.borrow().scope.clone();
        let mut routes = scope
            .as_ref()
            .map(|scope| discovery.snapshot().routes(&scope.group_id, None))
            .unwrap_or_default();
        if routes.is_empty() {
            if first_deadline.is_some() {
                context.set_gateway(GatewayStatus::Discovering);
            }
            let listener = context.changed.listen();
            let deadline = first_deadline.unwrap_or_else(|| context.clock.now() + backoff.failed());
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
        }
        // Continue after the failed session in stable candidate order.
        if let Some(previous) = route.as_ref()
            && let Some(index) = routes.iter().position(|candidate| candidate == previous)
        {
            routes.rotate_left(index + 1);
        }
        let Some(credentials) = context.live.borrow().credentials.clone() else {
            let listener = context.changed.listen();
            listener.await;
            continue;
        };
        let mut connected = None;
        let mut failure = SourceError::NoLocalGateway;
        for selected in routes {
            context.set_gateway(GatewayStatus::Connecting(selected.did.clone()));
            let address = selected.address;
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
                        connected = Some((selected, session));
                        break;
                    }
                    session.0.close();
                }
                Err(error) => failure = error.into(),
            }
        }
        let result = if let Some((selected, (handle, driver))) = connected {
            reconnect = Backoff::new(6, 60);
            let reconnecting = route.is_some();
            route = Some(selected.clone());
            first_deadline = None;
            let session = Session::new(handle.clone());
            let access = session.access.clone();
            let tasks = session.tasks.clone();
            context.live.borrow_mut().session = Some(session);
            tasks.spawn(super::work::refreshes(context.clone(), handle.clone()));

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
                        CentralEvent::DeviceListChanged { dids } => {
                            events
                                .live
                                .borrow()
                                .session
                                .as_ref()
                                .unwrap()
                                .access
                                .request_filtered(dids);
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
        let delay = reconnect.failed();
        wait_retry(&context, &discovery, &mut changes, route.as_ref(), delay).await?;
    }
}
pub(super) fn refresh_access(context: &Rc<Context>) {
    if let Some(session) = &context.live.borrow().session {
        session.access.request(context.clock.now());
    }
}
async fn access_refreshes(
    context: Rc<Context>,
    handle: SessionHandle,
    access: Rc<AccessRefresh>,
) -> Result<(), SourceError> {
    loop {
        let request = access.next(context.clock.as_ref()).await;
        let current = context.clone();
        let mut affected = None;
        let result = match request {
            AccessRequest::Full => {
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
                if result.is_err() {
                    access.retry_full(context.clock.now());
                }
                result
            }
            AccessRequest::Filtered(dids) => {
                let requested = dids.clone();
                let result = handle
                    .get_devices_filtered(&dids, move |result| match result {
                        Ok(entries) => {
                            current.set_access_problem(None);
                            current.observe_access_filtered(&requested, entries);
                        }
                        Err(CentralError::Cancelled) => {}
                        Err(error) => current.set_access_problem(Some(error.to_string())),
                    })
                    .await;
                affected = Some(dids);
                result
            }
        };
        match result {
            Ok(_) => {
                if let Some(dids) = affected {
                    for did in dids {
                        if context.live.borrow().devices.contains_key(&did) {
                            context.reconcile(&did)?;
                        }
                    }
                    context.changed.notify(usize::MAX);
                } else {
                    context.reconcile_all()?;
                }
            }
            Err(CentralError::Cancelled) => return Ok(()),
            Err(_) => {}
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
    let routes = group
        .as_ref()
        .map(|group| discovery.snapshot().routes(group, previous))
        .unwrap_or_default();
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
                        .map(|group| discovery.snapshot().routes(group, previous))
                        .unwrap_or_default()
                        != routes
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
