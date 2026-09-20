use super::{common, device_bridge::DeviceBridgeModel, storage::StoreAdapter};
use crate::{
    RuntimeError,
    device::DeviceService,
    storage::{DeviceStore, Identity, StorageError, Store},
};
use futures_lite::future;
use rs_matter::{
    Matter,
    crypto::{Crypto, default_crypto},
    dm::{
        devices::test::{DAC_PRIVKEY, TEST_DEV_ATT, TEST_DEV_COMM},
        endpoints,
        networks::{SysNetifs, eth::EthNetwork},
    },
    im::{EthInteractionModelState, InteractionModel},
    persist::BASIC_INFO_KEY,
    respond::DefaultResponder,
    transport::{
        MATTER_SOCKET_BIND_ADDR, exchange::MatterBuffers, network::mdns::astro::AstroMdns,
    },
};
use std::{net::UdpSocket, time::Duration};

const WINDOW_SECONDS: u16 = 900;

/// Executable Matter bridge that preserves the live transport across model rebuilds.
pub struct DeviceBridge<'a> {
    service: &'a DeviceService,
    identity: &'a Identity,
    devices: DeviceStore,
    store: StoreAdapter,
}

impl<'a> DeviceBridge<'a> {
    pub fn new(service: &'a DeviceService, identity: &'a Identity, store: Store) -> Self {
        Self {
            service,
            identity,
            devices: store.devices(),
            store: StoreAdapter::new(store.matter()),
        }
    }

    pub fn check_failure(&self) -> Result<(), StorageError> {
        self.store.check_failure()
    }

    #[cfg(test)]
    pub(super) fn store_for_test(&self) -> StoreAdapter {
        self.store.clone()
    }

    pub async fn run(
        &self,
        port: u16,
        report_pairing: impl Fn(super::PairingEvent) -> std::io::Result<()>,
    ) -> Result<(), RuntimeError> {
        self.check_failure()?;
        let mut bind_addr = MATTER_SOCKET_BIND_ADDR;
        bind_addr.set_port(port);
        let socket = async_io::Async::<UdpSocket>::bind(bind_addr)
            .map_err(|error| format!("Failed to bind Matter UDP port {port}: {error}"))?;
        let bound_port = socket.get_ref().local_addr()?.port();
        let info = common::basic_info(self.identity);
        let matter = Matter::new(&info, TEST_DEV_COMM, &TEST_DEV_ATT, bound_port);
        let kv = matter.kv(self.store.clone());
        self.store
            .with_context("restore Matter data", matter.startup(&kv))?;
        let crypto = default_crypto(rand::rng(), DAC_PRIVKEY);
        let buffers: MatterBuffers = MatterBuffers::new();
        let state: EthInteractionModelState =
            EthInteractionModelState::new(EthNetwork::new_default());
        let mut initial_model = Some(DeviceBridgeModel::with_store(
            self.service.clone(),
            self.devices.clone(),
            self.store.clone(),
        )?);
        {
            let model = initial_model.as_ref().expect("initial model is present");
            let mut random = crypto.rand()?;
            let handler = endpoints::EthSysHandlerBuilder::new()
                .netif_diag(&SysNetifs)
                .build(&mut random)
                .chain(|endpoint, _| endpoint != 0, model);
            let im =
                InteractionModel::new(&matter, &crypto, &buffers, (model, &handler), &kv, &state);
            self.store
                .with_context("restore Matter model", im.startup().await)?;
        }
        let missing_basic_info = !self.store.storage().contains(BASIC_INFO_KEY)?;
        common::initialize_basic_info(&matter, &kv, missing_basic_info).map_err(|error| {
            StorageError::new(
                self.store.storage().path(),
                "initialize Matter basic information",
                error,
            )
        })?;
        let opened = !matter.has_fabrics();
        if opened {
            matter.open_basic_comm_window(WINDOW_SECONDS, &crypto, &())?;
            report_pairing(super::pairing::opened(&info, WINDOW_SECONDS)?)?;
        }
        let timeout = async {
            if opened {
                async_io::Timer::after(Duration::from_secs(WINDOW_SECONDS.into())).await;
                if !matter.has_fabrics() {
                    report_pairing(super::PairingEvent::Expired)?;
                }
            }
            std::future::pending::<Result<(), RuntimeError>>().await
        };
        let fatal = async { Err::<(), RuntimeError>(self.store.wait_failure().await.into()) };
        let transport = async {
            matter
                .run(&crypto, &socket, &socket, &socket)
                .await
                .map_err(|error| format!("Matter transport task failed: {error}").into())
        };
        let mdns = async {
            AstroMdns::new()
                .run(&matter)
                .await
                .map_err(|error| format!("mDNS service failed: {error}").into())
        };
        let protocol = async {
            loop {
                let (model, restore) = match initial_model.take() {
                    Some(model) => (model, false),
                    None => (
                        DeviceBridgeModel::with_store(
                            self.service.clone(),
                            self.devices.clone(),
                            self.store.clone(),
                        )?,
                        true,
                    ),
                };
                let mut random = crypto.rand()?;
                let handler = endpoints::EthSysHandlerBuilder::new()
                    .netif_diag(&SysNetifs)
                    .build(&mut random)
                    .chain(|endpoint, _| endpoint != 0, &model);
                let im = InteractionModel::new(
                    &matter,
                    &crypto,
                    &buffers,
                    (&model, &handler),
                    &kv,
                    &state,
                );
                if restore {
                    self.store
                        .with_context("restore Matter model", im.startup().await)?;
                }
                let responder = DefaultResponder::new(&im);
                let outcome = future::or(
                    async { responder.run::<4, 4>().await.map(|_| false) },
                    future::or(async { im.run().await.map(|_| false) }, async {
                        model.rebuild_requested().await;
                        Ok(true)
                    }),
                )
                .await
                .map_err(|error| -> RuntimeError {
                    format!("Matter model task failed: {error}").into()
                })?;
                if !outcome {
                    return Err("Matter model stopped unexpectedly".into());
                }
            }
        };
        log::info!("Matter UDP listening on port {bound_port}");
        let result = future::or(
            fatal,
            future::or(timeout, future::or(transport, future::or(mdns, protocol))),
        )
        .await;
        self.check_failure()?;
        result
    }
}
