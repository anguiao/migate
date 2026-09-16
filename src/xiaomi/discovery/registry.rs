use std::{collections::BTreeMap, net::Ipv4Addr};

use super::{DiscoveryError, GatewayProfile, MIOT_SERVICE_TYPE, NetworkEpoch, NetworkSnapshot};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScopedIpv4 {
    pub address: Ipv4Addr,
    pub interface_indexes: Vec<u32>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawResolvedService {
    pub service_type: String,
    pub instance: String,
    pub port: u16,
    pub profile: String,
    pub addresses: Vec<ScopedIpv4>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MdnsEvent {
    Resolved(RawResolvedService),
    Removed {
        service_type: String,
        instance: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct GatewayEndpoint {
    pub interface_index: u32,
    pub source_address: Ipv4Addr,
    pub address: Ipv4Addr,
    pub port: u16,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GatewayCandidate {
    pub gateway_did: u64,
    pub home_group: String,
    pub endpoints: Vec<GatewayEndpoint>,
    pub unverified: bool,
}

struct Observation {
    profile: GatewayProfile,
    endpoints: Vec<GatewayEndpoint>,
    generation: u64,
}

pub struct DiscoveryRegistry {
    epoch: NetworkEpoch,
    network: NetworkSnapshot,
    observations: BTreeMap<String, Observation>,
    latest_profiles: BTreeMap<u64, (GatewayProfile, u64)>,
    next_generation: u64,
}

impl DiscoveryRegistry {
    pub fn new(epoch: NetworkEpoch, network: NetworkSnapshot) -> Self {
        Self {
            epoch,
            network,
            observations: BTreeMap::new(),
            latest_profiles: BTreeMap::new(),
            next_generation: 1,
        }
    }

    pub fn apply(&mut self, event: MdnsEvent) -> Result<(), DiscoveryError> {
        match event {
            MdnsEvent::Resolved(service) => self.resolve(service),
            MdnsEvent::Removed {
                service_type,
                instance,
            } => {
                if service_type != MIOT_SERVICE_TYPE {
                    return Err(DiscoveryError::new("unexpected mDNS service type"));
                }
                if let Some(removed) = self.observations.remove(&instance) {
                    let gateway_did = removed.profile.gateway_did;
                    let removed_was_latest = self
                        .latest_profiles
                        .get(&gateway_did)
                        .is_some_and(|(_, generation)| *generation == removed.generation);
                    if removed_was_latest && removed.profile.master {
                        if let Some(latest) = self
                            .observations
                            .values()
                            .filter(|observation| observation.profile.gateway_did == gateway_did)
                            .max_by_key(|observation| observation.generation)
                        {
                            self.latest_profiles
                                .insert(gateway_did, (latest.profile.clone(), latest.generation));
                        } else {
                            self.latest_profiles.remove(&gateway_did);
                        }
                    }
                }
                Ok(())
            }
        }
    }

    pub fn candidates(&self) -> Vec<GatewayCandidate> {
        self.latest_profiles
            .values()
            .filter(|(profile, _)| profile.master && profile.mqtt)
            .map(|(latest, _)| {
                let gateway_did = latest.gateway_did;
                let mut endpoints = self
                    .observations
                    .values()
                    .filter(|observation| observation.profile.gateway_did == gateway_did)
                    .flat_map(|observation| observation.endpoints.clone())
                    .collect::<Vec<_>>();
                endpoints.sort();
                endpoints.dedup();
                GatewayCandidate {
                    gateway_did,
                    home_group: latest.home_group.clone(),
                    endpoints,
                    unverified: true,
                }
            })
            .collect()
    }

    pub fn update_network(&mut self, epoch: NetworkEpoch, network: NetworkSnapshot) {
        if self.epoch != epoch || self.network != network {
            self.observations.clear();
            self.latest_profiles.clear();
        }
        self.epoch = epoch;
        self.network = network;
    }

    fn resolve(&mut self, service: RawResolvedService) -> Result<(), DiscoveryError> {
        if service.service_type != MIOT_SERVICE_TYPE {
            return Err(DiscoveryError::new("unexpected mDNS service type"));
        }
        if service.instance.is_empty() || service.port == 0 {
            return Err(DiscoveryError::new("invalid mDNS service endpoint"));
        }
        let profile = GatewayProfile::parse_base64(&service.profile)?;
        if self.observations.values().any(|observation| {
            observation.profile.gateway_did == profile.gateway_did
                && observation.profile.home_group != profile.home_group
        }) {
            return Err(DiscoveryError::new(
                "gateway advertised conflicting home groups",
            ));
        }
        let mut endpoints = Vec::new();
        for scoped_address in service.addresses {
            for interface_index in scoped_address.interface_indexes {
                for interface in self.network.interfaces_with_index(interface_index) {
                    if !interface.on_link(scoped_address.address) {
                        continue;
                    }
                    endpoints.push(GatewayEndpoint {
                        interface_index,
                        source_address: interface.address,
                        address: scoped_address.address,
                        port: service.port,
                    });
                }
            }
        }
        endpoints.sort();
        endpoints.dedup();
        if endpoints.is_empty() {
            return Err(DiscoveryError::new(
                "mDNS service has no on-link physical endpoint",
            ));
        }
        let generation = self.next_generation;
        self.next_generation = self.next_generation.saturating_add(1);
        self.latest_profiles
            .insert(profile.gateway_did, (profile.clone(), generation));
        self.observations.insert(
            service.instance,
            Observation {
                profile,
                endpoints,
                generation,
            },
        );
        Ok(())
    }
}
