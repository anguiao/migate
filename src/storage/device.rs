use super::{AuthRevision, AuthSessionGeneration};
use super::{StorageError, Store};
use crate::device::{
    AccountId, DeviceDid, FeatureId, FeatureIdentity as DeviceFeatureIdentity, HomeId,
    PhysicalDeviceId, Property, PropertyValue, StateSource,
};
use rusqlite::{OptionalExtension as _, Transaction, params};
use std::path::Path;
use uuid::Uuid;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HomeBinding {
    pub account: AccountId,
    pub home: HomeId,
    pub display_name: String,
}

fn publish_definition(
    transaction: &Transaction<'_>,
    definition: &PublishedFeatureDefinition,
) -> rusqlite::Result<FeatureIdentity> {
    let identity = if let Some(found) = query_feature(transaction, &definition.feature)? {
        transaction.execute(
            "UPDATE feature_identities SET active=1
             WHERE account_uid=?1 AND home_id=?2 AND parent_did=?3
               AND service_instance=?4 AND role=?5",
            params![
                definition.feature.physical.account.as_str(),
                definition.feature.physical.home.as_str(),
                definition.feature.physical.parent_did.as_str(),
                definition.feature.service_instance,
                definition.feature.role.as_str()
            ],
        )?;
        FeatureIdentity {
            active: true,
            ..found
        }
    } else {
        let endpoint: i64 = transaction.query_row(
            "SELECT next_endpoint FROM feature_allocator WHERE id=1",
            [],
            |row| row.get(0),
        )?;
        if endpoint > u16::MAX as i64 - 1 {
            return Err(rusqlite::Error::IntegralValueOutOfRange(0, endpoint));
        }
        transaction.execute(
            "UPDATE feature_allocator SET next_endpoint=next_endpoint+1 WHERE id=1",
            [],
        )?;
        let public_id = Uuid::new_v4().simple().to_string();
        transaction.execute(
            "INSERT INTO feature_identities(
                account_uid,home_id,parent_did,service_instance,role,endpoint,public_id,active
             ) VALUES(?1,?2,?3,?4,?5,?6,?7,1)",
            params![
                definition.feature.physical.account.as_str(),
                definition.feature.physical.home.as_str(),
                definition.feature.physical.parent_did.as_str(),
                definition.feature.service_instance,
                definition.feature.role.as_str(),
                endpoint,
                public_id
            ],
        )?;
        FeatureIdentity {
            feature: definition.feature.clone(),
            endpoint: endpoint as u16,
            public_id: FeatureId::new(public_id).map_err(invalid)?,
            active: true,
        }
    };
    transaction.execute(
        "INSERT INTO published_feature_definitions(
            account_uid,home_id,parent_did,service_instance,role,model,spec_document,name
         ) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)
         ON CONFLICT(account_uid,home_id,parent_did,service_instance,role)
         DO UPDATE SET model=excluded.model,spec_document=excluded.spec_document,name=excluded.name",
        params![
            definition.feature.physical.account.as_str(),
            definition.feature.physical.home.as_str(),
            definition.feature.physical.parent_did.as_str(),
            definition.feature.service_instance,
            definition.feature.role.as_str(),
            definition.model,
            definition.spec_document,
            definition.name
        ],
    )?;
    Ok(identity)
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceRecord {
    pub identity: PhysicalDeviceId,
    pub model: String,
    pub name: String,
    pub room_id: Option<String>,
    pub admitted: bool,
}
#[derive(Clone, Eq, PartialEq)]
pub struct DeviceToken(pub Vec<u8>);
impl std::fmt::Debug for DeviceToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("DeviceToken([REDACTED])")
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CachedSpec {
    pub type_urn: String,
    pub document: String,
    pub fetched_at: i64,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FeatureIdentity {
    pub feature: DeviceFeatureIdentity,
    pub endpoint: u16,
    pub public_id: FeatureId,
    pub active: bool,
}
#[derive(Clone, Debug, PartialEq)]
pub struct PersistedState {
    pub feature: DeviceFeatureIdentity,
    pub property: Property,
    pub value: PropertyValue,
    pub source: StateSource,
    pub observed_at: i64,
    pub report_version: u64,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublishedFeatureDefinition {
    pub feature: DeviceFeatureIdentity,
    pub model: String,
    pub spec_document: String,
    pub name: String,
}
#[derive(Clone, Debug)]
pub struct PublishedTopologyDelta {
    pub account: AccountId,
    pub session_generation: AuthSessionGeneration,
    pub binding: Option<HomeBinding>,
    pub devices: Vec<DeviceRecord>,
    pub definitions: Vec<PublishedFeatureDefinition>,
    pub deactivate: Vec<DeviceFeatureIdentity>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogHomeRecord {
    pub home_id: String,
    pub name: String,
    pub group_id: String,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogRoomRecord {
    pub home_id: String,
    pub room_id: String,
    pub name: String,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogDeviceMetadata {
    pub spec_type: Option<String>,
    pub pid: Option<i64>,
    pub local_ip: Option<String>,
    pub parent_id: Option<String>,
    pub online: Option<bool>,
    pub feature_document: String,
}
#[derive(Clone)]
pub struct CatalogSnapshot {
    pub account: AccountId,
    pub homes: Vec<CatalogHomeRecord>,
    pub rooms: Vec<CatalogRoomRecord>,
    pub devices: Vec<(DeviceRecord, CatalogDeviceMetadata, Option<[u8; 16]>)>,
    pub specs: Vec<CachedSpec>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredCatalog {
    pub homes: Vec<CatalogHomeRecord>,
    pub rooms: Vec<CatalogRoomRecord>,
    pub devices: Vec<(DeviceRecord, CatalogDeviceMetadata)>,
}

#[derive(Clone)]
pub struct DeviceStore {
    store: Store,
}
impl DeviceStore {
    pub fn publish_topology(
        &self,
        delta: &PublishedTopologyDelta,
    ) -> Result<Option<Vec<FeatureIdentity>>, StorageError> {
        if delta
            .binding
            .as_ref()
            .is_some_and(|binding| binding.account != delta.account)
            || delta
                .devices
                .iter()
                .any(|device| device.identity.account != delta.account)
            || delta
                .deactivate
                .iter()
                .any(|feature| feature.physical.account != delta.account)
            || delta.binding.as_ref().is_some_and(|binding| {
                delta
                    .devices
                    .iter()
                    .any(|device| device.identity.home != binding.home)
                    || delta
                        .definitions
                        .iter()
                        .any(|definition| definition.feature.physical.home != binding.home)
                    || delta
                        .deactivate
                        .iter()
                        .any(|feature| feature.physical.home != binding.home)
            })
        {
            return Err(StorageError::new(
                self.path(),
                "validate topology delta",
                "Topology entry belongs to another account",
            ));
        }
        for device in &delta.devices {
            validate_texts(self.path(), [device.model.as_str(), device.name.as_str()])?;
        }
        for definition in &delta.definitions {
            validate_texts(
                self.path(),
                [definition.model.as_str(), definition.name.as_str()],
            )?;
            validate_json(
                self.path(),
                "validate published Xiaomi feature spec",
                &definition.spec_document,
            )?;
            if definition.feature.physical.account != delta.account {
                return Err(StorageError::new(
                    self.path(),
                    "validate topology delta",
                    "Feature belongs to another account",
                ));
            }
        }
        self.transaction_value("publish Xiaomi topology", |transaction| {
            let authorized = transaction.query_row(
                "SELECT generation=(?1) AND EXISTS(SELECT 1 FROM xiaomi_auth WHERE id=1 AND uid=?2)
                 FROM auth_session_generation WHERE id=1",
                params![delta.session_generation.get(), delta.account.as_str()],
                |row| row.get::<_, bool>(0),
            )?;
            if !authorized {
                return Ok(None);
            }
            if let Some(binding) = &delta.binding {
                let conflicting_home = transaction.query_row(
                    "SELECT EXISTS(
                        SELECT 1 FROM home_binding
                        WHERE id=1 AND account_uid=?1 AND home_id<>?2
                     )",
                    params![binding.account.as_str(), binding.home.as_str()],
                    |row| row.get::<_, bool>(0),
                )?;
                if conflicting_home {
                    return Ok(None);
                }
                transaction.execute(
                    "INSERT INTO home_binding(id,account_uid,home_id,display_name)
                     VALUES(1,?1,?2,?3)
                     ON CONFLICT(id) DO UPDATE SET
                        account_uid=excluded.account_uid,
                        home_id=excluded.home_id,
                        display_name=excluded.display_name",
                    params![
                        binding.account.as_str(),
                        binding.home.as_str(),
                        binding.display_name
                    ],
                )?;
            }
            for device in &delta.devices {
                upsert_device(transaction, device)?;
            }
            for feature in &delta.deactivate {
                transaction.execute(
                    "UPDATE feature_identities SET active=0
                     WHERE account_uid=?1 AND home_id=?2 AND parent_did=?3
                       AND service_instance=?4 AND role=?5",
                    params![
                        feature.physical.account.as_str(),
                        feature.physical.home.as_str(),
                        feature.physical.parent_did.as_str(),
                        feature.service_instance,
                        feature.role.as_str()
                    ],
                )?;
            }
            let mut published = Vec::with_capacity(delta.definitions.len());
            for definition in &delta.definitions {
                published.push(publish_definition(transaction, definition)?);
            }
            Ok(Some(published))
        })
    }
    pub(super) fn new(store: Store) -> Self {
        Self { store }
    }
    pub fn path(&self) -> &Path {
        self.store.path()
    }
    pub fn load_binding(&self) -> Result<Option<HomeBinding>, StorageError> {
        self.store
            .inner
            .connection
            .query_row(
                "SELECT account_uid,home_id,display_name FROM home_binding WHERE id=1",
                [],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(|e| self.db("load Xiaomi home binding", e))?
            .map(|(a, h, n)| {
                Ok(HomeBinding {
                    account: AccountId::new(a).map_err(invalid)?,
                    home: HomeId::new(h).map_err(invalid)?,
                    display_name: n,
                })
            })
            .transpose()
            .map_err(|e| self.db("validate Xiaomi home binding", e))
    }
    pub fn load_devices(&self) -> Result<Vec<DeviceRecord>, StorageError> {
        let mut statement = self
            .store
            .inner
            .connection
            .prepare(
                "SELECT account_uid,home_id,parent_did,model,name,room_id,admitted
                 FROM devices ORDER BY account_uid,home_id,parent_did",
            )
            .map_err(|error| self.db("load Xiaomi devices", error))?;
        let rows = statement
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, bool>(6)?,
                ))
            })
            .map_err(|e| self.db("load Xiaomi devices", e))?;
        rows.map(|row| {
            let (a, h, d, m, n, room, admitted) =
                row.map_err(|e| self.db("load Xiaomi devices", e))?;
            Ok(DeviceRecord {
                identity: physical(&a, &h, &d).map_err(|e| self.db("validate Xiaomi device", e))?,
                model: m,
                name: n,
                room_id: room,
                admitted,
            })
        })
        .collect()
    }
    pub fn replace_catalog_if_revision(
        &self,
        snapshot: &CatalogSnapshot,
        expected: AuthRevision,
    ) -> Result<bool, StorageError> {
        let home_ids = snapshot
            .homes
            .iter()
            .map(|home| home.home_id.as_str())
            .collect::<std::collections::HashSet<_>>();
        for home in &snapshot.homes {
            validate_texts(
                self.path(),
                [
                    home.home_id.as_str(),
                    home.name.as_str(),
                    home.group_id.as_str(),
                ],
            )?;
        }
        for room in &snapshot.rooms {
            validate_texts(self.path(), [room.home_id.as_str(), room.room_id.as_str()])?;
            validate_display_text(self.path(), &room.name)?;
            if !home_ids.contains(room.home_id.as_str()) {
                return Err(StorageError::new(
                    self.path(),
                    "validate Xiaomi catalog room",
                    "Room belongs to a home outside the snapshot",
                ));
            }
        }
        for (device, metadata, _) in &snapshot.devices {
            validate_texts(self.path(), [device.model.as_str(), device.name.as_str()])?;
            if device.identity.account != snapshot.account
                || !home_ids.contains(device.identity.home.as_str())
                || device.room_id.as_ref().is_some_and(|room_id| {
                    !snapshot.rooms.iter().any(|room| {
                        room.home_id == device.identity.home.as_str() && room.room_id == *room_id
                    })
                })
            {
                return Err(StorageError::new(
                    self.path(),
                    "validate Xiaomi catalog device",
                    "Device belongs outside the catalog snapshot",
                ));
            }
            if let Some(spec_type) = &metadata.spec_type {
                validate_texts(self.path(), [spec_type.as_str()])?;
            }
            validate_json(
                self.path(),
                "validate compiled feature metadata",
                &metadata.feature_document,
            )?;
        }
        for spec in &snapshot.specs {
            validate_json(self.path(), "validate MIoT spec cache", &spec.document)?;
        }
        self.transaction_value("replace complete Xiaomi catalog", |transaction| {
            if !revision_authorizes(transaction, expected, &snapshot.account)? {
                return Ok(false);
            }
            transaction.execute(
                "DELETE FROM catalog_rooms WHERE account_uid=?1",
                [snapshot.account.as_str()],
            )?;
            transaction.execute(
                "DELETE FROM catalog_homes WHERE account_uid=?1",
                [snapshot.account.as_str()],
            )?;
            for home in &snapshot.homes {
                transaction.execute(
                    "INSERT INTO catalog_homes(account_uid,home_id,name,group_id)
                     VALUES(?1,?2,?3,?4)",
                    params![
                        snapshot.account.as_str(),
                        home.home_id,
                        home.name,
                        home.group_id
                    ],
                )?;
            }
            for room in &snapshot.rooms {
                transaction.execute(
                    "INSERT INTO catalog_rooms(account_uid,home_id,room_id,name)
                     VALUES(?1,?2,?3,?4)",
                    params![
                        snapshot.account.as_str(),
                        room.home_id,
                        room.room_id,
                        room.name
                    ],
                )?;
            }
            let retained: std::collections::HashSet<_> = snapshot
                .devices
                .iter()
                .map(|(device, _, _)| {
                    (
                        device.identity.home.as_str(),
                        device.identity.parent_did.as_str(),
                    )
                })
                .collect();
            let mut statement = transaction
                .prepare("SELECT home_id,parent_did FROM devices WHERE account_uid=?1")?;
            let previous = statement
                .query_map([snapshot.account.as_str()], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(statement);
            for (home, did) in previous {
                if retained.contains(&(home.as_str(), did.as_str())) {
                    continue;
                }
                transaction.execute(
                    "UPDATE feature_identities SET active=0
                     WHERE account_uid=?1 AND home_id=?2 AND parent_did=?3",
                    params![snapshot.account.as_str(), home, did],
                )?;
                transaction.execute(
                    "DELETE FROM devices
                     WHERE account_uid=?1 AND home_id=?2 AND parent_did=?3",
                    params![snapshot.account.as_str(), home, did],
                )?;
            }
            for (device, metadata, token) in &snapshot.devices {
                upsert_catalog_device(transaction, device)?;
                transaction.execute(
                    "INSERT INTO catalog_device_metadata(
                        account_uid,home_id,parent_did,spec_type,pid,local_ip,
                        parent_id,online,feature_document
                     ) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)
                     ON CONFLICT(account_uid,home_id,parent_did) DO UPDATE SET
                        spec_type=excluded.spec_type,pid=excluded.pid,
                        local_ip=excluded.local_ip,parent_id=excluded.parent_id,
                        online=excluded.online,feature_document=excluded.feature_document",
                    params![
                        device.identity.account.as_str(),
                        device.identity.home.as_str(),
                        device.identity.parent_did.as_str(),
                        metadata.spec_type,
                        metadata.pid,
                        metadata.local_ip,
                        metadata.parent_id,
                        metadata.online,
                        metadata.feature_document
                    ],
                )?;
                if let Some(token) = token {
                    upsert_token(transaction, &device.identity, token)?;
                } else {
                    transaction.execute(
                        "DELETE FROM device_tokens
                         WHERE account_uid=?1 AND home_id=?2 AND parent_did=?3",
                        params![
                            device.identity.account.as_str(),
                            device.identity.home.as_str(),
                            device.identity.parent_did.as_str()
                        ],
                    )?;
                }
            }
            for spec in &snapshot.specs {
                transaction.execute(
                    "INSERT INTO miot_specs(type_urn,document,fetched_at)
                     VALUES(?1,?2,?3)
                     ON CONFLICT(type_urn) DO UPDATE SET
                        document=excluded.document,fetched_at=excluded.fetched_at",
                    params![spec.type_urn, spec.document, spec.fetched_at],
                )?;
            }
            Ok(true)
        })
    }
    pub fn load_catalog(&self, account: &AccountId) -> Result<StoredCatalog, StorageError> {
        let connection = &self.store.inner.connection;
        let mut home_statement = connection
            .prepare(
                "SELECT home_id,name,group_id FROM catalog_homes
                 WHERE account_uid=?1 ORDER BY home_id",
            )
            .map_err(|error| self.db("load Xiaomi catalog homes", error))?;
        let homes = home_statement
            .query_map([account.as_str()], |row| {
                Ok(CatalogHomeRecord {
                    home_id: row.get(0)?,
                    name: row.get(1)?,
                    group_id: row.get(2)?,
                })
            })
            .map_err(|error| self.db("load Xiaomi catalog homes", error))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|error| self.db("load Xiaomi catalog homes", error))?;
        let mut room_statement = connection
            .prepare(
                "SELECT home_id,room_id,name FROM catalog_rooms
                 WHERE account_uid=?1 ORDER BY home_id,room_id",
            )
            .map_err(|error| self.db("load Xiaomi catalog rooms", error))?;
        let rooms = room_statement
            .query_map([account.as_str()], |row| {
                Ok(CatalogRoomRecord {
                    home_id: row.get(0)?,
                    room_id: row.get(1)?,
                    name: row.get(2)?,
                })
            })
            .map_err(|error| self.db("load Xiaomi catalog rooms", error))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|error| self.db("load Xiaomi catalog rooms", error))?;
        let mut device_statement = connection
            .prepare(
                "SELECT d.home_id,d.parent_did,d.model,d.name,d.room_id,d.admitted,
                        m.spec_type,m.pid,m.local_ip,m.parent_id,m.online,m.feature_document
                 FROM devices d
                 JOIN catalog_device_metadata m USING(account_uid,home_id,parent_did)
                 WHERE d.account_uid=?1 ORDER BY d.home_id,d.parent_did",
            )
            .map_err(|error| self.db("load Xiaomi catalog devices", error))?;
        let devices = device_statement
            .query_map([account.as_str()], |row| {
                let home = row.get::<_, String>(0)?;
                let did = row.get::<_, String>(1)?;
                Ok((
                    DeviceRecord {
                        identity: physical(account.as_str(), &home, &did).map_err(invalid)?,
                        model: row.get(2)?,
                        name: row.get(3)?,
                        room_id: row.get(4)?,
                        admitted: row.get(5)?,
                    },
                    CatalogDeviceMetadata {
                        spec_type: row.get(6)?,
                        pid: row.get(7)?,
                        local_ip: row.get(8)?,
                        parent_id: row.get(9)?,
                        online: row.get(10)?,
                        feature_document: row.get(11)?,
                    },
                ))
            })
            .map_err(|error| self.db("load Xiaomi catalog devices", error))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|error| self.db("load Xiaomi catalog devices", error))?;
        Ok(StoredCatalog {
            homes,
            rooms,
            devices,
        })
    }
    pub fn load_token(&self, id: &PhysicalDeviceId) -> Result<Option<Vec<u8>>, StorageError> {
        self.store
            .inner
            .connection
            .query_row(
                "SELECT token FROM device_tokens
                 WHERE account_uid=?1 AND home_id=?2 AND parent_did=?3",
                params![
                    id.account.as_str(),
                    id.home.as_str(),
                    id.parent_did.as_str()
                ],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| self.db("load Xiaomi device token", error))
    }
    pub fn load_spec(&self, key: &str) -> Result<Option<CachedSpec>, StorageError> {
        let cached = self
            .store
            .inner
            .connection
            .query_row(
                "SELECT type_urn,document,fetched_at FROM miot_specs WHERE type_urn=?1",
                [key],
                |r| {
                    Ok(CachedSpec {
                        type_urn: r.get(0)?,
                        document: r.get(1)?,
                        fetched_at: r.get(2)?,
                    })
                },
            )
            .optional()
            .map_err(|error| self.db("load MIoT spec", error))?;
        if let Some(spec) = &cached {
            validate_json(self.path(), "validate MIoT spec cache", &spec.document)?;
        }
        Ok(cached)
    }
    pub fn allocate_feature(
        &self,
        f: &DeviceFeatureIdentity,
    ) -> Result<FeatureIdentity, StorageError> {
        self.transaction_value("allocate feature identity", |transaction| {
            if let Some(found) = query_feature(transaction, f)? {
                transaction.execute(
                    "UPDATE feature_identities SET active=1
                     WHERE account_uid=?1 AND home_id=?2 AND parent_did=?3
                       AND service_instance=?4 AND role=?5",
                    params![
                        f.physical.account.as_str(),
                        f.physical.home.as_str(),
                        f.physical.parent_did.as_str(),
                        f.service_instance,
                        f.role.as_str()
                    ],
                )?;
                return Ok(FeatureIdentity {
                    active: true,
                    ..found
                });
            }
            let endpoint: i64 = transaction.query_row(
                "SELECT next_endpoint FROM feature_allocator WHERE id=1",
                [],
                |row| row.get(0),
            )?;
            if endpoint > u16::MAX as i64 - 1 {
                return Err(rusqlite::Error::IntegralValueOutOfRange(0, endpoint));
            }
            transaction.execute(
                "UPDATE feature_allocator SET next_endpoint=next_endpoint+1 WHERE id=1",
                [],
            )?;
            let public_id = Uuid::new_v4().simple().to_string();
            transaction.execute(
                "INSERT INTO feature_identities(
                    account_uid,home_id,parent_did,service_instance,role,endpoint,public_id,active
                 ) VALUES(?1,?2,?3,?4,?5,?6,?7,1)",
                params![
                    f.physical.account.as_str(),
                    f.physical.home.as_str(),
                    f.physical.parent_did.as_str(),
                    f.service_instance,
                    f.role.as_str(),
                    endpoint,
                    public_id
                ],
            )?;
            Ok(FeatureIdentity {
                feature: f.clone(),
                endpoint: endpoint as u16,
                public_id: FeatureId::new(public_id).map_err(invalid)?,
                active: true,
            })
        })
    }
    pub fn load_active_published_feature_definitions(
        &self,
    ) -> Result<Vec<PublishedFeatureDefinition>, StorageError> {
        self.query_published_feature_definitions(
            "SELECT definitions.account_uid,definitions.home_id,definitions.parent_did,
                definitions.service_instance,definitions.role,definitions.model,
                definitions.spec_document,definitions.name
             FROM feature_identities AS identities
             LEFT JOIN published_feature_definitions AS definitions USING(
                account_uid,home_id,parent_did,service_instance,role
             )
             WHERE identities.active=1
             ORDER BY identities.account_uid,identities.home_id,
                identities.parent_did,identities.service_instance,identities.role",
        )
    }

    fn query_published_feature_definitions(
        &self,
        query: &str,
    ) -> Result<Vec<PublishedFeatureDefinition>, StorageError> {
        let mut statement = self
            .store
            .inner
            .connection
            .prepare(query)
            .map_err(|error| self.db("load published Xiaomi features", error))?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, u32>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                ))
            })
            .map_err(|error| self.db("load published Xiaomi features", error))?;
        rows.map(|row| {
            let (account, home, did, service_instance, role, model, spec_document, name) =
                row.map_err(|error| self.db("load published Xiaomi features", error))?;
            validate_json(
                self.path(),
                "validate published Xiaomi feature spec",
                &spec_document,
            )?;
            Ok(PublishedFeatureDefinition {
                feature: DeviceFeatureIdentity {
                    physical: physical(&account, &home, &did)
                        .map_err(|error| self.db("validate published Xiaomi feature", error))?,
                    service_instance,
                    role: role.parse().map_err(|error| {
                        StorageError::new(
                            self.path(),
                            "validate published Xiaomi feature role",
                            error,
                        )
                    })?,
                },
                model,
                spec_document,
                name,
            })
        })
        .collect()
    }
    pub fn load_features(&self, active_only: bool) -> Result<Vec<FeatureIdentity>, StorageError> {
        let query = if active_only {
            "SELECT account_uid,home_id,parent_did,service_instance,role,
                    endpoint,public_id,active
             FROM feature_identities WHERE active=1 ORDER BY endpoint"
        } else {
            "SELECT account_uid,home_id,parent_did,service_instance,role,
                    endpoint,public_id,active
             FROM feature_identities ORDER BY endpoint"
        };
        let mut stmt = self
            .store
            .inner
            .connection
            .prepare(query)
            .map_err(|e| self.db("load feature identities", e))?;
        let rows = stmt
            .query_map([], parse_feature_row)
            .map_err(|e| self.db("load feature identities", e))?;
        rows.map(|r| r.map_err(|e| self.db("load feature identities", e)))
            .collect()
    }
    pub fn save_states(&self, states: &[PersistedState]) -> Result<(), StorageError> {
        let mut encoded = Vec::with_capacity(states.len());
        for state in states {
            validate_persisted_value(self.path(), state.property, &state.value)?;
            let version = i64::try_from(state.report_version)
                .map_err(|error| StorageError::new(self.path(), "validate feature state", error))?;
            let value = serde_json::to_string(&state.value)
                .map_err(|error| StorageError::new(self.path(), "encode feature state", error))?;
            encoded.push((state, property_name(state.property), value, version));
        }
        self.transaction("save feature state", |transaction| {
            for (state, property, value, version) in &encoded {
                transaction.execute(
                    "INSERT INTO feature_states(
                    account_uid,home_id,parent_did,service_instance,role,property,
                    value,source,observed_at,report_version
                 ) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
                 ON CONFLICT(account_uid,home_id,parent_did,service_instance,role,property)
                 DO UPDATE SET value=excluded.value,source=excluded.source,
                    observed_at=excluded.observed_at,report_version=excluded.report_version
                 WHERE excluded.report_version>=feature_states.report_version",
                    params![
                        state.feature.physical.account.as_str(),
                        state.feature.physical.home.as_str(),
                        state.feature.physical.parent_did.as_str(),
                        state.feature.service_instance,
                        state.feature.role.as_str(),
                        property,
                        value,
                        source_name(state.source),
                        state.observed_at,
                        version
                    ],
                )?;
            }
            Ok(())
        })
    }
    pub fn load_states(&self) -> Result<Vec<PersistedState>, StorageError> {
        let mut statement = self
            .store
            .inner
            .connection
            .prepare(
                "SELECT account_uid,home_id,parent_did,service_instance,role,property,
                    value,source,observed_at,report_version
             FROM feature_states
             ORDER BY account_uid,home_id,parent_did,service_instance,role,property",
            )
            .map_err(|error| self.db("load feature states", error))?;
        let rows = statement
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, u32>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, String>(6)?,
                    r.get::<_, String>(7)?,
                    r.get::<_, i64>(8)?,
                    r.get::<_, i64>(9)?,
                ))
            })
            .map_err(|e| self.db("load feature states", e))?;
        rows.map(|row| {
            let (a, h, d, si, role, property, value, source, at, version) =
                row.map_err(|e| self.db("load feature states", e))?;
            let state = PersistedState {
                feature: DeviceFeatureIdentity {
                    physical: physical(&a, &h, &d)
                        .map_err(|e| self.db("validate feature state", e))?,
                    service_instance: si,
                    role: role
                        .parse()
                        .map_err(|e| self.db("validate feature state", invalid(e)))?,
                },
                property: parse_property(&property)
                    .map_err(|e| self.db("validate feature state", e))?,
                value: serde_json::from_str(&value)
                    .map_err(|e| self.db("validate feature state", invalid(e)))?,
                source: parse_source(&source).map_err(|e| self.db("validate feature state", e))?,
                observed_at: at,
                report_version: u64::try_from(version)
                    .map_err(|e| self.db("validate feature state", invalid(e)))?,
            };
            validate_persisted_value(self.path(), state.property, &state.value)?;
            Ok(state)
        })
        .collect()
    }
    fn transaction(
        &self,
        op: &'static str,
        body: impl FnOnce(&rusqlite::Transaction<'_>) -> rusqlite::Result<()>,
    ) -> Result<(), StorageError> {
        self.transaction_value(op, body)
    }
    fn transaction_value<T>(
        &self,
        op: &'static str,
        body: impl FnOnce(&rusqlite::Transaction<'_>) -> rusqlite::Result<T>,
    ) -> Result<T, StorageError> {
        let tx = self
            .store
            .inner
            .connection
            .unchecked_transaction()
            .map_err(|e| self.db(op, e))?;
        let value = body(&tx).map_err(|e| self.db(op, e))?;
        tx.commit().map_err(|e| self.db(op, e))?;
        Ok(value)
    }
    fn db(&self, op: impl Into<String>, e: rusqlite::Error) -> StorageError {
        self.store.database_error(op, e)
    }
}

fn query_feature(
    connection: &rusqlite::Connection,
    feature: &DeviceFeatureIdentity,
) -> rusqlite::Result<Option<FeatureIdentity>> {
    connection
        .query_row(
            "SELECT account_uid,home_id,parent_did,service_instance,role,
                    endpoint,public_id,active
             FROM feature_identities
             WHERE account_uid=?1 AND home_id=?2 AND parent_did=?3
               AND service_instance=?4 AND role=?5",
            params![
                feature.physical.account.as_str(),
                feature.physical.home.as_str(),
                feature.physical.parent_did.as_str(),
                feature.service_instance,
                feature.role.as_str()
            ],
            parse_feature_row,
        )
        .optional()
}

fn upsert_device(connection: &rusqlite::Connection, device: &DeviceRecord) -> rusqlite::Result<()> {
    connection.execute(
        "INSERT INTO devices(account_uid,home_id,parent_did,model,name,room_id,admitted)
         VALUES(?1,?2,?3,?4,?5,?6,?7)
         ON CONFLICT(account_uid,home_id,parent_did) DO UPDATE SET
            model=excluded.model,name=excluded.name,room_id=excluded.room_id,
            admitted=excluded.admitted",
        params![
            device.identity.account.as_str(),
            device.identity.home.as_str(),
            device.identity.parent_did.as_str(),
            device.model,
            device.name,
            device.room_id,
            device.admitted
        ],
    )?;
    Ok(())
}

fn upsert_catalog_device(
    connection: &rusqlite::Connection,
    device: &DeviceRecord,
) -> rusqlite::Result<()> {
    connection.execute(
        "INSERT INTO devices(account_uid,home_id,parent_did,model,name,room_id,admitted)
         VALUES(?1,?2,?3,?4,?5,?6,?7)
         ON CONFLICT(account_uid,home_id,parent_did) DO UPDATE SET
            model=excluded.model,name=excluded.name,room_id=excluded.room_id",
        params![
            device.identity.account.as_str(),
            device.identity.home.as_str(),
            device.identity.parent_did.as_str(),
            device.model,
            device.name,
            device.room_id,
            device.admitted
        ],
    )?;
    Ok(())
}

fn upsert_token(
    connection: &rusqlite::Connection,
    identity: &PhysicalDeviceId,
    token: &[u8],
) -> rusqlite::Result<()> {
    connection.execute(
        "INSERT INTO device_tokens(account_uid,home_id,parent_did,token)
         VALUES(?1,?2,?3,?4)
         ON CONFLICT(account_uid,home_id,parent_did) DO UPDATE SET token=excluded.token",
        params![
            identity.account.as_str(),
            identity.home.as_str(),
            identity.parent_did.as_str(),
            token
        ],
    )?;
    Ok(())
}
fn revision_authorizes(
    connection: &rusqlite::Connection,
    expected: AuthRevision,
    account: &AccountId,
) -> rusqlite::Result<bool> {
    connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM auth_revision JOIN xiaomi_auth ON xiaomi_auth.id=1
            WHERE auth_revision.id=1 AND auth_revision.revision=?1 AND xiaomi_auth.uid=?2
         )",
        params![expected.get(), account.as_str()],
        |row| row.get(0),
    )
}
fn parse_feature_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<FeatureIdentity> {
    let endpoint = r.get::<_, u16>(5)?;
    let a = r.get::<_, String>(0)?;
    let h = r.get::<_, String>(1)?;
    let d = r.get::<_, String>(2)?;
    Ok(FeatureIdentity {
        feature: DeviceFeatureIdentity {
            physical: physical(&a, &h, &d)?,
            service_instance: r.get(3)?,
            role: r.get::<_, String>(4)?.parse().map_err(invalid)?,
        },
        endpoint,
        public_id: FeatureId::new(r.get::<_, String>(6)?).map_err(invalid)?,
        active: r.get(7)?,
    })
}
fn physical(a: &str, h: &str, d: &str) -> rusqlite::Result<PhysicalDeviceId> {
    Ok(PhysicalDeviceId {
        account: AccountId::new(a).map_err(invalid)?,
        home: HomeId::new(h).map_err(invalid)?,
        parent_did: DeviceDid::new(d).map_err(invalid)?,
    })
}
fn invalid<E: std::error::Error + Send + Sync + 'static>(e: E) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(Box::new(e))
}
fn validate_texts<'a>(
    path: &Path,
    values: impl IntoIterator<Item = &'a str>,
) -> Result<(), StorageError> {
    for value in values {
        if value.is_empty() || value.chars().any(char::is_control) {
            return Err(StorageError::new(
                path,
                "validate device data",
                "Text value is invalid",
            ));
        }
    }
    Ok(())
}

fn validate_display_text(path: &Path, value: &str) -> Result<(), StorageError> {
    if value.chars().any(char::is_control) {
        return Err(StorageError::new(
            path,
            "validate device data",
            "Text value is invalid",
        ));
    }
    Ok(())
}

fn validate_json(path: &Path, operation: &'static str, document: &str) -> Result<(), StorageError> {
    serde_json::from_str::<serde_json::Value>(document)
        .map(|_| ())
        .map_err(|error| StorageError::new(path, operation, error))
}
fn validate_persisted_value(
    path: &Path,
    property: Property,
    value: &PropertyValue,
) -> Result<(), StorageError> {
    use Property::*;
    let valid = match (property, value) {
        (
            Brightness | CurtainPosition | CurtainTargetPosition | Humidity | Battery,
            PropertyValue::Percent(value),
        ) => value.get().is_finite() && (0.0..=100.0).contains(&value.get()),
        (Power, PropertyValue::Power(_))
        | (Color, PropertyValue::Color(_))
        | (HvacMode, PropertyValue::HvacMode(_))
        | (FanSpeed, PropertyValue::FanSpeed(_))
        | (SwingMode, PropertyValue::SwingMode(_))
        | (CurtainMovement, PropertyValue::CurtainMovement(_))
        | (Oscillation, PropertyValue::Oscillation(_))
        | (Motion, PropertyValue::Motion(_))
        | (Occupancy, PropertyValue::Occupancy(_))
        | (Contact, PropertyValue::ContactOpen(_))
        | (VacuumCleanMode, PropertyValue::VacuumCleanMode(_))
        | (VacuumOperationalState, PropertyValue::VacuumOperationalState(_)) => true,
        (
            CurrentTemperature | TargetTemperature | Temperature,
            PropertyValue::Temperature(number),
        ) => number.is_finite() && *number >= -273.15,
        (ColorTemperature, PropertyValue::ColorTemperature(value)) => *value > 0,
        (Illuminance, PropertyValue::Illuminance(number)) => number.is_finite() && *number >= 0.0,
        (VacuumFault, PropertyValue::VacuumFault(fault)) => {
            !fault.is_empty() && !fault.chars().any(char::is_control)
        }
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(StorageError::new(
            path,
            "validate feature state",
            "Property value does not match its property",
        ))
    }
}
fn property_name(p: Property) -> &'static str {
    match p {
        Property::Power => "power",
        Property::Brightness => "brightness",
        Property::ColorTemperature => "color-temperature",
        Property::Color => "color",
        Property::CurrentTemperature => "current-temperature",
        Property::TargetTemperature => "target-temperature",
        Property::HvacMode => "hvac-mode",
        Property::FanSpeed => "fan-speed",
        Property::SwingMode => "swing-mode",
        Property::CurtainPosition => "curtain-position",
        Property::CurtainTargetPosition => "curtain-target-position",
        Property::CurtainMovement => "curtain-movement",
        Property::Oscillation => "oscillation",
        Property::Temperature => "temperature",
        Property::Humidity => "humidity",
        Property::Illuminance => "illuminance",
        Property::Motion => "motion",
        Property::Occupancy => "occupancy",
        Property::Contact => "contact",
        Property::Battery => "battery",
        Property::VacuumCleanMode => "vacuum-clean-mode",
        Property::VacuumOperationalState => "vacuum-operational-state",
        Property::VacuumFault => "vacuum-fault",
    }
}
fn parse_property(s: &str) -> rusqlite::Result<Property> {
    use Property::*;
    Ok(match s {
        "power" => Power,
        "brightness" => Brightness,
        "color-temperature" => ColorTemperature,
        "color" => Color,
        "current-temperature" => CurrentTemperature,
        "target-temperature" => TargetTemperature,
        "hvac-mode" => HvacMode,
        "fan-speed" => FanSpeed,
        "swing-mode" => SwingMode,
        "curtain-position" => CurtainPosition,
        "curtain-target-position" => CurtainTargetPosition,
        "curtain-movement" => CurtainMovement,
        "oscillation" => Oscillation,
        "temperature" => Temperature,
        "humidity" => Humidity,
        "illuminance" => Illuminance,
        "motion" => Motion,
        "occupancy" => Occupancy,
        "contact" => Contact,
        "battery" => Battery,
        "vacuum-clean-mode" => VacuumCleanMode,
        "vacuum-operational-state" => VacuumOperationalState,
        "vacuum-fault" => VacuumFault,
        _ => return Err(invalid_value()),
    })
}
fn source_name(s: StateSource) -> &'static str {
    match s {
        StateSource::Gateway => "gateway",
        StateSource::Lan => "lan",
        StateSource::Cloud => "cloud",
        StateSource::Cache => "cache",
    }
}
fn parse_source(s: &str) -> rusqlite::Result<StateSource> {
    match s {
        "gateway" => Ok(StateSource::Gateway),
        "lan" => Ok(StateSource::Lan),
        "cloud" => Ok(StateSource::Cloud),
        "cache" => Ok(StateSource::Cache),
        _ => Err(invalid_value()),
    }
}
fn invalid_value() -> rusqlite::Error {
    rusqlite::Error::InvalidQuery
}
