mod bridged_info;
mod common;
mod device_bridge;
mod light;
mod model;
mod pairing;
mod sensors;
mod storage;

pub use device_bridge::{DeviceBridge, DeviceBridgeModel};
pub use pairing::PairingEvent;

use crate::{
    RuntimeError,
    storage::{Identity, MatterStore, StorageError},
    virtual_device::VirtualLight,
};
use model::{NODE, basic_info, initialize_basic_info};
use rs_matter::{
    Matter,
    crypto::{Crypto, default_crypto},
    dm::{
        Dataver,
        clusters::{identify, scenes},
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
use storage::StoreAdapter;

const WINDOW_SECONDS: u16 = 900;

/// The fixed virtual-light bridge and storage failures that outlive its run future.
pub struct Bridge<'a> {
    light: &'a VirtualLight,
    identity: &'a Identity,
    store: StoreAdapter,
}

impl<'a> Bridge<'a> {
    pub fn new(light: &'a VirtualLight, identity: &'a Identity, store: MatterStore) -> Self {
        Self {
            light,
            identity,
            store: StoreAdapter::new(store),
        }
    }

    /// Check for a recorded storage failure, including after cancelling `run`.
    pub fn check_failure(&self) -> Result<(), StorageError> {
        self.store.check_failure()
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
        let store = &self.store;
        let label = bridged_info::load_label(store.storage())?;
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
        let mut random = crypto.rand()?;
        let scenes = scenes::ScenesState::<16>::new();
        let identify = identify::IdentifyHandler::new(Dataver::new_rand(&mut random));
        let on_off = model::on_off(self.light, &scenes, Dataver::new_rand(&mut random));
        let handler = model::handler(
            self.light,
            &self.identity.light_id,
            label,
            &identify,
            &scenes,
            &on_off,
            random,
        );
        let model = (NODE, handler);
        let im = InteractionModel::new(&matter, &crypto, &buffers, model, &kv, &state);
        store.with_context("restore Matter model", im.startup().await)?;
        // Initialize only after every existing blob has been restored successfully.
        store.with_context(
            "initialize the default Matter node label",
            initialize_basic_info(&matter, &kv, missing_basic_info),
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

#[cfg(test)]
mod tests;
