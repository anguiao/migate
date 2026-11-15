use crate::{
    storage::XiaomiRecord,
    xiaomi::{
        central::{self, CentralError, SessionDriver, SessionHandle},
        cloud::{self, NotificationDriver, NotificationError, NotificationHandle},
        lan,
    },
};
use futures_util::future::LocalBoxFuture;
use std::{
    net::{Ipv4Addr, SocketAddr},
    time::Duration,
};

pub(super) trait Network {
    fn gateway<'a>(
        &'a self,
        address: SocketAddr,
        credentials: &'a XiaomiRecord,
    ) -> LocalBoxFuture<'a, Result<(SessionHandle, SessionDriver), CentralError>>;
    fn notifications<'a>(
        &'a self,
        credentials: &'a XiaomiRecord,
    ) -> LocalBoxFuture<'a, Result<(NotificationHandle, NotificationDriver), NotificationError>>;
    fn lan_target(&self, address: Ipv4Addr) -> (SocketAddr, Duration);
    fn discover_lan<'a>(
        &'a self,
        discovery: &'a lan::Discovery,
        did: u64,
    ) -> LocalBoxFuture<'a, Option<Ipv4Addr>>;
}

pub(super) struct XiaomiNetwork;

impl Network for XiaomiNetwork {
    fn gateway<'a>(
        &'a self,
        address: SocketAddr,
        credentials: &'a XiaomiRecord,
    ) -> LocalBoxFuture<'a, Result<(SessionHandle, SessionDriver), CentralError>> {
        Box::pin(central::connect(address, credentials))
    }

    fn notifications<'a>(
        &'a self,
        credentials: &'a XiaomiRecord,
    ) -> LocalBoxFuture<'a, Result<(NotificationHandle, NotificationDriver), NotificationError>>
    {
        Box::pin(cloud::connect_notifications(credentials))
    }

    fn lan_target(&self, address: Ipv4Addr) -> (SocketAddr, Duration) {
        ((address, lan::PORT).into(), lan::TIMEOUT)
    }

    fn discover_lan<'a>(
        &'a self,
        discovery: &'a lan::Discovery,
        did: u64,
    ) -> LocalBoxFuture<'a, Option<Ipv4Addr>> {
        Box::pin(discovery.address(did))
    }
}
