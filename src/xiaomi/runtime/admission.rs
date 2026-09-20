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
pub(crate) struct AdmissionCatalog {
    pub account: AccountId,
    pub session_generation: AuthSessionGeneration,
    pub catalog: DeviceCatalog,
    pub specifications: HashMap<String, String>,
}

#[derive(Clone, Debug)]
pub(crate) struct AuthenticatedGateway {
    pub account: AccountId,
    pub session_generation: AuthSessionGeneration,
    pub candidate: GatewayCandidate,
    pub selected_endpoint: crate::xiaomi::discovery::GatewayEndpoint,
    pub network: NetworkSnapshot,
    pub evidence: GatewayEvidence,
}

#[derive(Clone, Debug)]
pub(crate) struct AuthenticatedLan {
    pub account: AccountId,
    pub session_generation: AuthSessionGeneration,
    pub target: LanTarget,
    pub network: NetworkSnapshot,
    pub evidence: LanEvidence,
    pub legacy_operation: LegacyOperationEvidence,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LegacyOperationEvidence {
    Unverified,
    SuccessfulRead,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CloudEvidence {
    pub account: AccountId,
    pub session_generation: AuthSessionGeneration,
    pub status: CloudStatus,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CloudStatus {
    Ready,
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
    pub(crate) runtime: RuntimeFeature,
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

pub(crate) struct AdmissionController {
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

    #[cfg(test)]
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
                    feature.definition.service_instance == definition.feature.service_instance
                        && feature.definition.role == definition.feature.role
                })
                .ok_or_else(|| {
                    StorageError::new(
                        self.store.path(),
                        "restore published Xiaomi feature",
                        "Published spec no longer contains the feature",
                    )
                })?;
            self.service.restore(
                definition.feature,
                definition.name,
                descriptor.definition.capabilities,
            );
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
                service_instance: descriptor.definition.service_instance,
                role: descriptor.definition.role,
            };
            definitions.push(PublishedFeatureDefinition {
                feature: identity,
                model: device.model.clone(),
                spec_document: document.clone(),
                name: if device.features.len() == 1 {
                    device.name.clone()
                } else if descriptor.definition.name == device.name {
                    descriptor.definition.name.clone()
                } else {
                    format!("{} {}", device.name, descriptor.definition.name)
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
            descriptor.definition.capabilities.clone(),
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
            descriptor.definition.service_instance == definition.feature.service_instance
                && descriptor.definition.role == definition.feature.role
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
                    descriptor.definition.service_instance == identity.service_instance
                        && descriptor.definition.role == identity.role
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
mod tests;
