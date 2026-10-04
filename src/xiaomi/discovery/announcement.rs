use std::{
    collections::BTreeMap,
    net::{SocketAddr, SocketAddrV6},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use mdns_sd::{ResolvedService, ScopedIp};

use super::model::{DiscoveryError, DiscoverySnapshot, Gateway};

pub const SERVICE_TYPE: &str = "_miot-central._tcp.local.";

#[derive(Default)]
pub(super) struct Registry {
    services: BTreeMap<String, Announcement>,
}

impl Registry {
    pub(super) fn remove(&mut self, name: &str) {
        self.services.remove(&name.to_ascii_lowercase());
    }

    pub(super) fn resolve(&mut self, service: ResolvedService) -> Result<(), DiscoveryError> {
        let name = service.fullname.to_ascii_lowercase();
        // Replacement is authoritative even when the new profile is unusable.
        self.services.remove(&name);
        if service.ty_domain != SERVICE_TYPE || service.port == 0 || !service.is_valid() {
            return Err(DiscoveryError::InvalidService);
        }
        let profile = parse_profile(
            service
                .get_property_val_str("profile")
                .ok_or(DiscoveryError::InvalidProfile)?,
        )?;
        let mut addresses = service
            .addresses
            .iter()
            .filter_map(|address| socket_address(address, service.port))
            .collect::<Vec<_>>();
        addresses.sort_unstable();
        addresses.dedup();
        if addresses.is_empty() {
            return Err(DiscoveryError::InvalidService);
        }
        self.services
            .insert(name, Announcement { profile, addresses });
        Ok(())
    }

    pub(super) fn snapshot(&self) -> DiscoverySnapshot {
        let mut gateways: BTreeMap<(&str, &str), Vec<SocketAddr>> = BTreeMap::new();
        for announcement in self
            .services
            .values()
            .filter(|announcement| announcement.profile.is_master())
        {
            gateways
                .entry((&announcement.profile.group_id, &announcement.profile.did))
                .or_default()
                .extend(&announcement.addresses);
        }
        DiscoverySnapshot {
            gateways: gateways
                .into_iter()
                .map(|((group_id, did), mut addresses)| {
                    addresses.sort_unstable();
                    addresses.dedup();
                    Gateway {
                        did: did.into(),
                        group_id: group_id.into(),
                        addresses,
                    }
                })
                .collect(),
            revision: 0,
        }
    }
}

struct Announcement {
    profile: Profile,
    addresses: Vec<SocketAddr>,
}

pub(super) struct Profile {
    pub(super) did: String,
    pub(super) group_id: String,
    role: u8,
    mqtt: bool,
}

impl Profile {
    pub(super) fn is_master(&self) -> bool {
        self.role == 1 && self.mqtt
    }
}

pub(super) fn parse_profile(value: &str) -> Result<Profile, DiscoveryError> {
    let bytes = STANDARD
        .decode(value)
        .map_err(|_| DiscoveryError::InvalidProfile)?;
    if bytes.len() < 23 {
        return Err(DiscoveryError::InvalidProfile);
    }
    Ok(Profile {
        did: u64::from_be_bytes(bytes[1..9].try_into().unwrap()).to_string(),
        group_id: bytes[9..17]
            .iter()
            .rev()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
        role: bytes[20] >> 4,
        mqtt: bytes[22] & 2 != 0,
    })
}

pub(super) fn socket_address(address: &ScopedIp, port: u16) -> Option<SocketAddr> {
    match address {
        ScopedIp::V4(address) => Some(SocketAddr::new((*address.addr()).into(), port)),
        ScopedIp::V6(address) => {
            let scope = address.scope_id().index;
            if address.addr().is_unicast_link_local() && scope == 0 {
                return None;
            }
            Some(SocketAddrV6::new(*address.addr(), port, 0, scope).into())
        }
        _ => None,
    }
}
