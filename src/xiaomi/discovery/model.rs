use std::{error::Error, fmt, net::SocketAddr};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Gateway {
    pub did: String,
    pub group_id: String,
    pub addresses: Vec<SocketAddr>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GatewayRoute {
    pub did: String,
    pub group_id: String,
    pub address: SocketAddr,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DiscoverySnapshot {
    pub gateways: Vec<Gateway>,
    pub revision: u64,
}

impl DiscoverySnapshot {
    /// Retain an eligible current master and address. Backup announcements do
    /// not interrupt an authenticated route; replacement selection is stable.
    pub fn route(&self, group_id: &str, current: Option<&GatewayRoute>) -> Option<GatewayRoute> {
        let current = current.filter(|route| route.group_id == group_id);
        let current_gateway = current.and_then(|route| {
            self.gateways
                .iter()
                .find(|gateway| gateway.group_id == group_id && gateway.did == route.did)
        });
        let (gateway, address) = if let Some((gateway, route)) = current_gateway.zip(current) {
            let address = gateway
                .addresses
                .iter()
                .find(|address| **address == route.address)
                .or_else(|| gateway.addresses.iter().min())?;
            (gateway, *address)
        } else {
            let gateway = self
                .gateways
                .iter()
                .filter(|gateway| gateway.group_id == group_id && !gateway.addresses.is_empty())
                .min_by(|left, right| left.did.cmp(&right.did))?;
            (gateway, *gateway.addresses.iter().min()?)
        };
        Some(GatewayRoute {
            did: gateway.did.clone(),
            group_id: gateway.group_id.clone(),
            address,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DiscoveryError {
    InvalidProfile,
    InvalidService,
    Backend(String),
    Stopped,
}

impl fmt::Display for DiscoveryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidProfile => formatter.write_str("invalid local gateway profile"),
            Self::InvalidService => formatter.write_str("invalid local gateway service"),
            Self::Backend(error) => write!(formatter, "local gateway discovery failed: {error}"),
            Self::Stopped => formatter.write_str("local gateway discovery stopped"),
        }
    }
}

impl Error for DiscoveryError {}
