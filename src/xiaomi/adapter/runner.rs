use super::{
    bootstrap, cloud, local,
    model::{AdapterError, AdapterStatus, TopologyReady},
    state::Context,
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
pub struct XiaomiAdapter {
    pub(super) cloud: CloudClient,
    pub(super) auth: AuthService,
    pub(super) discovery: Discovery,
    pub(super) context: Rc<Context>,
    #[cfg(test)]
    pub(super) test_ca: Option<String>,
    #[cfg(test)]
    pub(super) request_timeout: Duration,
}
impl XiaomiAdapter {
    pub fn source_handle(&self) -> super::SourceHandle {
        super::SourceHandle(self.context.clone())
    }

    pub fn new(
        store: Store,
        cloud: CloudClient,
        discovery: Discovery,
    ) -> Result<(Self, TopologyReady), AdapterError> {
        Self::build(store, cloud, discovery, bootstrap::system_clock())
    }
    pub async fn run(self, mut on_status: impl FnMut(AdapterStatus)) -> Result<(), AdapterError> {
        let mut reported = self.context.diagnostics();
        on_status(reported.clone());
        let context = self.context.clone();
        let monitor = async {
            loop {
                let listener = context.status_changed.listen();
                let changed = context.changed.listen();
                let credentials = context.credentials_changed.listen();
                let commands = context.commands.changed.listen();
                let status = context.diagnostics();
                if status != reported {
                    on_status(status.clone());
                    reported = status;
                }
                future::or(
                    listener,
                    future::or(changed, future::or(credentials, commands)),
                )
                .await;
            }
        };
        let result = future::or(monitor, self.run_inner()).await;
        if let Err(error) = &result {
            self.context.ready.finish(Err(error.clone()));
            self.context.disconnect(error.to_string());
        }
        let status = self.context.diagnostics();
        if status != reported {
            on_status(status);
        }
        result
    }
    async fn run_inner(&self) -> Result<(), AdapterError> {
        future::or(
            local::run(
                self.context.clone(),
                self.discovery.clone(),
                #[cfg(test)]
                (self.test_ca.clone(), self.request_timeout),
            ),
            future::or(
                cloud::run(&self.cloud, &self.discovery, self.context.clone()),
                future::or(
                    super::credentials::run(&self.auth, self.context.clone()),
                    future::or(
                        super::notifications::run(self.context.clone()),
                        future::or(
                            super::refresh::cloud(self.context.clone()),
                            self.context.tasks.run(),
                        ),
                    ),
                ),
            ),
        )
        .await
    }
}
impl Drop for XiaomiAdapter {
    fn drop(&mut self) {
        self.context.stop();
        for physical in self.context.registry.list() {
            self.context.registry.remove(physical.key());
        }
        for slot in self.context.live.borrow().devices.values() {
            if let Some(loading) = &slot.loading {
                loading.life.close();
            }
        }
        self.context.tasks.clear();
        self.context.ready.finish(Err(AdapterError::Cancelled));
    }
}
