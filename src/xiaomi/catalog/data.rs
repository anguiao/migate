use super::{CompileError, CompiledSpec, FeatureDescriptor, compile_spec};
use crate::device::{AccountId, DeviceDid, FeatureRole, HomeId, PhysicalDeviceId};
use crate::storage::{
    AuthRevision, CachedSpec, CatalogDeviceMetadata, CatalogHomeRecord, CatalogRoomRecord,
    CatalogSnapshot, DeviceRecord, DeviceStore, DeviceToken,
};
use crate::xiaomi::cloud::OwnedCatalog;
use std::collections::{BTreeMap, HashMap};

#[derive(Clone, Debug, PartialEq)]
pub struct DeviceCatalog {
    pub uid: String,
    pub homes: Vec<CatalogHome>,
    pub devices: Vec<CatalogDevice>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogHome {
    pub id: String,
    pub name: String,
    pub group_id: String,
    pub rooms: Vec<CatalogRoom>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogRoom {
    pub id: String,
    pub name: String,
}
#[derive(Clone, Debug, PartialEq)]
pub struct CatalogDevice {
    pub home_id: String,
    pub room_id: Option<String>,
    pub parent_did: String,
    pub name: String,
    pub model: String,
    pub spec_type: Option<String>,
    pub pid: Option<i64>,
    pub token: Option<DeviceToken>,
    pub online: Option<bool>,
    pub local_ip: Option<String>,
    pub parent_id: Option<String>,
    pub features: Vec<FeatureDescriptor>,
}

pub fn assemble_catalog(
    cloud: &OwnedCatalog,
    specs: &HashMap<String, String>,
) -> Result<DeviceCatalog, CompileError> {
    let mut devices = Vec::new();
    for home in &cloud.homes {
        let membership: std::collections::HashSet<_> = home
            .dids
            .iter()
            .chain(home.rooms.iter().flat_map(|room| &room.dids))
            .map(|did| split_did(did).0)
            .collect();
        let mut entries: BTreeMap<String, Vec<&crate::xiaomi::cloud::CloudDevice>> =
            BTreeMap::new();
        for device in cloud
            .devices
            .iter()
            .filter(|device| membership.contains(split_did(&device.did).0))
        {
            let (parent, _) = split_did(&device.did);
            entries.entry(parent.to_owned()).or_default().push(device);
        }
        for (parent_did, entries) in entries {
            let parent = entries
                .iter()
                .find(|device| device.did == parent_did)
                .copied();
            let diagnostic = parent.unwrap_or(entries[0]);
            let mut compiled = match parent {
                Some(parent) => {
                    if let Some(document) = parent
                        .spec_type
                        .as_ref()
                        .and_then(|type_urn| specs.get(type_urn))
                    {
                        compile_spec(&parent.model, document)?
                    } else {
                        CompiledSpec {
                            type_urn: parent.spec_type.clone().unwrap_or_default(),
                            features: vec![],
                        }
                    }
                }
                None => CompiledSpec {
                    type_urn: String::new(),
                    features: vec![],
                },
            };
            if parent.is_some() {
                for split in entries
                    .iter()
                    .filter_map(|device| split_did(&device.did).1.map(|siid| (*device, siid)))
                {
                    if let Some(feature) = compiled
                        .features
                        .iter_mut()
                        .find(|feature| feature.service_instance == split.1)
                    {
                        feature.name = split.0.name.clone();
                    }
                }
            }
            let room_id = home
                .rooms
                .iter()
                .find(|room| room.dids.iter().any(|did| split_did(did).0 == parent_did))
                .map(|room| room.id.clone());
            devices.push(CatalogDevice {
                home_id: home.id.clone(),
                room_id,
                parent_did,
                name: diagnostic.name.clone(),
                model: diagnostic.model.clone(),
                spec_type: parent.and_then(|parent| parent.spec_type.clone()),
                pid: parent.and_then(|parent| parent.pid),
                token: parent.and_then(|parent| parent.token.clone()),
                online: parent.and_then(|parent| parent.online),
                local_ip: parent.and_then(|parent| parent.local_ip.clone()),
                parent_id: parent.and_then(|parent| parent.parent_id.clone()),
                features: compiled.features,
            });
        }
    }
    Ok(DeviceCatalog {
        uid: cloud.uid.clone(),
        homes: cloud
            .homes
            .iter()
            .map(|home| CatalogHome {
                id: home.id.clone(),
                name: home.name.clone(),
                group_id: home.group_id.clone(),
                rooms: home
                    .rooms
                    .iter()
                    .map(|room| CatalogRoom {
                        id: room.id.clone(),
                        name: room.name.clone(),
                    })
                    .collect(),
            })
            .collect(),
        devices,
    })
}

pub fn persist_catalog(
    store: &DeviceStore,
    catalog: &DeviceCatalog,
    specs: &HashMap<String, String>,
    fetched_at: i64,
    expected: AuthRevision,
) -> Result<bool, crate::storage::StorageError> {
    let account = AccountId::new(catalog.uid.clone()).map_err(|error| {
        crate::storage::StorageError::new(store.path(), "validate Xiaomi catalog account", error)
    })?;
    let homes = catalog
        .homes
        .iter()
        .map(|home| CatalogHomeRecord {
            home_id: home.id.clone(),
            name: home.name.clone(),
            group_id: home.group_id.clone(),
        })
        .collect();
    let rooms = catalog
        .homes
        .iter()
        .flat_map(|home| {
            home.rooms.iter().map(|room| CatalogRoomRecord {
                home_id: home.id.clone(),
                room_id: room.id.clone(),
                name: room.name.clone(),
            })
        })
        .collect();
    let mut devices = Vec::new();
    for device in &catalog.devices {
        let identity = PhysicalDeviceId {
            account: account.clone(),
            home: HomeId::new(device.home_id.clone()).map_err(|error| {
                crate::storage::StorageError::new(
                    store.path(),
                    "validate Xiaomi catalog home",
                    error,
                )
            })?,
            parent_did: DeviceDid::new(device.parent_did.clone()).map_err(|error| {
                crate::storage::StorageError::new(
                    store.path(),
                    "validate Xiaomi catalog device",
                    error,
                )
            })?,
        };
        let feature_document = serde_json::to_string(&device.features.iter().map(|feature| serde_json::json!({"siid":feature.service_instance,"role":feature.role.as_str(),"name":feature.name})).collect::<Vec<_>>()).map_err(|error| crate::storage::StorageError::new(store.path(), "encode Xiaomi feature metadata", error))?;
        let record = DeviceRecord {
            identity,
            model: device.model.clone(),
            name: device.name.clone(),
            room_id: device.room_id.clone(),
            admitted: false,
        };
        let metadata = CatalogDeviceMetadata {
            spec_type: device.spec_type.clone(),
            pid: device.pid,
            local_ip: device.local_ip.clone(),
            parent_id: device.parent_id.clone(),
            online: device.online,
            feature_document,
        };
        let token = device
            .token
            .as_ref()
            .and_then(|token| <[u8; 16]>::try_from(token.0.as_slice()).ok());
        devices.push((record, metadata, token));
    }
    let specs = specs
        .iter()
        .map(|(type_urn, document)| CachedSpec {
            type_urn: type_urn.clone(),
            document: document.clone(),
            fetched_at,
        })
        .collect();
    store.replace_catalog_if_revision(
        &CatalogSnapshot {
            account,
            homes,
            rooms,
            devices,
            specs,
        },
        expected,
    )
}

pub fn restore_catalog(
    store: &DeviceStore,
    uid: &str,
) -> Result<DeviceCatalog, crate::storage::StorageError> {
    let account = AccountId::new(uid.to_owned()).map_err(|error| {
        crate::storage::StorageError::new(store.path(), "validate Xiaomi catalog account", error)
    })?;
    let stored = store.load_catalog(&account)?;
    let mut devices = Vec::with_capacity(stored.devices.len());
    for (record, metadata) in stored.devices {
        let mut features = if let Some(type_urn) = &metadata.spec_type {
            match store.load_spec(type_urn)? {
                Some(spec) => {
                    compile_spec(&record.model, &spec.document)
                        .map_err(|error| {
                            crate::storage::StorageError::new(
                                store.path(),
                                "compile cached Xiaomi device spec",
                                error,
                            )
                        })?
                        .features
                }
                None => vec![],
            }
        } else {
            vec![]
        };
        restore_features(store, &metadata.feature_document, &mut features)?;
        devices.push(CatalogDevice {
            home_id: record.identity.home.as_str().to_owned(),
            room_id: record.room_id,
            parent_did: record.identity.parent_did.as_str().to_owned(),
            name: record.name,
            model: record.model,
            spec_type: metadata.spec_type,
            pid: metadata.pid,
            token: None,
            online: metadata.online,
            local_ip: metadata.local_ip,
            parent_id: metadata.parent_id,
            features,
        });
    }
    Ok(DeviceCatalog {
        uid: uid.to_owned(),
        homes: stored
            .homes
            .into_iter()
            .map(|home| CatalogHome {
                rooms: stored
                    .rooms
                    .iter()
                    .filter(|room| room.home_id == home.home_id)
                    .map(|room| CatalogRoom {
                        id: room.room_id.clone(),
                        name: room.name.clone(),
                    })
                    .collect(),
                id: home.home_id,
                name: home.name,
                group_id: home.group_id,
            })
            .collect(),
        devices,
    })
}

fn restore_features(
    store: &DeviceStore,
    document: &str,
    features: &mut Vec<FeatureDescriptor>,
) -> Result<(), crate::storage::StorageError> {
    let entries = serde_json::from_str::<serde_json::Value>(document)
        .ok()
        .and_then(|value| value.as_array().cloned())
        .ok_or_else(|| {
            crate::storage::StorageError::new(
                store.path(),
                "decode Xiaomi feature metadata",
                "Feature metadata must be an array",
            )
        })?;
    let mut metadata = Vec::with_capacity(entries.len());
    for entry in entries {
        let object = entry.as_object().ok_or_else(|| {
            crate::storage::StorageError::new(
                store.path(),
                "decode Xiaomi feature metadata",
                "Feature metadata entry must be an object",
            )
        })?;
        let siid = object
            .get("siid")
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .ok_or_else(|| {
                crate::storage::StorageError::new(
                    store.path(),
                    "decode Xiaomi feature metadata",
                    "Feature metadata has an invalid service IID",
                )
            })?;
        let name = object
            .get("name")
            .and_then(serde_json::Value::as_str)
            .filter(|name| !name.is_empty())
            .ok_or_else(|| {
                crate::storage::StorageError::new(
                    store.path(),
                    "decode Xiaomi feature metadata",
                    "Feature metadata has an invalid name",
                )
            })?;
        let role = object
            .get("role")
            .and_then(serde_json::Value::as_str)
            .and_then(|role| role.parse::<FeatureRole>().ok())
            .ok_or_else(|| {
                crate::storage::StorageError::new(
                    store.path(),
                    "decode Xiaomi feature metadata",
                    "Feature metadata has an invalid role",
                )
            })?;
        metadata.push((siid, role, name.to_owned()));
    }
    features.retain(|feature| {
        metadata
            .iter()
            .any(|(siid, role, _)| feature.service_instance == *siid && feature.role == *role)
    });
    for (siid, role, name) in metadata {
        let feature = features
            .iter_mut()
            .find(|feature| feature.service_instance == siid && feature.role == role)
            .ok_or_else(|| {
                crate::storage::StorageError::new(
                    store.path(),
                    "restore Xiaomi feature metadata",
                    "Cached spec no longer provides a persisted feature",
                )
            })?;
        feature.name = name;
    }
    Ok(())
}

fn split_did(did: &str) -> (&str, Option<u32>) {
    if let Some((parent, suffix)) = did.rsplit_once(".s")
        && !parent.is_empty()
        && let Ok(siid) = suffix.parse()
    {
        return (parent, Some(siid));
    }
    (did, None)
}
