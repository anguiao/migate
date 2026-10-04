use super::{
    bootstrap, cloud, local,
    model::{SourceError, SourceStatus, TopologyReady},
    state::Context,
};
use crate::{
    device::{CommandReceiver, DeviceKey},
    storage::Store,
    xiaomi::{auth::AuthService, cloud::CloudClient, discovery::Discovery},
};
use futures_lite::future;
use std::rc::Rc;
#[cfg(test)]
use std::time::Duration;

/// A single device source. Network sessions can change; its Device and command
/// receiver remain alive for the complete bridge lifetime.
pub struct XiaomiSource {
    pub(super) store: Store,
    pub(super) cloud: CloudClient,
    pub(super) auth: AuthService,
    pub(super) discovery: Discovery,
    pub(super) key: DeviceKey,
    pub(super) context: Rc<Context>,
    pub(super) receiver: Option<CommandReceiver>,
    pub(super) cached: bool,
    #[cfg(test)]
    pub(super) test_ca: Option<String>,
    #[cfg(test)]
    pub(super) request_timeout: Duration,
}
impl XiaomiSource {
    pub fn new(
        store: Store,
        cloud: CloudClient,
        discovery: Discovery,
        key: DeviceKey,
    ) -> Result<(Self, TopologyReady), SourceError> {
        Self::build(store, cloud, discovery, key, bootstrap::system_clock())
    }
    pub async fn run(mut self, mut on_status: impl FnMut(SourceStatus)) -> Result<(), SourceError> {
        let mut reported = self.context.status.borrow().clone();
        on_status(reported.clone());
        let context = self.context.clone();
        let result = {
            let monitor = async {
                loop {
                    let listener = context.status_changed.listen();
                    let status = context.status.borrow().clone();
                    if status != reported {
                        on_status(status.clone());
                        reported = status;
                    }
                    listener.await;
                }
            };
            future::or(self.run_inner(), monitor).await
        };
        if let Err(error) = &result {
            self.context.disconnect(error.to_string());
            self.context.ready.finish(Err(error.clone()));
        }
        let status = self.context.status.borrow().clone();
        if status != reported {
            on_status(status);
        }
        result
    }
    async fn run_inner(&mut self) -> Result<(), SourceError> {
        if !self.cached {
            self.prepare_uncached().await?;
        }
        let context = self.context.clone();
        let store = self.store.clone();
        let discovery = self.discovery.clone();
        let cached = self.cached;
        #[cfg(test)]
        let test = (self.test_ca.clone(), self.request_timeout);
        let receiver = self.receiver.as_mut().unwrap();
        let local = local::local_run(
            receiver,
            context.clone(),
            store.clone(),
            discovery.clone(),
            cached,
            #[cfg(test)]
            test,
        );
        if cached {
            future::or(
                local,
                cloud::cached_auth(
                    &self.auth,
                    &self.cloud,
                    &self.key,
                    &discovery,
                    &context,
                    &store,
                ),
            )
            .await
        } else {
            local.await
        }
    }
}
impl Drop for XiaomiSource {
    fn drop(&mut self) {
        drop(self.receiver.take());
        self.context
            .disconnect("Xiaomi device source stopped".into());
        self.context.ready.finish(Err(SourceError::Cancelled));
    }
}
