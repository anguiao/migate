use super::{
    XiaomiSource,
    model::{GatewayStatus, PreparedDevice, Ready, SourceError, SourceStatus, TopologyReady},
    state::{Context, Live, Preparation, Topology, update_auth},
};
use crate::{
    device::{Availability, Clock, CommandReceiver, Device, DeviceKey},
    storage::{CachedDevice, Store},
    xiaomi::{
        auth::{AuthReport, AuthService, AuthenticationState, FailureReason},
        catalog::{self, CatalogError},
        cloud::CloudClient,
        discovery::Discovery,
    },
};
use event_listener::Event;
use futures_lite::future;
use std::{
    cell::RefCell,
    future::Future,
    pin::Pin,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

const DISCOVERY_WAIT: Duration = Duration::from_secs(30);

pub(super) fn system_clock() -> Arc<dyn Clock> {
    Arc::new(SystemClock(Instant::now()))
}

struct SystemClock(Instant);
impl Clock for SystemClock {
    fn now(&self) -> Duration {
        self.0.elapsed()
    }
    fn wait_until(&self, deadline: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            async_io::Timer::at(self.0 + deadline).await;
        })
    }
}
impl XiaomiSource {
    pub(super) fn build(
        store: Store,
        cloud: CloudClient,
        discovery: Discovery,
        key: DeviceKey,
        clock: Arc<dyn Clock>,
    ) -> Result<(Self, TopologyReady), SourceError> {
        if key.source != "xiaomi"
            || key.id.is_empty()
            || key.id.trim() != key.id
            || key.id.chars().any(char::is_control)
        {
            return Err(SourceError::Unavailable("invalid Xiaomi device key".into()));
        }
        let auth = AuthService::new(store.xiaomi(), cloud.clone());
        let credentials = auth.saved_bridge_credentials()?;
        let cached = auth.load_bridge_cache(&key)?;
        let report = auth.local_status()?;
        let ready = Rc::new(Ready::default());
        let (device, receiver) = if let Some(metadata) = &cached {
            let (device, receiver) = make_device(metadata);
            device.set_availability(Availability::Unavailable(
                "restoring local gateway connection".into(),
            ));
            ready.finish(Ok(PreparedDevice {
                device: device.clone(),
                metadata: metadata.clone(),
            }));
            (Some(device), Some(receiver))
        } else {
            (None, None)
        };
        let was_cached = cached.is_some();
        let context = Rc::new(Context {
            live: RefCell::new(Live {
                credentials,
                topology: cached.zip(device).map(|(metadata, device)| Topology {
                    metadata,
                    device,
                    dirty: false,
                }),
                session: None,
                preparation: Preparation::default(),
                reset_backoff: false,
                blocked: None,
            }),
            changed: Event::new(),
            status: RefCell::new(SourceStatus {
                authentication: report,
                gateway: GatewayStatus::Discovering,
                catalog_problem: None,
            }),
            status_changed: Event::new(),
            ready: ready.clone(),
            clock: clock.clone(),
        });
        Ok((
            Self {
                store,
                cloud,
                auth,
                discovery,
                key,
                context,
                receiver,
                cached: was_cached,
                #[cfg(test)]
                test_ca: None,
                #[cfg(test)]
                request_timeout: Duration::from_secs(10),
            },
            TopologyReady(ready),
        ))
    }
    pub(super) async fn prepare_uncached(&mut self) -> Result<(), SourceError> {
        let preparation = self.auth.prepare_bridge(&self.key).await?;
        update_auth(&self.context, &preparation);
        let credentials = preparation
            .credentials
            .ok_or_else(|| SourceError::Unavailable("Xiaomi sign-in is required".into()))?;
        if preparation.homes.is_empty() {
            return Err(authentication_error(&preparation.report));
        }
        let target_homes = preparation
            .homes
            .iter()
            .filter(|home| home.uid == credentials.uid && home.dids.contains(&self.key.id))
            .cloned()
            .collect::<Vec<_>>();
        if target_homes.is_empty() {
            return Err(CatalogError::TargetMissing.into());
        }
        let mut changes = self.discovery.subscribe();
        let mut discovery_deadline = None;
        loop {
            if discovery_deadline.is_some_and(|deadline| self.context.clock.now() >= deadline) {
                return Err(SourceError::NoLocalGateway);
            }
            let snapshot = self.discovery.snapshot();
            match catalog::load_scoped(
                &snapshot,
                &target_homes,
                &credentials,
                &self.key,
                &self.cloud,
            )
            .await
            {
                Ok(metadata) => {
                    let (device, receiver) = make_device(&metadata);
                    self.receiver = Some(receiver);
                    let mut live = self.context.live.borrow_mut();
                    live.topology = Some(Topology {
                        device,
                        metadata,
                        dirty: true,
                    });
                    break;
                }
                Err(CatalogError::NoLocalHome) => {
                    let deadline = *discovery_deadline
                        .get_or_insert_with(|| self.context.clock.now() + DISCOVERY_WAIT);
                    if self.context.clock.now() >= deadline {
                        return Err(SourceError::NoLocalGateway);
                    }
                    future::or(
                        async { changes.changed().await.map(|_| ()).map_err(Into::into) },
                        async {
                            self.context.clock.wait_until(deadline).await;
                            Err(SourceError::NoLocalGateway)
                        },
                    )
                    .await?;
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }
}

fn make_device(metadata: &CachedDevice) -> (Device, CommandReceiver) {
    Device::new(metadata.outlet.description.clone())
}
fn authentication_error(report: &AuthReport) -> SourceError {
    match &report.authentication {
        AuthenticationState::Unavailable(FailureReason::Cloud(error))
        | AuthenticationState::SignInRequired(FailureReason::Cloud(error)) => {
            SourceError::Cloud(error.clone())
        }
        AuthenticationState::Unavailable(reason) | AuthenticationState::SignInRequired(reason) => {
            SourceError::Unavailable(reason.to_string())
        }
        _ => SourceError::Unavailable("Xiaomi owned home directory is unavailable".into()),
    }
}
