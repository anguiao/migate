use super::{
    PairingEvent,
    model::{self, basic_info},
    pairing,
    storage::StoreAdapter,
};
use crate::{
    RuntimeError,
    device::DeviceRegistry,
    storage::{Identity, StorageError, Store},
};
use rs_matter::{
    Matter,
    crypto::{Crypto, default_crypto},
    dm::{
        devices::test::{DAC_PRIVKEY, TEST_DEV_ATT, TEST_DEV_COMM},
        networks::eth::EthNetwork,
    },
    im::{EthInteractionModelState, InteractionModel},
    respond::DefaultResponder,
    transport::{
        MATTER_SOCKET_BIND_ADDR, exchange::MatterBuffers, network::mdns::astro::AstroMdns,
    },
};
use std::{net::UdpSocket, time::Duration};

const WINDOW_SECONDS: u16 = 900;

/// One frozen Matter instance and storage failures that outlive its run future.
pub struct Bridge<'a> {
    storage: Store,
    identity: &'a Identity,
    pub(super) store: StoreAdapter,
    /// Committed published refs, identities and root version captured at
    /// construction; a running instance never rereads newer fixed rows.
    frozen: Result<(Vec<model::FrozenEntry>, u32), StorageError>,
}
impl<'a> Bridge<'a> {
    pub fn new(registry: &DeviceRegistry, store: &Store, identity: &'a Identity) -> Self {
        Self {
            storage: store.clone(),
            identity,
            store: StoreAdapter::new(store.matter()),
            frozen: model::capture(registry, store),
        }
    }

    /// Check for a recorded storage failure, including after cancelling `run`.
    pub fn check_failure(&self) -> Result<(), StorageError> {
        self.store.check_failure()?;
        self.frozen.clone().map(|_| ())
    }

    /// Run protocol services on the caller's local executor until failure or cancellation.
    /// Bind the requested UDP port; zero asks the OS to choose an available port.
    /// Report pairing window changes through the caller's output callback.
    /// After dropping this future, call `check_failure` before treating cancellation as success.
    pub async fn run(
        &self,
        port: u16,
        report_pairing: impl Fn(PairingEvent) -> std::io::Result<()>,
    ) -> Result<(), RuntimeError> {
        self.check_failure()?;
        let (frozen, root_version) = self.frozen.clone()?;
        let store = &self.store;
        let mut bind_addr = MATTER_SOCKET_BIND_ADDR;
        bind_addr.set_port(port);
        let socket = async_io::Async::<UdpSocket>::bind(bind_addr)
            .map_err(|e| format!("Failed to bind Matter UDP port {port}: {e}"))?;
        let bound_port = socket.get_ref().local_addr()?.port();
        let info = basic_info(self.identity);
        let matter = Matter::new(&info, TEST_DEV_COMM, &TEST_DEV_ATT, bound_port);
        let kv = matter.kv(store.clone());
        // Validate every persisted protocol blob first (loads only). Then align
        // the in-memory BasicInfoSettings to the committed root, start the
        // interaction model, and persist the complete settings only after all
        // protocol data has loaded; corruption never leaves a write behind.
        store.with_context("restore Matter data", matter.startup(&kv))?;
        let settings = store.with_context(
            "prepare the root ConfigurationVersion",
            model::prepare_basic_info(&kv, root_version),
        )?;
        matter.with_state(|state| state.basic_info_settings = settings.clone());
        let buffers: MatterBuffers = MatterBuffers::new();
        let state: EthInteractionModelState =
            EthInteractionModelState::new(EthNetwork::new_default());
        let crypto = default_crypto(rand::rng(), DAC_PRIVKEY);
        let handler = model::handler_from(frozen, &self.storage, store.clone(), crypto.rand()?)?;
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
            AstroMdns::new()
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
