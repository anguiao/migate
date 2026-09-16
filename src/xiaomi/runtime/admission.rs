use std::collections::{BTreeMap, HashMap, HashSet};

use crate::{
    device::{AccountId, DeviceDid, DeviceService, FeatureIdentity, HomeId, PhysicalDeviceId},
    storage::{
        AuthSessionGeneration, AuthSnapshot, DeviceRecord, DeviceStore, HomeBinding,
        PublishedFeatureDefinition, PublishedTopologyDelta, StorageError,
    },
    xiaomi::{
        catalog::{CatalogDevice, DeviceCatalog, FeatureDescriptor, compile_spec},
        discovery::{GatewayCandidate, NetworkEpoch, NetworkSnapshot},
        gateway::GatewayEvidence,
        lan::{LanEvidence, LanTarget},
    },
};

use super::{CommandRuntime, OperationPaths, RuntimeFeature};

#[derive(Clone, Debug)]
pub struct AdmissionCatalog {
    pub account: AccountId,
    pub session_generation: AuthSessionGeneration,
    pub catalog: DeviceCatalog,
    pub specifications: HashMap<String, String>,
}

#[derive(Clone, Debug)]
pub struct AuthenticatedGateway {
    pub account: AccountId,
    pub session_generation: AuthSessionGeneration,
    pub candidate: GatewayCandidate,
    pub selected_endpoint: crate::xiaomi::discovery::GatewayEndpoint,
    pub network: NetworkSnapshot,
    pub evidence: GatewayEvidence,
}

#[derive(Clone, Debug)]
pub struct AuthenticatedLan {
    pub account: AccountId,
    pub session_generation: AuthSessionGeneration,
    pub target: LanTarget,
    pub network: NetworkSnapshot,
    pub evidence: LanEvidence,
    pub legacy_operation: LegacyOperationEvidence,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LegacyOperationEvidence {
    Unverified,
    SuccessfulRead,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CloudEvidence {
    pub account: AccountId,
    pub session_generation: AuthSessionGeneration,
    pub status: CloudStatus,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CloudStatus {
    Ready,
    TransportUnavailable,
    InvalidToken,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdmissionStatus {
    Unbound,
    Active,
    SuspendedConflict,
}
#[derive(Clone, Debug)]
pub struct AdmissionFeature {
    pub identity: FeatureIdentity,
    pub runtime: RuntimeFeature,
    pub paths: OperationPaths,
    pub gateways: Vec<GatewayPathEvidence>,
    pub lan_evidence: Option<LanEvidence>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GatewayPathEvidence {
    pub gateway_did: u64,
    pub access: bool,
    pub push: bool,
    pub online: Option<bool>,
}

#[derive(Clone, Debug)]
pub struct AdmissionSnapshot {
    pub binding: Option<HomeBinding>,
    pub status: AdmissionStatus,
    pub epoch: NetworkEpoch,
    pub features: Vec<AdmissionFeature>,
}

pub struct AdmissionController {
    store: DeviceStore,
    service: DeviceService,
    commands: CommandRuntime,
    epoch: NetworkEpoch,
    authority_generation: u64,
    session_generation: AuthSessionGeneration,
    gateways: BTreeMap<u64, GatewayAuthority>,
    lan: BTreeMap<PhysicalDeviceId, LanAuthority>,
    memberships: BTreeMap<PhysicalDeviceId, DeviceProof>,
    catalog_online: BTreeMap<PhysicalDeviceId, Option<bool>>,
    runtime_features: BTreeMap<FeatureIdentity, RuntimeFeature>,
    cloud_ready: bool,
    status: AdmissionStatus,
}

#[derive(Clone, Copy)]
struct DeviceProof {
    access: bool,
    notify: bool,
    online: Option<bool>,
}
#[derive(Clone)]
struct GatewayAuthority {
    home_id: String,
    device_paths: HashMap<String, DeviceProof>,
}
#[derive(Clone)]
struct LanAuthority {
    target: LanTarget,
    evidence: LanEvidence,
    operation_supported: bool,
}

impl AdmissionController {
    pub fn new(
        store: DeviceStore,
        service: DeviceService,
        commands: CommandRuntime,
        epoch: NetworkEpoch,
        session_generation: AuthSessionGeneration,
    ) -> Self {
        Self {
            store,
            service,
            commands,
            epoch,
            authority_generation: 1,
            session_generation,
            gateways: BTreeMap::new(),
            lan: BTreeMap::new(),
            memberships: BTreeMap::new(),
            catalog_online: BTreeMap::new(),
            runtime_features: BTreeMap::new(),
            cloud_ready: false,
            status: AdmissionStatus::Unbound,
        }
    }

    pub fn status(&self) -> AdmissionStatus {
        self.status
    }

    pub fn observe_auth(&mut self, snapshot: &AuthSnapshot) -> Result<(), StorageError> {
        if snapshot.session_generation == self.session_generation {
            return Ok(());
        }
        let new_uid = snapshot.record.as_ref().map(|record| record.uid.as_str());
        self.suspend(AdmissionStatus::Unbound);
        self.gateways.clear();
        self.lan.clear();
        self.memberships.clear();
        self.catalog_online.clear();
        self.cloud_ready = false;
        let old_binding = self.store.load_binding()?;
        self.session_generation = snapshot.session_generation;
        if let (Some(binding), Some(new_uid)) = (old_binding.as_ref(), new_uid)
            && binding.account.as_str() != new_uid
        {
            for feature in self.service.features() {
                self.commands.unregister(&feature.identity);
                self.service.remove(&feature.identity);
            }
            self.runtime_features.clear();
        }
        Ok(())
    }

    pub fn snapshot(&self) -> Result<AdmissionSnapshot, StorageError> {
        let binding = self.store.load_binding()?;
        let features = self
            .runtime_features
            .values()
            .map(|feature| {
                let proof = (self.status == AdmissionStatus::Active)
                    .then(|| {
                        aggregate_proof_for_home(
                            &self.gateways,
                            feature.identity.physical.home.as_str(),
                            feature.identity.physical.parent_did.as_str(),
                        )
                    })
                    .flatten();
                let paths = if self.status == AdmissionStatus::Active {
                    OperationPaths {
                        gateway: proof.is_some_and(|proof| proof.access),
                        lan: self
                            .lan
                            .get(&feature.identity.physical)
                            .is_some_and(|authority| authority.operation_supported),
                        cloud: self.cloud_ready
                            && self.memberships.contains_key(&feature.identity.physical)
                            && self
                                .catalog_online
                                .get(&feature.identity.physical)
                                .is_some_and(|online| *online != Some(false)),
                    }
                } else {
                    OperationPaths::default()
                };
                let gateways = self
                    .gateways
                    .iter()
                    .filter_map(|(gateway_did, gateway)| {
                        (gateway.home_id == feature.identity.physical.home.as_str())
                            .then(|| {
                                gateway
                                    .device_paths
                                    .get(feature.identity.physical.parent_did.as_str())
                            })
                            .flatten()
                            .map(|proof| GatewayPathEvidence {
                                gateway_did: *gateway_did,
                                access: self.status == AdmissionStatus::Active
                                    && paths.gateway
                                    && proof.access
                                    && proof.online != Some(false),
                                push: self.status == AdmissionStatus::Active
                                    && proof.notify
                                    && proof.online != Some(false),
                                online: proof.online,
                            })
                    })
                    .collect::<Vec<_>>();
                AdmissionFeature {
                    identity: feature.identity.clone(),
                    paths,
                    gateways,
                    lan_evidence: (self.status == AdmissionStatus::Active)
                        .then(|| self.lan.get(&feature.identity.physical))
                        .flatten()
                        .map(|authority| authority.evidence.clone()),
                    runtime: feature.clone(),
                }
            })
            .collect();
        Ok(AdmissionSnapshot {
            binding,
            status: self.status,
            epoch: self.epoch,
            features,
        })
    }

    pub fn invalidate(&mut self, epoch: NetworkEpoch) {
        self.epoch = epoch;
        self.gateways.clear();
        self.lan.clear();
        self.status = if self.runtime_features.is_empty() {
            AdmissionStatus::Unbound
        } else {
            AdmissionStatus::Active
        };
    }

    pub fn restore_archived(&self) -> Result<(), StorageError> {
        for definition in self.store.load_active_published_feature_definitions()? {
            let compiled =
                compile_spec(&definition.model, &definition.spec_document).map_err(|error| {
                    StorageError::new(self.store.path(), "compile published Xiaomi feature", error)
                })?;
            let descriptor = compiled
                .features
                .into_iter()
                .find(|feature| {
                    feature.service_instance == definition.feature.service_instance
                        && feature.role == definition.feature.role
                })
                .ok_or_else(|| {
                    StorageError::new(
                        self.store.path(),
                        "restore published Xiaomi feature",
                        "Published spec no longer contains the feature",
                    )
                })?;
            self.service
                .restore(definition.feature, definition.name, descriptor.capabilities);
        }
        for state in self.store.load_states()? {
            self.service
                .restore_last_known(crate::device::StateReport::new(
                    state.feature,
                    state.report_version,
                    state.source,
                    state.observed_at,
                    [(state.property, state.value)],
                ));
        }
        Ok(())
    }

    pub fn observe_gateway(
        &mut self,
        gateway: AuthenticatedGateway,
        catalog: &AdmissionCatalog,
    ) -> Result<AdmissionStatus, StorageError> {
        if !valid_gateway_provenance(
            &gateway,
            self.epoch,
            &catalog.account,
            catalog.session_generation,
        ) || catalog.account.as_str() != catalog.catalog.uid
            || catalog.session_generation != self.session_generation
        {
            return Ok(self.status);
        }
        let Some(home) = catalog
            .catalog
            .homes
            .iter()
            .find(|home| home.group_id == gateway.candidate.home_group)
        else {
            return Ok(self.status);
        };
        self.gateways.insert(
            gateway.evidence.gateway_did,
            GatewayAuthority {
                home_id: home.id.clone(),
                device_paths: gateway
                    .evidence
                    .devices
                    .iter()
                    .map(|device| {
                        (
                            device.did.clone(),
                            DeviceProof {
                                access: device.spec_v2_access == Some(true),
                                notify: device.push_available == Some(true),
                                online: device.online,
                            },
                        )
                    })
                    .collect(),
            },
        );
        let homes = self
            .gateways
            .values()
            .map(|authority| authority.home_id.as_str())
            .collect::<HashSet<_>>();
        if homes.len() != 1 {
            self.suspend(AdmissionStatus::SuspendedConflict);
            return Ok(self.status);
        }
        if let Some(binding) = self.store.load_binding()?
            && binding.account == catalog.account
            && binding.home.as_str() != home.id
        {
            self.suspend(AdmissionStatus::SuspendedConflict);
            return Ok(self.status);
        }
        let binding = HomeBinding {
            account: catalog.account.clone(),
            home: HomeId::new(home.id.clone())
                .map_err(|error| StorageError::new(self.store.path(), "validate home", error))?,
            display_name: home.name.clone(),
        };
        let committed = self.reconcile_catalog(catalog, &binding, true)?;
        if !committed {
            return Ok(self.status);
        }
        self.status = AdmissionStatus::Active;
        Ok(self.status)
    }

    pub fn apply_complete_catalog(
        &mut self,
        catalog: &AdmissionCatalog,
    ) -> Result<AdmissionStatus, StorageError> {
        if catalog.session_generation != self.session_generation
            || catalog.account.as_str() != catalog.catalog.uid
        {
            return Ok(self.status);
        }
        let Some(binding) = self.store.load_binding()? else {
            return Ok(self.status);
        };
        if binding.account != catalog.account {
            return Ok(self.status);
        }
        self.reconcile_catalog(
            catalog,
            &binding,
            self.status != AdmissionStatus::SuspendedConflict,
        )?;
        Ok(self.status)
    }

    fn reconcile_catalog(
        &mut self,
        catalog: &AdmissionCatalog,
        binding: &HomeBinding,
        publish_authority: bool,
    ) -> Result<bool, StorageError> {
        self.lan.retain(|physical, authority| {
            catalog
                .catalog
                .devices
                .iter()
                .find(|device| {
                    device.home_id == binding.home.as_str()
                        && device.parent_did == physical.parent_did.as_str()
                })
                .is_some_and(|device| {
                    authority
                        .target
                        .matches_catalog(device, binding.home.as_str())
                })
        });
        let active_features = self.store.load_features(true)?;
        let historically_admitted = active_features
            .iter()
            .map(|feature| feature.feature.physical.clone())
            .collect::<HashSet<_>>();
        let mut definitions = Vec::new();
        let mut devices = Vec::new();
        for device in catalog
            .catalog
            .devices
            .iter()
            .filter(|device| device.home_id == binding.home.as_str())
        {
            let physical = physical_identity(
                &binding.account,
                &binding.home,
                &device.parent_did,
                self.store.path(),
            )?;
            if publish_authority
                && (self.gateway_allows(device)
                    || self.memberships.contains_key(&physical)
                    || historically_admitted.contains(&physical))
            {
                if historically_admitted.contains(&physical) {
                    self.memberships
                        .entry(physical.clone())
                        .or_insert(DeviceProof {
                            access: false,
                            notify: false,
                            online: None,
                        });
                }
                self.catalog_online.insert(physical.clone(), device.online);
                if let Some(proof) = aggregate_proof(&self.gateways, &device.parent_did) {
                    self.memberships.insert(physical.clone(), proof);
                }
                self.prepare_device(
                    &binding.account,
                    &binding.home,
                    device,
                    catalog,
                    &mut devices,
                    &mut definitions,
                )?;
            }
        }
        let mut prepared = Vec::with_capacity(definitions.len());
        for definition in &definitions {
            let descriptor = compile_published_descriptor(&self.store, definition)?;
            let generation = match self.runtime_features.get(&definition.feature) {
                Some(runtime) if runtime.descriptor == descriptor => runtime.authority_generation,
                previous => {
                    self.authority_generation = self.authority_generation.saturating_add(1);
                    if previous.is_some() {
                        self.commands.unregister(&definition.feature);
                        self.service.revoke(&definition.feature);
                        self.runtime_features.remove(&definition.feature);
                    }
                    self.authority_generation
                }
            };
            prepared.push((descriptor, generation));
        }
        let mut deactivate = Vec::new();
        for existing in self.service.features() {
            if existing.identity.physical.account != binding.account
                || existing.identity.physical.home != binding.home
            {
                continue;
            }
            let authoritative_removal =
                is_authoritatively_removed(&existing.identity, catalog, binding);
            if authoritative_removal {
                self.commands.unregister(&existing.identity);
                self.service.remove(&existing.identity);
                self.runtime_features.remove(&existing.identity);
                deactivate.push(existing.identity.clone());
                self.remove_device_proof_if_absent(&existing.identity.physical, catalog, binding);
            } else if !definitions
                .iter()
                .any(|definition| definition.feature == existing.identity)
            {
                self.commands.unregister(&existing.identity);
                self.service.revoke(&existing.identity);
                self.runtime_features.remove(&existing.identity);
            }
        }
        for persisted in self.store.load_features(true)? {
            if persisted.feature.physical.account == binding.account
                && persisted.feature.physical.home == binding.home
                && !deactivate.contains(&persisted.feature)
                && is_authoritatively_removed(&persisted.feature, catalog, binding)
            {
                self.remove_device_proof_if_absent(&persisted.feature.physical, catalog, binding);
                deactivate.push(persisted.feature);
            }
        }
        let present_devices = catalog
            .catalog
            .devices
            .iter()
            .filter(|device| device.home_id == binding.home.as_str())
            .map(|device| device.parent_did.as_str())
            .collect::<HashSet<_>>();
        self.memberships.retain(|physical, _| {
            physical.home != binding.home || present_devices.contains(physical.parent_did.as_str())
        });
        self.catalog_online.retain(|physical, _| {
            physical.home != binding.home || present_devices.contains(physical.parent_did.as_str())
        });
        let published = self.store.publish_topology(&PublishedTopologyDelta {
            account: binding.account.clone(),
            session_generation: self.session_generation,
            binding: Some(binding.clone()),
            devices,
            definitions: definitions.clone(),
            deactivate,
        })?;
        if published.is_none() {
            self.suspend(AdmissionStatus::Unbound);
            self.gateways.clear();
            self.lan.clear();
            self.memberships.clear();
            self.catalog_online.clear();
            return Ok(false);
        }
        for (definition, (descriptor, generation)) in definitions.into_iter().zip(prepared) {
            self.publish_definition(definition, descriptor, generation);
        }
        if !self.runtime_features.is_empty() && publish_authority {
            self.status = AdmissionStatus::Active;
        }
        Ok(true)
    }

    pub fn confirm_lan(
        &mut self,
        proof: &AuthenticatedLan,
        catalog: &AdmissionCatalog,
    ) -> Result<bool, StorageError> {
        if self.status == AdmissionStatus::SuspendedConflict
            || proof.account != catalog.account
            || proof.account.as_str() != catalog.catalog.uid
            || proof.session_generation != self.session_generation
            || catalog.session_generation != self.session_generation
            || proof.evidence.epoch != self.epoch
            || proof.target.epoch() != self.epoch
            || proof.evidence.did != proof.target.did()
            || proof.evidence.address != proof.target.address()
            || proof.evidence.interface_index != proof.target.interface().index()
            || !proof
                .network
                .interfaces_with_index(proof.evidence.interface_index)
                .any(|interface| {
                    interface == proof.target.interface()
                        && interface.on_link(*proof.evidence.address.ip())
                })
        {
            return Ok(false);
        }
        let Some(binding) = self.store.load_binding()? else {
            return Ok(false);
        };
        let Some(device) = catalog.catalog.devices.iter().find(|device| {
            device.home_id == binding.home.as_str()
                && device.parent_did == proof.evidence.did.to_string()
        }) else {
            return Ok(false);
        };
        if binding.account != proof.account
            || !proof.target.matches_catalog(device, binding.home.as_str())
        {
            return Ok(false);
        }
        let physical = physical_identity(
            &binding.account,
            &binding.home,
            &device.parent_did,
            self.store.path(),
        )?;
        let previous_membership = self.memberships.insert(
            physical.clone(),
            DeviceProof {
                access: false,
                notify: false,
                online: None,
            },
        );
        let previous_catalog_online = self.catalog_online.insert(physical.clone(), device.online);
        let committed = match self.reconcile_catalog(catalog, &binding, true) {
            Ok(committed) => committed,
            Err(error) => {
                match previous_membership {
                    Some(membership) => {
                        self.memberships.insert(physical.clone(), membership);
                    }
                    None => {
                        self.memberships.remove(&physical);
                    }
                }
                match previous_catalog_online {
                    Some(online) => {
                        self.catalog_online.insert(physical.clone(), online);
                    }
                    None => {
                        self.catalog_online.remove(&physical);
                    }
                }
                return Err(error);
            }
        };
        if !committed {
            return Ok(false);
        }
        let operation_supported = proof.evidence.native_supported
            || (proof.legacy_operation == LegacyOperationEvidence::SuccessfulRead
                && device.model == "lumi.acpartner.mcn02");
        self.lan.insert(
            physical.clone(),
            LanAuthority {
                target: proof.target.clone(),
                evidence: proof.evidence.clone(),
                operation_supported,
            },
        );
        self.status = AdmissionStatus::Active;
        Ok(true)
    }

    pub fn remove_lan(&mut self, device: &PhysicalDeviceId) {
        self.lan.remove(device);
    }

    pub fn remove_gateway(&mut self, gateway_did: u64) -> Result<(), StorageError> {
        self.gateways.remove(&gateway_did);
        let homes = self
            .gateways
            .values()
            .map(|gateway| gateway.home_id.as_str())
            .collect::<HashSet<_>>();
        if homes.len() > 1 {
            self.suspend(AdmissionStatus::SuspendedConflict);
            return Ok(());
        }
        if let (Some(binding), Some(home)) = (self.store.load_binding()?, homes.iter().next())
            && binding.home.as_str() != *home
        {
            self.suspend(AdmissionStatus::SuspendedConflict);
            return Ok(());
        }
        self.status = if self.runtime_features.is_empty() {
            AdmissionStatus::Unbound
        } else {
            AdmissionStatus::Active
        };
        for feature in self.runtime_features.values_mut() {
            if self.status == AdmissionStatus::Active {
                self.service.admit(&feature.identity);
            }
        }
        Ok(())
    }

    pub fn observe_cloud(&mut self, evidence: &CloudEvidence) -> Result<bool, StorageError> {
        if self.status == AdmissionStatus::SuspendedConflict
            || evidence.session_generation != self.session_generation
        {
            return Ok(false);
        }
        let binding = match self.store.load_binding() {
            Ok(Some(binding)) => binding,
            Ok(None) => return Ok(false),
            Err(error) => {
                self.cloud_ready = false;
                return Err(error);
            }
        };
        if binding.account != evidence.account {
            return Ok(false);
        }
        self.cloud_ready = evidence.status == CloudStatus::Ready;
        Ok(true)
    }

    pub fn observe_cloud_online(&mut self, physical: &PhysicalDeviceId, online: bool) -> bool {
        if self.status != AdmissionStatus::Active || !self.memberships.contains_key(physical) {
            return false;
        }
        self.catalog_online.insert(physical.clone(), Some(online));
        true
    }

    fn gateway_allows(&self, device: &CatalogDevice) -> bool {
        let proof = self
            .gateways
            .iter()
            .filter(|(_, gateway)| gateway.home_id == device.home_id)
            .filter_map(|(_, gateway)| gateway.device_paths.get(&device.parent_did))
            .copied()
            .reduce(|left, right| DeviceProof {
                access: left.access || right.access,
                notify: left.notify || right.notify,
                online: None,
            });
        proof.is_some_and(|proof| {
            if is_group_model(&device.model) {
                proof.access
            } else {
                proof.access || proof.notify
            }
        })
    }

    fn prepare_device(
        &self,
        account: &AccountId,
        home: &HomeId,
        device: &CatalogDevice,
        catalog: &AdmissionCatalog,
        devices: &mut Vec<DeviceRecord>,
        definitions: &mut Vec<PublishedFeatureDefinition>,
    ) -> Result<(), StorageError> {
        let physical = PhysicalDeviceId {
            account: account.clone(),
            home: home.clone(),
            parent_did: DeviceDid::new(device.parent_did.clone()).map_err(|error| {
                StorageError::new(self.store.path(), "validate device DID", error)
            })?,
        };
        devices.push(DeviceRecord {
            identity: physical.clone(),
            model: device.model.clone(),
            name: device.name.clone(),
            room_id: device.room_id.clone(),
            admitted: true,
        });
        let Some(spec_type) = device.spec_type.as_ref() else {
            return Ok(());
        };
        let Some(document) = catalog.specifications.get(spec_type) else {
            return Ok(());
        };
        for descriptor in &device.features {
            let identity = FeatureIdentity {
                physical: physical.clone(),
                service_instance: descriptor.service_instance,
                role: descriptor.role,
            };
            definitions.push(PublishedFeatureDefinition {
                feature: identity,
                model: device.model.clone(),
                spec_document: document.clone(),
                name: if device.features.len() == 1 {
                    device.name.clone()
                } else if descriptor.name == device.name {
                    descriptor.name.clone()
                } else {
                    format!("{} {}", device.name, descriptor.name)
                },
            });
        }
        Ok(())
    }

    fn remove_device_proof_if_absent(
        &mut self,
        physical: &PhysicalDeviceId,
        catalog: &AdmissionCatalog,
        binding: &HomeBinding,
    ) {
        let present = catalog.catalog.devices.iter().any(|device| {
            device.home_id == binding.home.as_str()
                && device.parent_did == physical.parent_did.as_str()
        });
        if present {
            return;
        }
        self.memberships.remove(physical);
        self.catalog_online.remove(physical);
        self.lan.remove(physical);
        for gateway in self.gateways.values_mut() {
            gateway.device_paths.remove(physical.parent_did.as_str());
        }
    }

    fn publish_definition(
        &mut self,
        definition: PublishedFeatureDefinition,
        descriptor: FeatureDescriptor,
        authority_generation: u64,
    ) {
        self.service.publish(
            definition.feature.clone(),
            definition.name,
            descriptor.capabilities.clone(),
        );
        let runtime = RuntimeFeature {
            identity: definition.feature.clone(),
            descriptor,
            authority_generation,
            auth_session_generation: self.session_generation,
        };
        self.commands.register(runtime.clone());
        self.runtime_features
            .insert(runtime.identity.clone(), runtime);
    }

    fn suspend(&mut self, status: AdmissionStatus) {
        self.authority_generation = self.authority_generation.saturating_add(1);
        self.commands.cancel_all();
        for feature in self.runtime_features.values_mut() {
            feature.authority_generation = self.authority_generation;
            self.commands.register(feature.clone());
        }
        self.service.logout();
        self.status = status;
    }
}

fn compile_published_descriptor(
    store: &DeviceStore,
    definition: &PublishedFeatureDefinition,
) -> Result<FeatureDescriptor, StorageError> {
    compile_spec(&definition.model, &definition.spec_document)
        .map_err(|error| StorageError::new(store.path(), "compile published feature", error))?
        .features
        .into_iter()
        .find(|descriptor| {
            descriptor.service_instance == definition.feature.service_instance
                && descriptor.role == definition.feature.role
        })
        .ok_or_else(|| {
            StorageError::new(
                store.path(),
                "compile published feature",
                "Published spec does not contain the selected feature",
            )
        })
}

fn is_authoritatively_removed(
    identity: &FeatureIdentity,
    catalog: &AdmissionCatalog,
    binding: &HomeBinding,
) -> bool {
    match catalog.catalog.devices.iter().find(|device| {
        device.home_id == binding.home.as_str()
            && device.parent_did == identity.physical.parent_did.as_str()
    }) {
        None => true,
        Some(device) => {
            device
                .spec_type
                .as_ref()
                .is_some_and(|spec_type| catalog.specifications.contains_key(spec_type))
                && !device.features.iter().any(|descriptor| {
                    descriptor.service_instance == identity.service_instance
                        && descriptor.role == identity.role
                })
        }
    }
}

fn physical_identity(
    account: &AccountId,
    home: &HomeId,
    did: &str,
    path: &std::path::Path,
) -> Result<PhysicalDeviceId, StorageError> {
    Ok(PhysicalDeviceId {
        account: account.clone(),
        home: home.clone(),
        parent_did: DeviceDid::new(did.to_owned())
            .map_err(|error| StorageError::new(path, "validate device DID", error))?,
    })
}

fn is_group_model(model: &str) -> bool {
    model
        .split('.')
        .next_back()
        .is_some_and(|segment| segment.starts_with("group"))
}

fn valid_gateway_provenance(
    gateway: &AuthenticatedGateway,
    epoch: NetworkEpoch,
    account: &AccountId,
    session_generation: AuthSessionGeneration,
) -> bool {
    gateway.account == *account
        && gateway.session_generation == session_generation
        && gateway.evidence.epoch == epoch
        && gateway.evidence.gateway_did == gateway.candidate.gateway_did
        && !gateway.evidence.peer_did.is_empty()
        && gateway.selected_endpoint.port != 0
        && !gateway.selected_endpoint.address.is_unspecified()
        && !gateway.selected_endpoint.address.is_loopback()
        && !gateway.selected_endpoint.address.is_multicast()
        && gateway.selected_endpoint.address != std::net::Ipv4Addr::BROADCAST
        && gateway
            .candidate
            .endpoints
            .contains(&gateway.selected_endpoint)
        && gateway
            .network
            .interfaces_with_index(gateway.selected_endpoint.interface_index)
            .any(|interface| {
                interface.address() == gateway.selected_endpoint.source_address
                    && interface.on_link(gateway.selected_endpoint.address)
            })
}

fn aggregate_proof(gateways: &BTreeMap<u64, GatewayAuthority>, did: &str) -> Option<DeviceProof> {
    aggregate_matching_proof(gateways.values(), did)
}

fn aggregate_proof_for_home(
    gateways: &BTreeMap<u64, GatewayAuthority>,
    home: &str,
    did: &str,
) -> Option<DeviceProof> {
    aggregate_matching_proof(
        gateways.values().filter(|gateway| gateway.home_id == home),
        did,
    )
}

fn aggregate_matching_proof<'a>(
    gateways: impl Iterator<Item = &'a GatewayAuthority>,
    did: &str,
) -> Option<DeviceProof> {
    gateways
        .filter_map(|gateway| gateway.device_paths.get(did))
        .copied()
        .map(|proof| DeviceProof {
            access: proof.access && proof.online != Some(false),
            notify: proof.notify && proof.online != Some(false),
            online: proof.online,
        })
        .reduce(|left, right| DeviceProof {
            access: left.access || right.access,
            notify: left.notify || right.notify,
            online: match (left.online, right.online) {
                (Some(false), Some(false)) => Some(false),
                (Some(true), _) | (_, Some(true)) => Some(true),
                _ => None,
            },
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        storage::{DeviceToken, Store, TokenSet, XiaomiRecord},
        xiaomi::{
            catalog::{CatalogHome, compile_spec},
            discovery::{GatewayEndpoint, InterfaceRecord, LinkType},
            gateway::GatewayDevice,
            runtime::{
                CommandTransport, ControlPath, SendGuard, TransportCommand, TransportFailure,
            },
        },
    };
    use futures_util::{FutureExt, future::LocalBoxFuture};
    use std::{net::Ipv4Addr, rc::Rc, time::Duration};

    struct NoTransport;
    impl CommandTransport for NoTransport {
        fn send(
            &self,
            _: ControlPath,
            _: TransportCommand,
            _: Duration,
            _: SendGuard,
        ) -> LocalBoxFuture<'static, Result<(), TransportFailure>> {
            async { Err(TransportFailure::Unavailable) }.boxed_local()
        }
    }

    fn credentials() -> XiaomiRecord {
        credentials_for_uid("10001")
    }

    fn credentials_for_uid(uid: &str) -> XiaomiRecord {
        XiaomiRecord {
            uid: uid.into(),
            region: "cn".into(),
            oauth_client_uuid: "123e4567-e89b-12d3-a456-426614174000".into(),
            redirect_uri: "http://127.0.0.1/callback".into(),
            tokens: TokenSet {
                access_token: "a".into(),
                refresh_token: "r".into(),
                expires_at: 10_000,
                refresh_at: 5_000,
            },
            virtual_did: "123456789012345".into(),
            private_key_pem: "-----BEGIN PRIVATE KEY-----\nk\n-----END PRIVATE KEY-----".into(),
            certificate_pem: "-----BEGIN CERTIFICATE-----\nc\n-----END CERTIFICATE-----".into(),
        }
    }

    fn proof(
        group: &str,
        gateway_did: u64,
        access: Option<bool>,
        notify: Option<bool>,
        session_generation: AuthSessionGeneration,
    ) -> AuthenticatedGateway {
        let source = Ipv4Addr::new(192, 168, 1, 2);
        let target = Ipv4Addr::new(192, 168, 1, 3);
        let network = NetworkSnapshot::select(
            vec![InterfaceRecord {
                index: 4,
                name: "en0".into(),
                address: source,
                netmask: Ipv4Addr::new(255, 255, 255, 0),
                up: true,
                point_to_point: false,
                loopback: false,
                link_type: LinkType::Ethernet,
            }],
            None,
        )
        .unwrap();
        let selected_endpoint = GatewayEndpoint {
            interface_index: 4,
            source_address: source,
            address: target,
            port: 8883,
        };
        AuthenticatedGateway {
            account: AccountId::new("10001").unwrap(),
            session_generation,
            candidate: GatewayCandidate {
                gateway_did,
                home_group: group.into(),
                endpoints: vec![selected_endpoint.clone()],
                unverified: true,
            },
            selected_endpoint,
            network,
            evidence: GatewayEvidence {
                gateway_did,
                peer_did: gateway_did.to_string(),
                epoch: NetworkEpoch::new(7),
                devices: vec![GatewayDevice {
                    did: "12345".into(),
                    name: "Light".into(),
                    urn: "urn".into(),
                    model: "yeelink.light.ml9".into(),
                    online: None,
                    spec_v2_access: access,
                    push_available: notify,
                }],
            },
        }
    }

    fn catalog(session_generation: AuthSessionGeneration) -> AdmissionCatalog {
        let document =
            include_str!("../../../tests/fixtures/miot_specs/yeelink.light.ml9.json").to_owned();
        let compiled = compile_spec("yeelink.light.ml9", &document).unwrap();
        AdmissionCatalog {
            account: AccountId::new("10001").unwrap(),
            session_generation,
            catalog: DeviceCatalog {
                uid: "10001".into(),
                homes: vec![CatalogHome {
                    id: "home-a".into(),
                    name: "Home".into(),
                    group_id: "0011223344556677".into(),
                    rooms: vec![],
                }],
                devices: vec![CatalogDevice {
                    home_id: "home-a".into(),
                    room_id: None,
                    parent_did: "12345".into(),
                    name: "Light".into(),
                    model: "yeelink.light.ml9".into(),
                    spec_type: Some(compiled.type_urn.clone()),
                    pid: Some(0),
                    token: None,
                    online: None,
                    local_ip: None,
                    parent_id: None,
                    features: compiled.features,
                }],
            },
            specifications: HashMap::from([(compiled.type_urn, document)]),
        }
    }

    fn lan_proof(
        directory: &AdmissionCatalog,
        session_generation: AuthSessionGeneration,
        device_index: usize,
        epoch: NetworkEpoch,
    ) -> AuthenticatedLan {
        let interface = crate::xiaomi::discovery::NetworkInterface {
            index: 4,
            name: "en0".into(),
            address: Ipv4Addr::new(192, 168, 1, 2),
            netmask: Ipv4Addr::new(255, 255, 255, 0),
            prefix_len: 24,
        };
        let target = LanTarget::from_catalog(
            &directory.catalog.devices[device_index],
            "home-a",
            interface.clone(),
            epoch,
        )
        .unwrap();
        let network = NetworkSnapshot::select(
            vec![InterfaceRecord {
                index: interface.index(),
                name: "en0".into(),
                address: interface.address(),
                netmask: interface.netmask(),
                up: true,
                point_to_point: false,
                loopback: false,
                link_type: LinkType::Ethernet,
            }],
            None,
        )
        .unwrap();
        AuthenticatedLan {
            account: directory.account.clone(),
            session_generation,
            evidence: LanEvidence {
                did: target.did(),
                address: target.address(),
                interface_index: interface.index(),
                epoch,
                signed_timestamp: 42,
                native_supported: true,
            },
            target,
            network,
            legacy_operation: LegacyOperationEvidence::Unverified,
        }
    }

    #[test]
    fn authenticated_notify_only_device_binds_and_publishes_without_control_path() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        store.xiaomi().replace(&credentials()).unwrap();
        let service = DeviceService::new();
        let commands = CommandRuntime::new(service.clone(), Rc::new(NoTransport));
        let session = store.xiaomi().snapshot().unwrap().session_generation;
        let mut admission = AdmissionController::new(
            store.devices(),
            service,
            commands,
            NetworkEpoch::new(7),
            session,
        );
        assert_eq!(
            admission
                .observe_gateway(
                    proof("0011223344556677", 1, None, Some(true), session),
                    &catalog(session),
                )
                .unwrap(),
            AdmissionStatus::Active
        );
        let snapshot = admission.snapshot().unwrap();
        assert_eq!(snapshot.binding.unwrap().home.as_str(), "home-a");
        assert!(!snapshot.features.is_empty());
        assert!(!snapshot.features[0].paths.gateway);
        assert_eq!(
            snapshot.features[0].gateways,
            vec![GatewayPathEvidence {
                gateway_did: 1,
                access: false,
                push: true,
                online: None,
            }]
        );
    }

    #[test]
    fn second_authenticated_home_suspends_existing_admission() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        store.xiaomi().replace(&credentials()).unwrap();
        let service = DeviceService::new();
        let commands = CommandRuntime::new(service.clone(), Rc::new(NoTransport));
        let session = store.xiaomi().snapshot().unwrap().session_generation;
        let mut admission = AdmissionController::new(
            store.devices(),
            service.clone(),
            commands,
            NetworkEpoch::new(7),
            session,
        );
        let mut directory = catalog(session);
        directory.catalog.homes.push(CatalogHome {
            id: "home-b".into(),
            name: "Other".into(),
            group_id: "8899aabbccddeeff".into(),
            rooms: vec![],
        });
        admission
            .observe_gateway(
                proof("0011223344556677", 1, Some(true), None, session),
                &directory,
            )
            .unwrap();
        assert_eq!(
            admission
                .observe_gateway(
                    proof("8899aabbccddeeff", 2, Some(true), None, session),
                    &directory,
                )
                .unwrap(),
            AdmissionStatus::SuspendedConflict
        );
        assert!(service.features().iter().all(|feature| !feature.admitted));
    }

    #[test]
    fn same_uid_relogin_prevents_stale_gateway_from_republishing() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        store.xiaomi().replace(&credentials()).unwrap();
        let service = DeviceService::new();
        let commands = CommandRuntime::new(service.clone(), Rc::new(NoTransport));
        let session = store.xiaomi().snapshot().unwrap().session_generation;
        let mut admission = AdmissionController::new(
            store.devices(),
            service.clone(),
            commands,
            NetworkEpoch::new(7),
            session,
        );
        let stale_gateway = proof("0011223344556677", 1, Some(true), None, session);
        let mut stale_catalog = catalog(session);
        stale_catalog.catalog.devices[0].token = Some(DeviceToken(vec![7; 16]));
        stale_catalog.catalog.devices[0].local_ip = Some("192.168.1.9".into());
        admission
            .observe_gateway(stale_gateway.clone(), &stale_catalog)
            .unwrap();
        let stale_lan = lan_proof(&stale_catalog, session, 0, NetworkEpoch::new(7));
        store.xiaomi().logout().unwrap();
        store.xiaomi().replace(&credentials()).unwrap();
        admission
            .observe_auth(&store.xiaomi().snapshot().unwrap())
            .unwrap();
        assert_eq!(
            admission
                .observe_gateway(stale_gateway, &stale_catalog)
                .unwrap(),
            AdmissionStatus::Unbound
        );
        assert!(!admission.confirm_lan(&stale_lan, &stale_catalog).unwrap());
        assert_eq!(service.features().len(), 1);
        assert!(service.features().iter().all(|feature| !feature.admitted));
    }

    #[test]
    fn removing_conflicting_home_restores_remaining_fresh_gateway() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        store.xiaomi().replace(&credentials()).unwrap();
        let session = store.xiaomi().snapshot().unwrap().session_generation;
        let service = DeviceService::new();
        let commands = CommandRuntime::new(service.clone(), Rc::new(NoTransport));
        let mut admission = AdmissionController::new(
            store.devices(),
            service.clone(),
            commands,
            NetworkEpoch::new(7),
            session,
        );
        let mut directory = catalog(session);
        directory.catalog.homes.push(CatalogHome {
            id: "home-b".into(),
            name: "Other".into(),
            group_id: "8899aabbccddeeff".into(),
            rooms: vec![],
        });
        admission
            .observe_gateway(
                proof("0011223344556677", 1, Some(true), None, session),
                &directory,
            )
            .unwrap();
        admission
            .observe_gateway(
                proof("8899aabbccddeeff", 2, Some(true), None, session),
                &directory,
            )
            .unwrap();
        admission.remove_gateway(2).unwrap();

        assert_eq!(admission.status(), AdmissionStatus::Active);
        assert!(service.features().iter().all(|feature| feature.admitted));
        assert!(admission.snapshot().unwrap().features[0].paths.gateway);
    }

    #[test]
    fn push_only_group_does_not_create_topology() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        store.xiaomi().replace(&credentials()).unwrap();
        let session = store.xiaomi().snapshot().unwrap().session_generation;
        let service = DeviceService::new();
        let commands = CommandRuntime::new(service.clone(), Rc::new(NoTransport));
        let mut admission = AdmissionController::new(
            store.devices(),
            service.clone(),
            commands,
            NetworkEpoch::new(7),
            session,
        );
        let mut directory = catalog(session);
        directory.catalog.devices[0].model = "mijia.light.group3".into();
        admission
            .observe_gateway(
                proof("0011223344556677", 1, None, Some(true), session),
                &directory,
            )
            .unwrap();

        assert!(service.features().is_empty());
        assert!(store.devices().load_features(true).unwrap().is_empty());
    }

    #[test]
    fn topology_failure_does_not_publish_partial_memory() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        store.xiaomi().replace(&credentials()).unwrap();
        let session = store.xiaomi().snapshot().unwrap().session_generation;
        let service = DeviceService::new();
        let commands = CommandRuntime::new(service.clone(), Rc::new(NoTransport));
        let mut admission = AdmissionController::new(
            store.devices(),
            service.clone(),
            commands,
            NetworkEpoch::new(7),
            session,
        );
        let mut directory = catalog(session);
        let mut second = directory.catalog.devices[0].features[0].clone();
        second.service_instance += 100;
        let failed_instance = second.service_instance;
        directory.catalog.devices[0].features.push(second);
        let connection = rusqlite::Connection::open(store.path()).unwrap();
        connection
            .execute_batch(&format!(
                "CREATE TRIGGER fail_admission_feature
                 BEFORE INSERT ON published_feature_definitions
                 WHEN NEW.service_instance={failed_instance}
                 BEGIN SELECT RAISE(ABORT, 'injected failure'); END;"
            ))
            .unwrap();
        drop(connection);

        assert!(
            admission
                .observe_gateway(
                    proof("0011223344556677", 1, Some(true), None, session),
                    &directory,
                )
                .is_err()
        );
        assert!(service.features().is_empty());
        assert!(admission.snapshot().unwrap().features.is_empty());
        assert!(store.devices().load_features(false).unwrap().is_empty());
    }

    #[test]
    fn gateway_offline_does_not_override_catalog_cloud_reachability() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        store.xiaomi().replace(&credentials()).unwrap();
        let session = store.xiaomi().snapshot().unwrap().session_generation;
        let service = DeviceService::new();
        let commands = CommandRuntime::new(service.clone(), Rc::new(NoTransport));
        let mut admission = AdmissionController::new(
            store.devices(),
            service,
            commands,
            NetworkEpoch::new(7),
            session,
        );
        let mut directory = catalog(session);
        directory.catalog.devices[0].online = Some(true);
        let mut gateway = proof("0011223344556677", 1, Some(true), None, session);
        gateway.evidence.devices[0].online = Some(false);
        admission.observe_gateway(gateway, &directory).unwrap();
        admission
            .observe_cloud(&CloudEvidence {
                account: AccountId::new("10001").unwrap(),
                session_generation: session,
                status: CloudStatus::Ready,
            })
            .unwrap();
        let feature = &admission.snapshot().unwrap().features[0];
        assert!(!feature.paths.gateway);
        assert!(feature.paths.cloud);
    }

    #[test]
    fn logout_preserves_endpoints_unavailable_and_same_uid_relogin_reuses_them() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        store.xiaomi().replace(&credentials()).unwrap();
        let session = store.xiaomi().snapshot().unwrap().session_generation;
        let service = DeviceService::new();
        let commands = CommandRuntime::new(service.clone(), Rc::new(NoTransport));
        let mut admission = AdmissionController::new(
            store.devices(),
            service.clone(),
            commands,
            NetworkEpoch::new(7),
            session,
        );
        admission
            .observe_gateway(
                proof("0011223344556677", 1, Some(true), None, session),
                &catalog(session),
            )
            .unwrap();
        let identity = store.devices().load_features(true).unwrap()[0].clone();

        store.xiaomi().logout().unwrap();
        admission
            .observe_auth(&store.xiaomi().snapshot().unwrap())
            .unwrap();
        assert_eq!(service.features().len(), 1);
        assert!(service.features().iter().all(|feature| !feature.admitted));
        assert!(
            admission
                .snapshot()
                .unwrap()
                .features
                .iter()
                .all(|feature| feature.paths == OperationPaths::default())
        );

        store.xiaomi().replace(&credentials()).unwrap();
        let relogin = store.xiaomi().snapshot().unwrap();
        admission.observe_auth(&relogin).unwrap();
        admission
            .observe_gateway(
                proof(
                    "0011223344556677",
                    1,
                    Some(true),
                    None,
                    relogin.session_generation,
                ),
                &catalog(relogin.session_generation),
            )
            .unwrap();
        assert_eq!(store.devices().load_features(true).unwrap()[0], identity);
    }

    #[test]
    fn complete_catalog_removes_device_after_last_gateway_without_treating_path_loss_as_removal() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        store.xiaomi().replace(&credentials()).unwrap();
        let session = store.xiaomi().snapshot().unwrap().session_generation;
        let service = DeviceService::new();
        let commands = CommandRuntime::new(service.clone(), Rc::new(NoTransport));
        let mut admission = AdmissionController::new(
            store.devices(),
            service.clone(),
            commands,
            NetworkEpoch::new(7),
            session,
        );
        let directory = catalog(session);
        admission
            .observe_gateway(
                proof("0011223344556677", 1, Some(true), None, session),
                &directory,
            )
            .unwrap();
        admission.remove_gateway(1).unwrap();
        assert_eq!(service.features().len(), 1);
        admission.apply_complete_catalog(&directory).unwrap();
        assert_eq!(service.features().len(), 1);

        let mut removed = directory;
        removed.catalog.devices.clear();
        admission.apply_complete_catalog(&removed).unwrap();
        assert!(service.features().is_empty());
        assert!(store.devices().load_features(true).unwrap().is_empty());
    }

    #[test]
    fn existing_binding_accepts_fresh_lan_proof_without_gateway() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        store.xiaomi().replace(&credentials()).unwrap();
        let session = store.xiaomi().snapshot().unwrap().session_generation;
        let service = DeviceService::new();
        let commands = CommandRuntime::new(service.clone(), Rc::new(NoTransport));
        let mut initial = AdmissionController::new(
            store.devices(),
            service,
            commands,
            NetworkEpoch::new(7),
            session,
        );
        let mut directory = catalog(session);
        directory.catalog.devices[0].token = Some(DeviceToken(vec![7; 16]));
        directory.catalog.devices[0].local_ip = Some("192.168.1.9".into());
        initial
            .observe_gateway(
                proof("0011223344556677", 1, Some(true), None, session),
                &directory,
            )
            .unwrap();
        drop(initial);

        let service = DeviceService::new();
        let commands = CommandRuntime::new(service.clone(), Rc::new(NoTransport));
        let mut restored = AdmissionController::new(
            store.devices(),
            service,
            commands,
            NetworkEpoch::new(7),
            session,
        );
        restored.restore_archived().unwrap();
        assert!(
            restored
                .confirm_lan(
                    &lan_proof(&directory, session, 0, NetworkEpoch::new(7)),
                    &directory,
                )
                .unwrap()
        );
        let snapshot = restored.snapshot().unwrap();
        assert!(snapshot.features[0].paths.lan);
        assert!(!snapshot.features[0].paths.gateway);
    }

    #[test]
    fn missing_spec_keeps_archive_unavailable_but_complete_new_spec_deactivates_removed_feature() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        store.xiaomi().replace(&credentials()).unwrap();
        let session = store.xiaomi().snapshot().unwrap().session_generation;
        let service = DeviceService::new();
        let commands = CommandRuntime::new(service.clone(), Rc::new(NoTransport));
        let mut admission = AdmissionController::new(
            store.devices(),
            service.clone(),
            commands,
            NetworkEpoch::new(7),
            session,
        );
        let directory = catalog(session);
        admission
            .observe_gateway(
                proof("0011223344556677", 1, Some(true), None, session),
                &directory,
            )
            .unwrap();

        let mut missing = directory.clone();
        missing.specifications.clear();
        admission.apply_complete_catalog(&missing).unwrap();
        assert_eq!(service.features().len(), 1);
        assert!(!service.features()[0].admitted);
        assert_eq!(store.devices().load_features(true).unwrap().len(), 1);

        let mut removed = directory;
        removed.catalog.devices[0].features.clear();
        admission.apply_complete_catalog(&removed).unwrap();
        assert!(service.features().is_empty());
        assert!(store.devices().load_features(true).unwrap().is_empty());
    }

    #[test]
    fn different_uid_login_removes_old_account_from_memory() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        store.xiaomi().replace(&credentials()).unwrap();
        let session = store.xiaomi().snapshot().unwrap().session_generation;
        let service = DeviceService::new();
        let commands = CommandRuntime::new(service.clone(), Rc::new(NoTransport));
        let mut admission = AdmissionController::new(
            store.devices(),
            service.clone(),
            commands,
            NetworkEpoch::new(7),
            session,
        );
        admission
            .observe_gateway(
                proof("0011223344556677", 1, Some(true), None, session),
                &catalog(session),
            )
            .unwrap();
        store
            .xiaomi()
            .replace(&credentials_for_uid("20002"))
            .unwrap();
        admission
            .observe_auth(&store.xiaomi().snapshot().unwrap())
            .unwrap();
        assert!(service.features().is_empty());
        assert!(store.devices().load_features(true).unwrap().is_empty());

        let replacement = store.xiaomi().snapshot().unwrap();
        let mut new_catalog = catalog(replacement.session_generation);
        new_catalog.account = AccountId::new("20002").unwrap();
        new_catalog.catalog.uid = "20002".into();
        let mut new_gateway = proof(
            "0011223344556677",
            1,
            Some(true),
            None,
            replacement.session_generation,
        );
        new_gateway.account = AccountId::new("20002").unwrap();
        assert_eq!(
            admission
                .observe_gateway(new_gateway, &new_catalog)
                .unwrap(),
            AdmissionStatus::Active
        );
        assert!(service.features().iter().all(|feature| feature.admitted));
    }

    #[test]
    fn rename_updates_metadata_without_reallocating_identity() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        store.xiaomi().replace(&credentials()).unwrap();
        let session = store.xiaomi().snapshot().unwrap().session_generation;
        let service = DeviceService::new();
        let commands = CommandRuntime::new(service.clone(), Rc::new(NoTransport));
        let mut admission = AdmissionController::new(
            store.devices(),
            service.clone(),
            commands,
            NetworkEpoch::new(7),
            session,
        );
        let directory = catalog(session);
        admission
            .observe_gateway(
                proof("0011223344556677", 1, Some(true), None, session),
                &directory,
            )
            .unwrap();
        let allocated = store.devices().load_features(true).unwrap()[0].clone();
        let mut renamed = directory;
        renamed.catalog.devices[0].name = "Desk lamp".into();
        admission.apply_complete_catalog(&renamed).unwrap();

        assert_eq!(service.features()[0].name, "Desk lamp");
        assert_eq!(store.devices().load_features(true).unwrap()[0], allocated);
    }

    #[test]
    fn network_change_keeps_historical_admission_but_does_not_admit_a_new_catalog_device() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        store.xiaomi().replace(&credentials()).unwrap();
        let session = store.xiaomi().snapshot().unwrap().session_generation;
        let service = DeviceService::new();
        let commands = CommandRuntime::new(service.clone(), Rc::new(NoTransport));
        let mut admission = AdmissionController::new(
            store.devices(),
            service.clone(),
            commands,
            NetworkEpoch::new(7),
            session,
        );
        let mut directory = catalog(session);
        directory.catalog.devices[0].token = Some(DeviceToken(vec![7; 16]));
        directory.catalog.devices[0].local_ip = Some("192.168.1.9".into());
        let mut second = directory.catalog.devices[0].clone();
        second.parent_did = "67890".into();
        second.name = "Other light".into();
        second.token = Some(DeviceToken(vec![8; 16]));
        second.local_ip = Some("192.168.1.10".into());
        directory.catalog.devices.push(second);
        let mut gateway = proof("0011223344556677", 1, Some(true), None, session);
        let mut second_gateway_device = gateway.evidence.devices[0].clone();
        second_gateway_device.did = "67890".into();
        gateway.evidence.devices.push(second_gateway_device);
        admission.observe_gateway(gateway, &directory).unwrap();
        admission
            .observe_cloud(&CloudEvidence {
                account: AccountId::new("10001").unwrap(),
                session_generation: session,
                status: CloudStatus::Ready,
            })
            .unwrap();
        assert_eq!(service.features().len(), 2);

        admission.invalidate(NetworkEpoch::new(8));
        let mut new_device = directory.catalog.devices[0].clone();
        new_device.parent_did = "new-cloud-only".into();
        new_device.name = "New cloud-only light".into();
        new_device.token = None;
        new_device.local_ip = None;
        directory.catalog.devices.push(new_device);
        admission.apply_complete_catalog(&directory).unwrap();
        let features = service.features();
        assert_eq!(
            features.iter().filter(|feature| feature.admitted).count(),
            2
        );
        assert!(
            features.iter().all(|feature| {
                feature.identity.physical.parent_did.as_str() != "new-cloud-only"
            })
        );
        let snapshot = admission.snapshot().unwrap();
        assert_eq!(snapshot.epoch, NetworkEpoch::new(8));
        assert!(snapshot.features.iter().all(|feature| feature.paths
            == OperationPaths {
                cloud: true,
                ..OperationPaths::default()
            }));
    }

    #[test]
    fn complete_catalog_restores_persisted_admission_after_restart_for_the_bound_account_home() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        store.xiaomi().replace(&credentials()).unwrap();
        let session = store.xiaomi().snapshot().unwrap().session_generation;
        let catalog = catalog(session);
        {
            let service = DeviceService::new();
            let commands = CommandRuntime::new(service.clone(), Rc::new(NoTransport));
            let mut admission = AdmissionController::new(
                store.devices(),
                service,
                commands,
                NetworkEpoch::new(7),
                session,
            );
            admission
                .observe_gateway(
                    proof("0011223344556677", 1, Some(true), None, session),
                    &catalog,
                )
                .unwrap();
        }

        let service = DeviceService::new();
        let commands = CommandRuntime::new(service.clone(), Rc::new(NoTransport));
        let mut restored = AdmissionController::new(
            store.devices(),
            service.clone(),
            commands,
            NetworkEpoch::new(9),
            session,
        );
        restored.restore_archived().unwrap();
        restored.apply_complete_catalog(&catalog).unwrap();

        assert_eq!(restored.status(), AdmissionStatus::Active);
        assert_eq!(service.features().len(), 1);
        assert!(service.features()[0].admitted);
        assert_eq!(
            service.features()[0].identity.physical.home.as_str(),
            "home-a"
        );
    }

    #[test]
    fn descriptor_change_revokes_before_storage_and_publishes_a_new_generation_after_commit() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        store.xiaomi().replace(&credentials()).unwrap();
        let session = store.xiaomi().snapshot().unwrap().session_generation;
        let service = DeviceService::new();
        let commands = CommandRuntime::new(service.clone(), Rc::new(NoTransport));
        let mut admission = AdmissionController::new(
            store.devices(),
            service.clone(),
            commands,
            NetworkEpoch::new(7),
            session,
        );
        let directory = catalog(session);
        admission
            .observe_gateway(
                proof("0011223344556677", 1, Some(true), None, session),
                &directory,
            )
            .unwrap();
        let previous = admission.snapshot().unwrap().features[0].runtime.clone();
        let mut changed = directory;
        let type_urn = changed.catalog.devices[0].spec_type.clone().unwrap();
        let document = changed.specifications[&type_urn]
            .replace("\"value-range\":[1,100,1]", "\"value-range\":[1,90,1]");
        changed.catalog.devices[0].features = compile_spec("yeelink.light.ml9", &document)
            .unwrap()
            .features;
        changed.specifications.insert(type_urn, document);
        let connection = rusqlite::Connection::open(store.path()).unwrap();
        connection
            .execute_batch(
                "CREATE TRIGGER fail_descriptor_update
                 BEFORE UPDATE ON published_feature_definitions
                 BEGIN SELECT RAISE(ABORT, 'injected failure'); END;",
            )
            .unwrap();
        drop(connection);

        assert!(admission.apply_complete_catalog(&changed).is_err());
        assert!(service.features().iter().all(|feature| !feature.admitted));
        assert!(admission.snapshot().unwrap().features.is_empty());
        let connection = rusqlite::Connection::open(store.path()).unwrap();
        connection
            .execute_batch("DROP TRIGGER fail_descriptor_update")
            .unwrap();
        drop(connection);
        admission.apply_complete_catalog(&changed).unwrap();
        let current = admission.snapshot().unwrap().features[0].runtime.clone();
        assert_ne!(current.descriptor, previous.descriptor);
        assert!(current.authority_generation > previous.authority_generation);
    }

    mod review_regressions {
        use super::*;

        fn setup() -> (
            tempfile::TempDir,
            Store,
            DeviceService,
            AdmissionController,
            AdmissionCatalog,
        ) {
            let directory = tempfile::tempdir().unwrap();
            let store = Store::open(directory.path()).unwrap();
            store.xiaomi().replace(&credentials()).unwrap();
            let session = store.xiaomi().snapshot().unwrap().session_generation;
            let service = DeviceService::new();
            let commands = CommandRuntime::new(service.clone(), Rc::new(NoTransport));
            let admission = AdmissionController::new(
                store.devices(),
                service.clone(),
                commands,
                NetworkEpoch::new(7),
                session,
            );
            (directory, store, service, admission, catalog(session))
        }

        fn admit(admission: &mut AdmissionController, directory: &AdmissionCatalog) {
            admission
                .observe_gateway(
                    proof(
                        "0011223344556677",
                        1,
                        Some(true),
                        Some(true),
                        directory.session_generation,
                    ),
                    directory,
                )
                .unwrap();
        }

        fn enable_lan(
            admission: &mut AdmissionController,
            directory: &mut AdmissionCatalog,
            supported: bool,
        ) {
            directory.catalog.devices[0].token = Some(DeviceToken(vec![7; 16]));
            directory.catalog.devices[0].local_ip = Some("192.168.1.9".into());
            let mut evidence = lan_proof(
                directory,
                directory.session_generation,
                0,
                NetworkEpoch::new(7),
            );
            evidence.evidence.native_supported = supported;
            assert!(admission.confirm_lan(&evidence, directory).unwrap());
        }

        #[test]
        fn unchanged_catalog_preserves_verified_lan_path() {
            let (_directory, _store, _service, mut admission, mut catalog) = setup();
            admit(&mut admission, &catalog);
            enable_lan(&mut admission, &mut catalog, true);
            admission.apply_complete_catalog(&catalog).unwrap();
            assert!(admission.snapshot().unwrap().features[0].paths.lan);
        }

        #[test]
        fn gateway_loss_does_not_turn_signed_identity_into_operation_support() {
            let (_directory, _store, _service, mut admission, mut catalog) = setup();
            admit(&mut admission, &catalog);
            enable_lan(&mut admission, &mut catalog, false);
            assert!(!admission.snapshot().unwrap().features[0].paths.lan);
            admission.remove_gateway(1).unwrap();
            assert!(!admission.snapshot().unwrap().features[0].paths.lan);
        }

        #[test]
        fn retry_after_removal_write_failure_deactivates_persisted_endpoint() {
            let (_directory, store, service, mut admission, mut catalog) = setup();
            admit(&mut admission, &catalog);
            let removed_device = catalog.catalog.devices[0].clone();
            let connection = rusqlite::Connection::open(store.path()).unwrap();
            connection
                .execute_batch(
                    "CREATE TRIGGER fail_deactivation BEFORE UPDATE ON feature_identities
                 WHEN NEW.active=0 BEGIN SELECT RAISE(ABORT, 'injected failure'); END;",
                )
                .unwrap();
            catalog.catalog.devices.clear();
            assert!(admission.apply_complete_catalog(&catalog).is_err());
            assert!(service.features().is_empty());
            connection
                .execute_batch("DROP TRIGGER fail_deactivation")
                .unwrap();
            admission.apply_complete_catalog(&catalog).unwrap();
            assert!(store.devices().load_features(true).unwrap().is_empty());
            catalog.catalog.devices.push(removed_device);
            admission.apply_complete_catalog(&catalog).unwrap();
            assert!(service.features().is_empty());
            admission
                .observe_gateway(
                    proof(
                        "0011223344556677",
                        1,
                        Some(true),
                        Some(true),
                        catalog.session_generation,
                    ),
                    &catalog,
                )
                .unwrap();
            assert!(!service.features().is_empty());
        }

        #[test]
        fn cloud_recovery_cannot_reenable_paths_during_home_conflict() {
            let (_directory, _store, _service, mut admission, mut catalog) = setup();
            admit(&mut admission, &catalog);
            catalog.catalog.homes.push(CatalogHome {
                id: "home-b".into(),
                name: "Other home".into(),
                group_id: "8899aabbccddeeff".into(),
                rooms: vec![],
            });
            admission
                .observe_gateway(
                    proof(
                        "8899aabbccddeeff",
                        2,
                        Some(true),
                        Some(true),
                        catalog.session_generation,
                    ),
                    &catalog,
                )
                .unwrap();
            admission
                .observe_cloud(&CloudEvidence {
                    account: catalog.account.clone(),
                    session_generation: catalog.session_generation,
                    status: CloudStatus::Ready,
                })
                .unwrap();
            assert_eq!(admission.status(), AdmissionStatus::SuspendedConflict);
            let snapshot = admission.snapshot().unwrap();
            assert!(
                snapshot
                    .features
                    .iter()
                    .all(|feature| feature.paths == OperationPaths::default())
            );
            assert!(snapshot.features.iter().all(|feature| {
                feature
                    .gateways
                    .iter()
                    .all(|gateway| !gateway.access && !gateway.push)
            }));
        }

        #[test]
        fn rotated_token_discards_old_lan_session_evidence() {
            let (_directory, _store, _service, mut admission, mut catalog) = setup();
            admit(&mut admission, &catalog);
            enable_lan(&mut admission, &mut catalog, true);
            catalog.catalog.devices[0].token = Some(DeviceToken(vec![8; 16]));
            admission.apply_complete_catalog(&catalog).unwrap();
            let snapshot = admission.snapshot().unwrap();
            assert!(!snapshot.features[0].paths.lan);
            assert!(snapshot.features[0].lan_evidence.is_none());
        }

        #[test]
        fn unchanged_channel_label_does_not_replace_authority_generation() {
            let (_directory, _store, _service, mut admission, mut catalog) = setup();
            catalog.catalog.devices[0].features[0].name = "Desk channel".into();
            admit(&mut admission, &catalog);
            let previous = admission.snapshot().unwrap().features[0]
                .runtime
                .authority_generation;
            admission.apply_complete_catalog(&catalog).unwrap();
            assert_eq!(
                admission.snapshot().unwrap().features[0]
                    .runtime
                    .authority_generation,
                previous
            );
        }

        #[test]
        fn changing_one_descriptor_preserves_other_device_authority() {
            let (_directory, _store, _service, mut admission, mut catalog) = setup();
            let document =
                include_str!("../../../tests/fixtures/miot_specs/cuco.plug.v3.json").to_owned();
            let compiled = compile_spec("cuco.plug.v3", &document).unwrap();
            let mut other = catalog.catalog.devices[0].clone();
            other.parent_did = "67890".into();
            other.model = "cuco.plug.v3".into();
            other.spec_type = Some(compiled.type_urn.clone());
            other.features = compiled.features;
            catalog.specifications.insert(compiled.type_urn, document);
            catalog.catalog.devices.push(other);
            let mut gateway = proof(
                "0011223344556677",
                1,
                Some(true),
                Some(true),
                catalog.session_generation,
            );
            let mut other_gateway_device = gateway.evidence.devices[0].clone();
            other_gateway_device.did = "67890".into();
            other_gateway_device.model = "cuco.plug.v3".into();
            gateway.evidence.devices.push(other_gateway_device);
            admission.observe_gateway(gateway, &catalog).unwrap();
            let previous = admission
                .snapshot()
                .unwrap()
                .features
                .into_iter()
                .find(|feature| feature.identity.physical.parent_did.as_str() == "67890")
                .unwrap()
                .runtime;
            let type_urn = catalog.catalog.devices[0].spec_type.clone().unwrap();
            let document = catalog.specifications[&type_urn]
                .replace("\"value-range\":[1,100,1]", "\"value-range\":[1,90,1]");
            catalog.catalog.devices[0].features = compile_spec("yeelink.light.ml9", &document)
                .unwrap()
                .features;
            catalog.specifications.insert(type_urn, document);
            admission.apply_complete_catalog(&catalog).unwrap();
            let current = admission
                .snapshot()
                .unwrap()
                .features
                .into_iter()
                .find(|feature| feature.identity == previous.identity)
                .unwrap()
                .runtime;
            assert_eq!(current.authority_generation, previous.authority_generation);
        }

        #[test]
        fn only_verified_mcn02_legacy_read_enables_legacy_lan_operations() {
            let (_directory, _store, _service, mut admission, mut catalog) = setup();
            let document = include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/miot_specs/lumi.acpartner.mcn02.json"
            ))
            .to_owned();
            let compiled = compile_spec("lumi.acpartner.mcn02", &document).unwrap();
            catalog.catalog.devices[0].model = "lumi.acpartner.mcn02".into();
            catalog.catalog.devices[0].spec_type = Some(compiled.type_urn.clone());
            catalog.catalog.devices[0].features = compiled.features;
            catalog.catalog.devices[0].token = Some(DeviceToken(vec![7; 16]));
            catalog.catalog.devices[0].local_ip = Some("192.168.1.9".into());
            catalog.specifications.clear();
            catalog.specifications.insert(compiled.type_urn, document);
            admit(&mut admission, &catalog);
            let mut legacy = lan_proof(
                &catalog,
                catalog.session_generation,
                0,
                NetworkEpoch::new(7),
            );
            legacy.evidence.native_supported = false;
            legacy.legacy_operation = LegacyOperationEvidence::SuccessfulRead;
            assert!(admission.confirm_lan(&legacy, &catalog).unwrap());
            assert!(admission.snapshot().unwrap().features[0].paths.lan);

            let (_directory, _store, _service, mut other, mut ordinary) = setup();
            admit(&mut other, &ordinary);
            ordinary.catalog.devices[0].token = Some(DeviceToken(vec![7; 16]));
            ordinary.catalog.devices[0].local_ip = Some("192.168.1.9".into());
            let mut claimed_legacy = lan_proof(
                &ordinary,
                ordinary.session_generation,
                0,
                NetworkEpoch::new(7),
            );
            claimed_legacy.evidence.native_supported = false;
            claimed_legacy.legacy_operation = LegacyOperationEvidence::SuccessfulRead;
            assert!(other.confirm_lan(&claimed_legacy, &ordinary).unwrap());
            assert!(!other.snapshot().unwrap().features[0].paths.lan);
        }
    }
}
