use super::{
    PairingEvent,
    model::{self, basic_info},
    pairing,
    storage::StoreAdapter,
};
use crate::{
    RuntimeError,
    device::DeviceKey,
    storage::{Identity, StorageError, Store},
};
use rs_matter::{
    Matter,
    crypto::{Crypto, default_crypto},
    dm::devices::test::{DAC_PRIVKEY, TEST_DEV_ATT, TEST_DEV_COMM},
    im::{InteractionModel, InteractionModelState},
    respond::DefaultResponder,
    transport::{
        MATTER_SOCKET_BIND_ADDR, exchange::MatterBuffers, network::mdns::astro::AstroMdns,
    },
};
use std::{net::UdpSocket, rc::Rc, time::Duration};

const WINDOW_SECONDS: u16 = 900;

#[cfg(test)]
#[path = "tests/bridge.rs"]
mod tests;

/// Service registrations belong to the bridge, not one frozen Matter instance.
pub(crate) struct Advertisement {
    mdns: AstroMdns,
    port: u16,
}
impl Advertisement {
    pub(crate) fn new(port: u16) -> Self {
        Self {
            mdns: AstroMdns::new(),
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
}

/// One frozen Matter instance and storage failures that outlive its run future.
pub(crate) struct Bridge<'a> {
    identity: &'a Identity,
    pub(super) store: StoreAdapter,
    /// Explicit committed published references, identities and root version
    /// captured by the application; a running instance never rereads the
    /// device configuration tables.
    frozen: (Vec<model::FrozenEntry>, u32),
    renamer: Rc<dyn LabelWriter>,
}
impl<'a> Bridge<'a> {
    pub(crate) fn new(
        entries: Vec<model::FrozenEntry>,
        root_version: u32,
        store: &Store,
        identity: &'a Identity,
        renamer: Rc<dyn LabelWriter>,
    ) -> Self {
        Self {
            identity,
            store: StoreAdapter::new(store.matter()),
            frozen: (entries, root_version),
            renamer,
        }
    }

    /// Check for a recorded storage failure, including after cancelling `run`.
    pub(crate) fn check_failure(&self) -> Result<(), StorageError> {
        self.store.check_failure()
    }

    pub(crate) async fn run_with_advertisement(
        &self,
        advertisement: &mut Advertisement,
        report_pairing: impl Fn(PairingEvent) -> std::io::Result<()>,
    ) -> Result<(), RuntimeError> {
        self.check_failure()?;
        let (frozen, root_version) = self.frozen.clone();
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
        matter.with_state(|state| state.basic_info_settings = settings.clone());
        let buffers: MatterBuffers = MatterBuffers::new();
        let state: InteractionModelState = InteractionModelState::new();
        let crypto = default_crypto(rand::rng(), DAC_PRIVKEY);
        let handler = model::handler_from(frozen, crypto.rand()?, self.renamer.clone());
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
        let mdns = async {
            advertisement
                .mdns
                .run(&matter)
                .await
                .map_err(|e| format!("mDNS service failed: {e}").into())
        };
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
        result
    }
}
