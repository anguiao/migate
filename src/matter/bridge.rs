use super::{
    PairingEvent,
    model::{self, basic_info},
    pairing,
    storage::StoreAdapter,
};
use crate::{
    RuntimeError,
    configuration::Scope,
    device::{DeviceKey, FunctionKey},
    storage::{Identity, StorageError, Store},
};
use rs_matter::{
    Matter,
    crypto::{Crypto, default_crypto},
    dm::devices::test::{DAC_PRIVKEY, TEST_DEV_ATT, TEST_DEV_COMM},
    error::Error,
    im::{InteractionModel, InteractionModelState},
    respond::DefaultResponder,
    transport::{MATTER_SOCKET_BIND_ADDR, exchange::MatterBuffers},
};
use std::{future::Future, net::UdpSocket, rc::Rc, task::Poll, time::Duration};

const WINDOW_SECONDS: u16 = 900;

#[cfg(target_os = "linux")]
#[path = "bridge/avahi.rs"]
mod mdns;
#[cfg(target_os = "macos")]
#[path = "bridge/bonjour.rs"]
mod mdns;
#[cfg(test)]
mod tests;

/// Service registrations survive individual Matter runs.
pub(crate) struct Advertisement {
    publisher: mdns::Publisher,
    port: u16,
}
impl Advertisement {
    pub(crate) fn new(port: u16) -> Self {
        Self {
            publisher: mdns::Publisher::default(),
            port,
        }
    }

    fn bind(&mut self) -> Result<async_io::Async<UdpSocket>, RuntimeError> {
        let mut address = MATTER_SOCKET_BIND_ADDR;
        address.set_port(self.port);
        let socket = async_io::Async::<UdpSocket>::bind(address)
            .map_err(|error| format!("Failed to bind Matter UDP port {}: {error}", self.port))?;
        // A dynamically chosen port must remain valid for the retained mDNS
        // registrations. Each instance still owns and releases its socket.
        self.port = socket.get_ref().local_addr()?.port();
        Ok(socket)
    }
}

/// The one application configuration entry used by Matter label writes.
pub(crate) trait LabelWriter {
    fn write_label(&self, key: &DeviceKey, label: &str) -> Result<(), StorageError>;
    fn relay_label(&self, scope: &Scope, key: &FunctionKey) -> Option<String>;
    fn write_relay_label(
        &self,
        scope: &Scope,
        key: &FunctionKey,
        label: &str,
    ) -> Result<(), StorageError>;
}

/// Matter persistence and label access, including failures that outlive a run future.
pub(crate) struct Bridge<'a> {
    identity: &'a Identity,
    pub(super) store: StoreAdapter,
    renamer: Rc<dyn LabelWriter>,
    advertisement_rebuild: mdns::Rebuild,
}
impl<'a> Bridge<'a> {
    pub(crate) fn new(store: &Store, identity: &'a Identity, renamer: Rc<dyn LabelWriter>) -> Self {
        Self {
            identity,
            store: StoreAdapter::new(store.matter()),
            renamer,
            advertisement_rebuild: Default::default(),
        }
    }

    /// Check for a recorded storage failure, including after cancelling `run`.
    pub(crate) fn check_failure(&self) -> Result<(), StorageError> {
        self.store.check_failure()
    }

    /// A pending request finishes when the advertisement runner returns.
    pub(crate) fn request_rebuild(&self) -> Poll<()> {
        self.advertisement_rebuild.request()
    }

    /// Run until the advertisement is ready for a requested rebuild or a task fails.
    pub(crate) async fn run_with_advertisement(
        &self,
        configuration: model::MatterConfiguration,
        advertisement: &mut Advertisement,
        report_pairing: impl Fn(PairingEvent) -> std::io::Result<()>,
    ) -> Result<(), RuntimeError> {
        self.check_failure()?;
        let model::MatterConfiguration {
            entries,
            root_version,
        } = configuration;
        let store = &self.store;
        let socket = advertisement.bind()?;
        let bound_port = advertisement.port;
        let info = basic_info(self.identity);
        let matter = Matter::new(&info, TEST_DEV_COMM, &TEST_DEV_ATT, bound_port);
        let kv = matter.kv(store.clone());
        // Restore protocol records with rs-matter's recovery policy, then align
        // BasicInfoSettings to the committed root version. StoreAdapter keeps
        // database failures fatal throughout startup and serving.
        store.with_context("restore Matter data", matter.startup(&kv))?;
        let settings = store.with_context(
            "prepare the root ConfigurationVersion",
            model::prepare_basic_info(&kv, root_version),
        )?;
        matter.with_state(|state| *state.basic_info_settings = settings.clone());
        let buffers: MatterBuffers = MatterBuffers::new();
        let state: InteractionModelState = InteractionModelState::new();
        let crypto = default_crypto(rand::rng(), DAC_PRIVKEY);
        let handler = model::handler_from(&matter, entries, crypto.rand()?, self.renamer.clone());
        let im = InteractionModel::new(&matter, &crypto, &buffers, &handler, &kv, &state);
        store.with_context("restore Matter model", im.startup().await)?;
        let persist_kv = matter.kv(store.clone());
        store.with_context(
            "persist the root ConfigurationVersion",
            model::persist_basic_info(&persist_kv, &settings),
        )?;
        log::info!("Matter UDP listening on port {bound_port}");
        let responder = DefaultResponder::new(&im);
        let opened = !matter.has_fabrics();
        if opened {
            matter.open_basic_comm_window(WINDOW_SECONDS, &crypto, &())?;
            report_pairing(pairing::opened(&info, WINDOW_SECONDS)?)?;
        }
        let timeout = async {
            if opened {
                async_io::Timer::after(Duration::from_secs(WINDOW_SECONDS.into())).await;
                if !matter.has_fabrics() {
                    report_pairing(PairingEvent::Expired)?;
                }
            }
            std::future::pending::<Result<(), RuntimeError>>().await
        };
        let fatal = async { Err::<(), RuntimeError>(store.wait_failure().await.into()) };
        let transport = protocol_task("transport", matter.run(&crypto, &socket, &socket, &socket));
        let mdns = advertisement
            .publisher
            .run(&matter, &self.advertisement_rebuild);
        let respond = protocol_task("responder", responder.run::<4, 4>());
        let job = protocol_task("model", im.run());
        use futures_lite::future::or;
        let result = or(
            fatal,
            or(timeout, or(transport, or(mdns, or(respond, job)))),
        )
        .await;
        // A protocol task may return an error before the storage watcher is polled again.
        self.check_failure()?;
        result
    }
}

/// Only the advertisement runner can return successfully for a requested rebuild.
async fn protocol_task(
    name: &str,
    task: impl Future<Output = Result<(), Error>>,
) -> Result<(), RuntimeError> {
    task.await
        .map_err(|error| format!("Matter {name} task failed: {error}"))?;
    Err("Matter instance ended unexpectedly".into())
}
