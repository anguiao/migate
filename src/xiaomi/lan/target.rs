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
mod tests {
    use super::*;
    use crate::{storage::DeviceToken, xiaomi::catalog::compile_spec};

    fn interface() -> NetworkInterface {
        NetworkInterface {
            index: 4,
            name: "en-test".into(),
            address: Ipv4Addr::new(192, 168, 8, 10),
            netmask: Ipv4Addr::new(255, 255, 255, 0),
            prefix_len: 24,
        }
    }

    #[test]
    fn target_requires_on_link_unicast_distinct_from_local_host() {
        let interface = interface();
        assert!(validate_unicast_target(Ipv4Addr::new(192, 168, 8, 20), &interface).is_ok());
        for invalid in [
            Ipv4Addr::new(192, 168, 8, 0),
            Ipv4Addr::new(192, 168, 8, 255),
            Ipv4Addr::new(192, 168, 8, 10),
            Ipv4Addr::new(192, 168, 9, 20),
            Ipv4Addr::UNSPECIFIED,
            Ipv4Addr::BROADCAST,
            Ipv4Addr::new(224, 0, 0, 1),
            Ipv4Addr::LOCALHOST,
        ] {
            assert!(
                validate_unicast_target(invalid, &interface).is_err(),
                "{invalid}"
            );
        }
    }

    #[test]
    fn catalog_light_group_is_never_a_credentialed_lan_target() {
        let compiled = compile_spec(
            "mijia.light.group3",
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/miot_specs/mijia.light.group3.json"
            )),
        )
        .unwrap();
        let device = CatalogDevice {
            home_id: "home".into(),
            room_id: None,
            parent_did: "42".into(),
            name: "group".into(),
            model: "mijia.light.group3".into(),
            spec_type: Some(compiled.type_urn),
            pid: Some(0),
            token: Some(DeviceToken(vec![1; 16])),
            online: Some(true),
            local_ip: Some("192.168.8.20".into()),
            parent_id: None,
            features: compiled.features,
        };
        assert!(
            LanTarget::from_catalog(&device, "home", interface(), NetworkEpoch::new(1)).is_err()
        );
    }

    #[test]
    fn authenticated_target_can_start_from_matching_hello_when_cached_ip_is_missing() {
        let compiled = compile_spec(
            "mijia.light.group3",
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/miot_specs/mijia.light.group3.json"
            )),
        )
        .unwrap();
        let device = CatalogDevice {
            home_id: "home".into(),
            room_id: None,
            parent_did: "42".into(),
            name: "lamp".into(),
            model: "yeelink.light.color2".into(),
            spec_type: Some(compiled.type_urn),
            pid: Some(0),
            token: Some(DeviceToken(vec![1; 16])),
            online: None,
            local_ip: None,
            parent_id: None,
            features: compiled.features,
        };
        let candidate = LanHelloCandidate {
            did: 42,
            address: SocketAddrV4::new(Ipv4Addr::new(192, 168, 8, 20), 54321),
            interface_index: 4,
            epoch: NetworkEpoch::new(2),
            timestamp_hint: 901,
            native_hint: true,
            subscription_hint: None,
        };
        let target = LanTarget::from_candidate(
            &device,
            "home",
            interface(),
            NetworkEpoch::new(2),
            &candidate,
        )
        .unwrap();
        assert_eq!(target.address(), candidate.address);
        assert_eq!(target.timestamp_hint(), Some(901));
    }
}
