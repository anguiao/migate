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
/// One shared home session and one persistent command consumer per function.
pub struct XiaomiAdapter {
    pub(super) cloud: CloudClient,
    pub(super) auth: AuthService,
    pub(super) discovery: Discovery,
    pub(super) context: Rc<Context>,
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
        let auth = AuthService::new(store.xiaomi(), cloud.clone())
            .map_err(crate::xiaomi::auth::AuthError::Cloud)?;
        Self::with_services(
            store,
            cloud,
            discovery,
            bootstrap::system_clock(),
            auth,
            Rc::new(super::network::XiaomiNetwork),
        )
    }
    pub async fn run(self, mut on_status: impl FnMut(AdapterStatus)) -> Result<(), AdapterError> {
        let mut reported = None;
        let context = self.context.clone();
        let monitor = async {
            loop {
                let listener = context.status_changed.listen();
                let changed = context.changed.listen();
                let credentials = context.credentials_changed.listen();
                let commands = context.commands.changed.listen();
                let status = context.diagnostics();
                if reported.as_ref() != Some(&status) {
                    on_status(status.clone());
                    reported = Some(status);
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
        if reported.as_ref() != Some(&status) {
            on_status(status);
        }
        result
    }
    async fn run_inner(&self) -> Result<(), AdapterError> {
        future::or(
            local::run(self.context.clone(), self.discovery.clone()),
            future::or(
                cloud::run(&self.cloud, &self.discovery, self.context.clone()),
                future::or(
                    super::credentials::run(&self.auth, self.context.clone()),
                    future::or(
                        super::notifications::run(self.context.clone()),
                        future::or(
                            super::refresh::cloud(self.context.clone()),
                            future::or(
                                super::lan::watch_network(
                                    self.context.clone(),
                                    self.discovery.clone(),
                                ),
                                future::or(
                                    Box::pin(super::refresh::direct(self.context.clone())),
                                    future::or(
                                        super::refresh::polling(self.context.clone()),
                                        future::or(
                                            super::lan::run(self.context.clone()),
                                            self.context.tasks.run(),
                                        ),
                                    ),
                                ),
                            ),
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
        self.context.tasks.clear();
        self.context.ready.finish(Err(AdapterError::Cancelled));
    }
}
