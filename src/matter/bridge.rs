use super::{
    PairingEvent,
    model::{self, basic_info, initialize_basic_info},
    pairing,
    storage::StoreAdapter,
};
use crate::{
    RuntimeError,
    device::DeviceRegistry,
    lighting::LightingExecutor,
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
use std::{net::UdpSocket, rc::Rc, time::Duration};

const WINDOW_SECONDS: u16 = 900;

/// Dynamic device bridge and storage failures that outlive its run future.
pub struct Bridge<'a> {
    registry: DeviceRegistry,
    storage: Store,
    identity: &'a Identity,
    executor: Rc<LightingExecutor>,
    pub(super) store: StoreAdapter,
}
impl<'a> Bridge<'a> {
    pub(crate) fn new(
        registry: &DeviceRegistry,
        store: &Store,
        identity: &'a Identity,
        executor: Rc<LightingExecutor>,
    ) -> Self {
        Self {
            registry: registry.clone(),
            storage: store.clone(),
            identity,
            executor,
            store: StoreAdapter::new(store.matter()),
        }
    }

    /// Check for a recorded storage failure, including after cancelling `run`.
    pub fn check_failure(&self) -> Result<(), StorageError> {
        self.store.check_failure()
    }

    /// Run protocol services on the caller's local executor until failure or cancellation.
    /// Bind the requested UDP port; zero asks the OS to choose an available port.
    /// Report pairing window changes through the caller's output callback.
    /// A relation, source-instance or fixed-description change replaces Matter and cancels its
    /// unfinished lighting schedules while source connections keep running.
    /// After dropping this future, call `check_failure` before treating cancellation as success.
    pub async fn run(
        &self,
        port: u16,
        report_pairing: impl Fn(PairingEvent) -> std::io::Result<()>,
    ) -> Result<(), RuntimeError> {
        let mut retry = None;
        loop {
            let rebuild = self.run_instance(port, &report_pairing, retry).await?;
            // The old instance and all of its tasks and sockets have been
            // dropped. No old exchange can observe this configuration commit.
            log::info!("Matter configuration changed; rebuilding protocol services");
            retry = rebuild.apply();
        }
    }

    // Returning drops every object belonging to the old protocol instance
    // before the caller resolves and installs the latest configuration.
    async fn run_instance(
        &self,
        port: u16,
        report_pairing: &impl Fn(PairingEvent) -> std::io::Result<()>,
        retry: Option<model::Retry>,
    ) -> Result<model::Rebuild, RuntimeError> {
        self.check_failure()?;
        let store = &self.store;
        let missing_basic_info = !store
            .storage()
            .contains(rs_matter::persist::BASIC_INFO_KEY)?;
        let mut bind_addr = MATTER_SOCKET_BIND_ADDR;
        bind_addr.set_port(port);
        let socket = async_io::Async::<UdpSocket>::bind(bind_addr)
            .map_err(|e| format!("Failed to bind Matter UDP port {port}: {e}"))?;
        let bound_port = socket.get_ref().local_addr()?.port();
        let info = basic_info(self.identity);
        let matter = Matter::new(&info, TEST_DEV_COMM, &TEST_DEV_ATT, bound_port);
        let kv = matter.kv(store.clone());
        store.with_context("restore Matter data", matter.startup(&kv))?;
        let buffers: MatterBuffers = MatterBuffers::new();
        let state: EthInteractionModelState =
            EthInteractionModelState::new(EthNetwork::new_default());
        let crypto = default_crypto(rand::rng(), DAC_PRIVKEY);
        let handler = model::handler(
            &self.registry,
            &self.storage,
            store.clone(),
            self.executor.clone(),
            crypto.rand()?,
            retry,
        )?;
        let im = InteractionModel::new(&matter, &crypto, &buffers, &handler, &kv, &state);
        store.with_context("restore Matter model", im.startup().await)?;
        // Initialize only after every existing blob has been restored successfully.
        store.with_context(
            "initialize the default Matter node label",
            initialize_basic_info(&matter, &kv, missing_basic_info),
        )?;
        store.with_context("initialize Matter topology", handler.initialize(&im))?;
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
        result?;
        handler
            .take_rebuild()
            .ok_or_else(|| "Matter services ended without a requested rebuild".into())
    }
}
