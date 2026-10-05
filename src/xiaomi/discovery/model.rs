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
        self.routes(group_id, current).into_iter().next()
    }

    /// Try the current route first, then its other addresses and other masters.
    pub fn routes(&self, group_id: &str, current: Option<&GatewayRoute>) -> Vec<GatewayRoute> {
        let current = current.filter(|route| route.group_id == group_id);
        let priority = |route: &GatewayRoute| {
            if current == Some(route) {
                0
            } else if current.is_some_and(|current| current.did == route.did) {
                1
            } else {
                2
            }
        };
        let mut routes = self
            .gateways
            .iter()
            .filter(|gateway| gateway.group_id == group_id)
            .flat_map(|gateway| {
                gateway.addresses.iter().map(|address| GatewayRoute {
                    did: gateway.did.clone(),
                    group_id: gateway.group_id.clone(),
                    address: *address,
                })
            })
            .collect::<Vec<_>>();
        routes.sort_by(|left, right| {
            priority(left)
                .cmp(&priority(right))
                .then_with(|| left.did.cmp(&right.did))
                .then_with(|| left.address.cmp(&right.address))
        });
        routes
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
