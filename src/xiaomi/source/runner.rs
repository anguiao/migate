use super::{
    bootstrap, cloud, local,
    model::{SourceError, SourceStatus, TopologyReady},
    state::{self, Context},
};
use crate::{
    storage::Store,
    xiaomi::{auth::AuthService, cloud::CloudClient, discovery::Discovery},
};
use futures_lite::future;
use std::rc::Rc;
#[cfg(test)]
use std::time::Duration;
/// One shared home session and one persistent command consumer per function.
pub struct XiaomiSource {
    pub(super) cloud: CloudClient,
    pub(super) auth: AuthService,
    pub(super) discovery: Discovery,
    pub(super) context: Rc<Context>,
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
    ) -> Result<(Self, TopologyReady), SourceError> {
        Self::build(store, cloud, discovery, bootstrap::system_clock())
    }
    pub async fn run(self, mut on_status: impl FnMut(SourceStatus)) -> Result<(), SourceError> {
        let mut reported = self.context.status.borrow().clone();
        on_status(reported.clone());
        let context = self.context.clone();
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
        let result = future::or(monitor, self.run_inner()).await;
        if let Err(error) = &result {
            self.context.ready.finish(Err(error.clone()));
            self.context.disconnect(error.to_string());
        }
        let status = self.context.status.borrow().clone();
        if status != reported {
            on_status(status);
        }
        result
    }
    async fn run_inner(&self) -> Result<(), SourceError> {
        future::or(
            local::run(
                self.context.clone(),
                self.discovery.clone(),
                #[cfg(test)]
                (self.test_ca.clone(), self.request_timeout),
            ),
            future::or(
                cloud::run(
                    &self.auth,
                    &self.cloud,
                    &self.discovery,
                    self.context.clone(),
                ),
                state::tasks(self.context.clone()),
            ),
        )
        .await
    }
}
impl Drop for XiaomiSource {
    fn drop(&mut self) {
        self.context.disconnect("Xiaomi source stopped".into());
        for physical in self.context.registry.list() {
            self.context.registry.remove(&physical.description().key);
        }
        for life in self.context.live.borrow().loading.values() {
            life.close();
        }
        self.context.tasks.borrow_mut().clear();
        self.context.ready.finish(Err(SourceError::Cancelled));
    }
}
