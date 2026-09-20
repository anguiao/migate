use crate::xiaomi::{
    catalog::CatalogDevice,
    discovery::{NetworkEpoch, NetworkInterface},
};
use std::net::{Ipv4Addr, SocketAddrV4};

use super::{LanError, LanErrorKind, discovery::LanHelloCandidate};

const LAN_PIDS: [i64; 4] = [0, 8, 12, 23];

struct TargetIdentity {
    did: u64,
    model: String,
    token: [u8; 16],
}

#[derive(Clone)]
pub struct LanTarget {
    did: u64,
    model: String,
    address: SocketAddrV4,
    interface: NetworkInterface,
    epoch: NetworkEpoch,
    token: [u8; 16],
    timestamp_hint: Option<u32>,
}

impl std::fmt::Debug for LanTarget {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LanTarget")
            .field("did", &self.did)
            .field("model", &self.model)
            .field("address", &self.address)
            .field("interface", &self.interface)
            .field("epoch", &self.epoch)
            .field("token", &"[REDACTED]")
            .finish()
    }
}

impl LanTarget {
    pub fn from_catalog(
        device: &CatalogDevice,
        selected_home: &str,
        interface: NetworkInterface,
        epoch: NetworkEpoch,
    ) -> Result<Self, LanError> {
        let identity = target_identity(device, selected_home)?;
        let ip = device
            .local_ip
            .as_deref()
            .and_then(|value| value.parse::<Ipv4Addr>().ok())
            .ok_or_else(|| LanError::new("select LAN target", LanErrorKind::InvalidInput))?;
        validate_unicast_target(ip, &interface)?;
        let address = SocketAddrV4::new(ip, 54321);
        Ok(Self {
            did: identity.did,
            model: identity.model,
            address,
            interface,
            epoch,
            token: identity.token,
            timestamp_hint: None,
        })
    }

    pub fn from_candidate(
        device: &CatalogDevice,
        selected_home: &str,
        interface: NetworkInterface,
        epoch: NetworkEpoch,
        candidate: &LanHelloCandidate,
    ) -> Result<Self, LanError> {
        let identity = target_identity(device, selected_home)?;
        if candidate.did != identity.did
            || candidate.interface_index != interface.index()
            || candidate.epoch != epoch
            || candidate.address.port() != 54321
        {
            return Err(LanError::new(
                "select LAN candidate",
                LanErrorKind::InvalidInput,
            ));
        }
        validate_unicast_target(*candidate.address.ip(), &interface)?;
        Ok(Self {
            did: identity.did,
            model: identity.model,
            address: candidate.address,
            interface,
            epoch,
            token: identity.token,
            timestamp_hint: Some(candidate.timestamp_hint),
        })
    }

    pub fn did(&self) -> u64 {
        self.did
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn address(&self) -> SocketAddrV4 {
        self.address
    }

    pub fn interface(&self) -> &NetworkInterface {
        &self.interface
    }

    pub fn epoch(&self) -> NetworkEpoch {
        self.epoch
    }

    pub(crate) fn matches_catalog(&self, device: &CatalogDevice, selected_home: &str) -> bool {
        target_identity(device, selected_home).is_ok_and(|identity| {
            identity.did == self.did && identity.model == self.model && identity.token == self.token
        })
    }

    pub(super) fn token(&self) -> &[u8; 16] {
        &self.token
    }

    pub(super) fn timestamp_hint(&self) -> Option<u32> {
        self.timestamp_hint
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        did: u64,
        model: &str,
        address: SocketAddrV4,
        interface: NetworkInterface,
        epoch: NetworkEpoch,
        token: [u8; 16],
    ) -> Self {
        Self {
            did,
            model: model.into(),
            address,
            interface,
            epoch,
            token,
            timestamp_hint: None,
        }
    }
}

fn target_identity(
    device: &CatalogDevice,
    selected_home: &str,
) -> Result<TargetIdentity, LanError> {
    if selected_home.is_empty()
        || device.home_id != selected_home
        || device.features.is_empty()
        || !device.pid.is_some_and(|pid| LAN_PIDS.contains(&pid))
        || is_group_model(&device.model)
    {
        return Err(LanError::new(
            "select LAN target",
            LanErrorKind::InvalidInput,
        ));
    }
    let did = device
        .parent_did
        .parse::<u64>()
        .ok()
        .filter(|did| *did != 0)
        .ok_or_else(|| LanError::new("select LAN target", LanErrorKind::InvalidInput))?;
    let token = device
        .token
        .as_ref()
        .and_then(|token| <[u8; 16]>::try_from(token.0.as_slice()).ok())
        .ok_or_else(|| LanError::new("select LAN target", LanErrorKind::InvalidInput))?;
    Ok(TargetIdentity {
        did,
        model: device.model.clone(),
        token,
    })
}

fn is_group_model(model: &str) -> bool {
    model
        .split('.')
        .next_back()
        .is_some_and(|segment| segment.starts_with("group"))
}

fn validate_unicast_target(
    address: Ipv4Addr,
    interface: &NetworkInterface,
) -> Result<(), LanError> {
    let raw = u32::from(address);
    let mask = u32::from(interface.netmask());
    let network = u32::from(interface.address()) & mask;
    let broadcast = network | !mask;
    if !interface.on_link(address)
        || raw == network
        || raw == broadcast
        || address == interface.address()
        || address.is_unspecified()
        || address.is_loopback()
        || address.is_multicast()
        || address == Ipv4Addr::BROADCAST
    {
        return Err(LanError::new(
            "select LAN target",
            LanErrorKind::InvalidInput,
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
