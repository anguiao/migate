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
#[cfg(target_os = "macos")]
use rs_matter::transport::network::mdns::astro::AstroMdns as PlatformMdns;
use rs_matter::{
    Matter,
    crypto::{Crypto, default_crypto},
    dm::devices::test::{DAC_PRIVKEY, TEST_DEV_ATT, TEST_DEV_COMM},
    im::{InteractionModel, InteractionModelState},
    respond::DefaultResponder,
    transport::{MATTER_SOCKET_BIND_ADDR, exchange::MatterBuffers},
};
#[cfg(target_os = "linux")]
use rs_matter::{transport::network::mdns::avahi::AvahiMdns as PlatformMdns, utils::zbus};
use std::{net::UdpSocket, rc::Rc, time::Duration};

const WINDOW_SECONDS: u16 = 900;

#[cfg(target_os = "linux")]
mod avahi;
#[cfg(test)]
mod tests;

/// Service registrations survive individual Matter runs.
pub(crate) struct Advertisement {
    mdns: Option<PlatformMdns>,
    port: u16,
}
impl Advertisement {
    pub(crate) fn new(port: u16) -> Self {
        Self { mdns: None, port }
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

    async fn run(
        &mut self,
        matter: &Matter<'_>,
        #[cfg(target_os = "linux")] rebuild: &avahi::Rebuild,
    ) -> Result<(), RuntimeError> {
        let mdns = match &mut self.mdns {
            Some(mdns) => mdns,
            slot @ None => {
                #[cfg(target_os = "macos")]
                let mdns = PlatformMdns::new();
                #[cfg(target_os = "linux")]
                let mdns =
                    PlatformMdns::new(zbus::Connection::system().await.map_err(|error| {
                        format!("Failed to connect to the system D-Bus for Matter mDNS: {error}")
                    })?);
                slot.insert(mdns)
            }
        };
        #[cfg(target_os = "macos")]
        let result = mdns.run(matter).await;
        #[cfg(target_os = "linux")]
        let result = rebuild.run(mdns, matter).await;
        result.map_err(|error| {
            #[cfg(target_os = "macos")]
            let operation = "Failed to advertise Matter services with Bonjour";
            #[cfg(target_os = "linux")]
            let operation = "Failed to advertise Matter services with Avahi";
            format!("{operation}: {error}").into()
        })
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
    #[cfg(target_os = "linux")]
    advertisement_rebuild: avahi::Rebuild,
}
impl<'a> Bridge<'a> {
    pub(crate) fn new(store: &Store, identity: &'a Identity, renamer: Rc<dyn LabelWriter>) -> Self {
        Self {
            identity,
            store: StoreAdapter::new(store.matter()),
            renamer,
            #[cfg(target_os = "linux")]
            advertisement_rebuild: avahi::Rebuild::default(),
        }
    }

    /// Check for a recorded storage failure, including after cancelling `run`.
    pub(crate) fn check_failure(&self) -> Result<(), StorageError> {
        self.store.check_failure()
    }

    /// Return whether the runner must finish an asynchronous mDNS update before rebuilding.
    pub(crate) fn request_rebuild(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            self.advertisement_rebuild.request()
        }
        #[cfg(target_os = "macos")]
        {
            false
        }
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
        let transport = async {
            matter
                .run(&crypto, &socket, &socket, &socket)
                .await
                .map_err(|e| format!("Matter transport task failed: {e}").into())
        };
        let mdns = advertisement.run(
            &matter,
            #[cfg(target_os = "linux")]
            &self.advertisement_rebuild,
        );
        let respond = async {
            responder
                .run::<4, 4>()
                .await
                .map_err(|e| format!("Matter responder task failed: {e}").into())
        };
        let job = async {
            im.run()
                .await
                .map_err(|e| format!("Matter model task failed: {e}").into())
        };
        use futures_lite::future::or;
        let result = or(
            fatal,
            or(timeout, or(transport, or(mdns, or(respond, job)))),
        )
        .await;
        // A protocol task may return an error before the storage watcher is polled again.
        self.check_failure()?;
        result?;
        #[cfg(target_os = "linux")]
        if self.advertisement_rebuild.finished() {
            return Ok(());
        }
        Err("Matter instance ended unexpectedly".into())
    }
}
