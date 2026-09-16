use std::time::Duration;

use async_io::Timer;
use futures_lite::future;
use mdns_sd::{IfKind, ScopedIp, ServiceDaemon, ServiceEvent};

use super::{
    DiscoveryError, MIOT_SERVICE_TYPE, MdnsEvent, NetworkSnapshot, RawResolvedService, ScopedIpv4,
};

pub struct DiscoveryBrowser {
    daemon: Option<ServiceDaemon>,
    receiver: flume::Receiver<ServiceEvent>,
}

impl DiscoveryBrowser {
    pub fn start(network: &NetworkSnapshot) -> Result<Self, DiscoveryError> {
        Self::start_with_daemon(network, ServiceDaemon::new())
    }

    fn start_with_daemon(
        network: &NetworkSnapshot,
        daemon: Result<ServiceDaemon, mdns_sd::Error>,
    ) -> Result<Self, DiscoveryError> {
        let daemon = daemon
            .map_err(|error| DiscoveryError::new(format!("cannot start mDNS browser: {error}")))?;
        if let Err(error) = daemon.disable_interface(IfKind::All) {
            let _ = daemon.shutdown();
            return Err(DiscoveryError::new(format!(
                "cannot limit mDNS interfaces: {error}"
            )));
        }
        for interface in network.interfaces() {
            if let Err(error) = daemon.enable_interface(IfKind::IndexV4(interface.index())) {
                let _ = daemon.shutdown();
                return Err(DiscoveryError::new(format!(
                    "cannot enable mDNS interface: {error}"
                )));
            }
        }
        let receiver = match daemon.browse(MIOT_SERVICE_TYPE) {
            Ok(receiver) => receiver,
            Err(error) => {
                let _ = daemon.shutdown();
                return Err(DiscoveryError::new(format!(
                    "cannot browse mDNS services: {error}"
                )));
            }
        };
        Ok(Self {
            daemon: Some(daemon),
            receiver,
        })
    }

    pub async fn next_event(&self) -> Result<MdnsEvent, DiscoveryError> {
        loop {
            let event = self
                .receiver
                .recv_async()
                .await
                .map_err(|_| DiscoveryError::new("mDNS browser stopped"))?;
            match event {
                ServiceEvent::ServiceResolved(service) => {
                    let profile = service
                        .get_property_val_str("profile")
                        .ok_or_else(|| DiscoveryError::new("mDNS service has no profile"))?
                        .to_owned();
                    let addresses = service
                        .addresses
                        .iter()
                        .filter_map(|address| match address {
                            ScopedIp::V4(scoped) => Some(ScopedIpv4 {
                                address: *scoped.addr(),
                                interface_indexes: scoped
                                    .interface_ids()
                                    .iter()
                                    .map(|interface| interface.index)
                                    .collect(),
                            }),
                            ScopedIp::V6(_) => None,
                            _ => None,
                        })
                        .collect();
                    return Ok(MdnsEvent::Resolved(RawResolvedService {
                        service_type: service.ty_domain.clone(),
                        instance: service.fullname.clone(),
                        port: service.port,
                        profile,
                        addresses,
                    }));
                }
                ServiceEvent::ServiceRemoved(service_type, instance) => {
                    return Ok(MdnsEvent::Removed {
                        service_type,
                        instance,
                    });
                }
                ServiceEvent::SearchStarted(_)
                | ServiceEvent::ServiceFound(_, _)
                | ServiceEvent::SearchStopped(_) => {}
                _ => {}
            }
        }
    }

    pub fn refresh(&mut self) -> Result<(), DiscoveryError> {
        let daemon = self
            .daemon
            .as_ref()
            .ok_or_else(|| DiscoveryError::new("mDNS browser is stopped"))?;
        daemon.stop_browse(MIOT_SERVICE_TYPE).map_err(|error| {
            DiscoveryError::new(format!("cannot refresh mDNS browser: {error}"))
        })?;
        self.receiver = daemon.browse(MIOT_SERVICE_TYPE).map_err(|error| {
            DiscoveryError::new(format!("cannot refresh mDNS browser: {error}"))
        })?;
        Ok(())
    }

    pub async fn stop(mut self) -> Result<(), DiscoveryError> {
        let Some(daemon) = self.daemon.take() else {
            return Ok(());
        };
        let receiver = daemon
            .shutdown()
            .map_err(|error| DiscoveryError::new(format!("cannot stop mDNS browser: {error}")))?;
        future::race(
            async {
                receiver
                    .recv_async()
                    .await
                    .map(|_| ())
                    .map_err(|_| DiscoveryError::new("mDNS shutdown response was lost"))
            },
            async {
                Timer::after(Duration::from_secs(2)).await;
                Err(DiscoveryError::new("mDNS shutdown timed out"))
            },
        )
        .await
    }
}

impl Drop for DiscoveryBrowser {
    fn drop(&mut self) {
        if let Some(daemon) = self.daemon.take() {
            let _ = daemon.shutdown();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use futures_lite::future::block_on;

    use super::*;
    use crate::xiaomi::discovery::{DefaultRoute, InterfaceRecord, LinkType, RouteGateway};

    #[test]
    fn isolated_browser_can_refresh_and_stop() {
        let network = NetworkSnapshot::select(
            vec![InterfaceRecord {
                index: 999_999,
                name: "en-test".into(),
                address: Ipv4Addr::new(192, 0, 2, 1),
                netmask: Ipv4Addr::new(255, 255, 255, 0),
                up: true,
                point_to_point: false,
                loopback: false,
                link_type: LinkType::Ethernet,
            }],
            Some(DefaultRoute {
                interface_index: 999_999,
                gateway: RouteGateway::Ipv4(Ipv4Addr::new(192, 0, 2, 254)),
            }),
        )
        .unwrap();
        let mut browser =
            DiscoveryBrowser::start_with_daemon(&network, ServiceDaemon::new_with_port(0)).unwrap();
        browser.refresh().unwrap();
        block_on(browser.stop()).unwrap();
    }
}
