//! One committed configuration, shared by runtime and the terminal.

use crate::{
    device::{DeviceKey, DeviceRegistry, PhysicalDevice},
    storage::{
        PublicationCommit, StorageError, Store, StoredDevice, StoredFunction, StoredRelation,
    },
    xiaomi::adapter::publication::{SourceHandle, SourceSnapshot},
};
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet},
    rc::Rc,
};

pub(crate) struct EffectiveConfiguration {
    pub(crate) source: SourceSnapshot,
    pub(crate) records: Vec<StoredDevice>,
    pub(crate) published: DeviceRegistry,
    pub(crate) root_version: u32,
}

impl EffectiveConfiguration {
    pub(crate) fn devices(&self) -> Vec<PhysicalDevice> {
        self.source
            .entries
            .iter()
            .map(|entry| entry.physical.clone())
            .collect()
    }

    pub(crate) fn registry(&self) -> DeviceRegistry {
        let registry = DeviceRegistry::new();
        for device in self.devices() {
            registry.insert(device);
        }
        registry
    }
}

pub(crate) struct Prepared {
    source: SourceSnapshot,
    commit: PublicationCommit,
    pub(crate) rebuild: bool,
}

pub(crate) struct Publication {
    source: SourceHandle,
    store: Store,
    current: RefCell<Rc<EffectiveConfiguration>>,
    failure: RefCell<Option<String>>,
    pending: Cell<bool>,
}

impl Publication {
    pub(crate) fn open(source: SourceHandle, store: Store) -> Result<Self, StorageError> {
        let (snapshot, saved) = source.committed()?;
        let published = DeviceRegistry::new();
        for entry in &snapshot.entries {
            if saved
                .records
                .iter()
                .any(|record| record.key == *entry.physical.key() && record.published)
            {
                published.insert(entry.physical.clone());
            }
        }
        Ok(Self {
            source,
            store,
            current: RefCell::new(Rc::new(EffectiveConfiguration {
                source: snapshot,
                records: saved.records,
                published,
                root_version: saved.root_version,
            })),
            failure: RefCell::new(None),
            pending: Cell::new(false),
        })
    }

    pub(crate) fn current(&self) -> Rc<EffectiveConfiguration> {
        self.current.borrow().clone()
    }

    pub(crate) fn failure(&self) -> Option<String> {
        self.failure.borrow().clone()
    }

    /// Record a non-fatal prepare/save failure for the terminal; the committed
    /// configuration and runner stay untouched.
    pub(crate) fn fail(&self, reason: String) {
        *self.failure.borrow_mut() = Some(reason);
        self.pending.set(true);
    }

    pub(crate) fn pending(&self) -> bool {
        self.pending.get()
    }

    pub(crate) fn listen(&self) -> event_listener::EventListener {
        self.source.listen()
    }

    pub(crate) fn stop_actions(&self) {
        self.source.stop_actions();
    }

    /// Reacquire this only after the old Matter runner has been dropped when
    /// a rebuild is needed. No network or database mutation occurs here.
    pub(crate) fn prepare(&self) -> Result<Option<Prepared>, StorageError> {
        let snapshot = self.source.snapshot();
        let Some(scope) = snapshot.scope.clone() else {
            return Ok(None);
        };
        let saved = self.store.devices().load_publication(Some(&scope))?;
        let current = self.current();
        let group_keys = snapshot
            .relations
            .iter()
            .map(|relation| DeviceKey::new("xiaomi", &relation.group_did))
            .collect::<BTreeSet<_>>();
        let groups = snapshot
            .relations
            .iter()
            .filter(|relation| {
                relation.active
                    && snapshot.entries.iter().any(|entry| {
                        entry.physical.key().id == relation.group_did && entry.problem.is_none()
                    })
            })
            .collect::<Vec<_>>();
        let hidden = groups
            .iter()
            .flat_map(|group| {
                group
                    .members
                    .iter()
                    .map(|did| DeviceKey::new("xiaomi", did))
            })
            .collect::<BTreeSet<_>>();
        let mut records = saved
            .records
            .iter()
            .cloned()
            .map(|mut record| {
                record.published = false;
                (record.key.clone(), record)
            })
            .collect::<BTreeMap<_, _>>();
        let mut next_endpoint = self.store.devices().next_endpoint()?;
        let mut clear_revocations = Vec::new();
        for entry in &snapshot.entries {
            let key = entry.physical.key();
            let revoked = saved
                .revocations
                .iter()
                .any(|revocation| revocation.key == *key);
            if revoked && !entry.fresh {
                continue;
            }
            if revoked {
                clear_revocations.push(key.clone());
            }
            let publish = if group_keys.contains(key) {
                groups.iter().any(|group| group.group_did == key.id)
            } else {
                !hidden.contains(key)
            };
            let description = entry.physical.description();
            let mut record = match records.remove(key) {
                Some(mut existing) => {
                    let fixed_changed = !crate::matter::same_fixed_description(
                        &existing.description(),
                        &description,
                    );
                    if fixed_changed {
                        existing.configuration_version = existing
                            .configuration_version
                            .checked_add(1)
                            .ok_or_else(|| {
                                self.invalid("Endpoint configuration version is exhausted")
                            })?;
                    }
                    for function in &mut existing.functions {
                        let actual = description
                            .functions
                            .iter()
                            .find(|actual| actual.id == function.key.id)
                            .ok_or_else(|| {
                                self.invalid("Stored device topology is incompatible")
                            })?;
                        function.category = actual.category;
                        function.capabilities = actual.capabilities.clone();
                    }
                    existing
                }
                None => {
                    let needed = if description.functions.len() == 1 {
                        1
                    } else {
                        description.functions.len() + 1
                    };
                    if usize::from(next_endpoint) + needed - 1 > 65534 {
                        self.source.pending(
                            key,
                            crate::storage::AdmissionError::EndpointExhausted.to_string(),
                        );
                        self.pending.set(true);
                        return Ok(None);
                    }
                    let endpoint = allocate(&mut next_endpoint)
                        .ok_or_else(|| self.invalid("Device endpoint numbers are exhausted"))?;
                    let single = description.functions.len() == 1;
                    let mut functions = Vec::new();
                    for function in &description.functions {
                        functions.push(StoredFunction {
                            key: crate::device::FunctionKey::new(key.clone(), &function.id),
                            endpoint: if single {
                                endpoint
                            } else {
                                allocate(&mut next_endpoint).ok_or_else(|| {
                                    self.invalid("Device endpoint numbers are exhausted")
                                })?
                            },
                            label: function.label.clone(),
                            category: function.category,
                            capabilities: function.capabilities.clone(),
                        });
                    }
                    StoredDevice {
                        key: key.clone(),
                        unique_id: uuid::Uuid::new_v4().simple().to_string(),
                        endpoint,
                        label: entry.physical.label(),
                        functions,
                        published: publish,
                        configuration_version: 1,
                    }
                }
            };
            record.published = publish;
            records.insert(key.clone(), record);
        }
        let records = records.into_values().collect::<Vec<_>>();
        let fixed_changed = !published_equal(&saved.records, &records);
        let root_version = if fixed_changed {
            saved
                .root_version
                .checked_add(1)
                .ok_or_else(|| self.invalid("Root configuration version is exhausted"))?
        } else {
            saved.root_version
        };
        let refs_changed = current.source.entries.len() != snapshot.entries.len()
            || current
                .source
                .entries
                .iter()
                .zip(&snapshot.entries)
                .any(|(old, new)| !old.physical.same_instance(&new.physical));
        let relevant = current
            .source
            .relations
            .iter()
            .filter(|relation| {
                current
                    .published
                    .get(&DeviceKey::new("xiaomi", &relation.group_did))
                    .is_some()
            })
            .map(|relation| relation.group_did.as_str())
            .chain(groups.iter().map(|relation| relation.group_did.as_str()))
            .collect::<BTreeSet<_>>();
        let relation_changed =
            !relations_equal(&current.source.relations, &snapshot.relations, &relevant);
        let bindings_changed = current.source.entries.len() != snapshot.entries.len()
            || current
                .source
                .entries
                .iter()
                .zip(&snapshot.entries)
                .any(|(old, new)| old.cached != new.cached);
        // A restored repaired group already has its ExtendedColor Physical
        // instance but no installed plan; the reconstructed plan must be
        // installed before Matter serves without a version bump or rebuild.
        let repair_changed = current.source.entries.len() != snapshot.entries.len()
            || current
                .source
                .entries
                .iter()
                .zip(&snapshot.entries)
                .any(|(old, new)| {
                    old.repair != new.repair || old.repair_reason != new.repair_reason
                });
        let metadata_changed = current.source.relations != snapshot.relations
            || current.source.scope != snapshot.scope;
        if !fixed_changed
            && !refs_changed
            && !relation_changed
            && !bindings_changed
            && !metadata_changed
            && !repair_changed
            && clear_revocations.is_empty()
        {
            self.pending.set(false);
            return Ok(None);
        }
        self.pending.set(true);
        Ok(Some(Prepared {
            commit: PublicationCommit {
                scope,
                cached: snapshot
                    .entries
                    .iter()
                    .map(|entry| entry.cached.clone())
                    .collect(),
                records,
                relations: snapshot.relations.clone(),
                root_version,
                clear_revocations,
            },
            rebuild: fixed_changed
                || refs_changed
                || relation_changed
                || current.source.scope != snapshot.scope,
            source: snapshot,
        }))
    }

    pub(crate) fn apply(&self, prepared: Prepared) -> Result<(), StorageError> {
        if let Err(error) = self.store.devices().apply_publication(&prepared.commit) {
            *self.failure.borrow_mut() = Some(error.to_string());
            return Err(error);
        }
        self.source.install(&prepared.source);
        let published = DeviceRegistry::new();
        for entry in &prepared.source.entries {
            let record = prepared
                .commit
                .records
                .iter()
                .find(|record| record.key == *entry.physical.key())
                .unwrap();
            self.source.set_label(&record.key, &record.label);
            if record.published {
                published.insert(entry.physical.clone());
            }
        }
        *self.current.borrow_mut() = Rc::new(EffectiveConfiguration {
            source: prepared.source,
            records: prepared.commit.records,
            root_version: prepared.commit.root_version,
            published,
        });
        self.failure.borrow_mut().take();
        self.pending.set(false);
        log::info!(
            "Bridge configuration committed at root version {}",
            self.current().root_version
        );
        Ok(())
    }

    fn invalid(&self, reason: &'static str) -> StorageError {
        StorageError::new(self.store.path(), "prepare bridge configuration", reason)
    }
}

fn allocate(next: &mut u16) -> Option<u16> {
    let endpoint = *next;
    if endpoint > 65534 {
        return None;
    }
    *next += 1;
    Some(endpoint)
}

fn published_equal(old: &[StoredDevice], new: &[StoredDevice]) -> bool {
    let before = old
        .iter()
        .filter(|record| record.published)
        .collect::<Vec<_>>();
    let after = new
        .iter()
        .filter(|record| record.published)
        .collect::<Vec<_>>();
    before.len() == after.len()
        && before.iter().all(|old| {
            after.iter().any(|new| {
                old.key == new.key
                    && old.endpoint == new.endpoint
                    && crate::matter::same_fixed_description(&old.description(), &new.description())
            })
        })
}

fn relations_equal(
    old: &[StoredRelation],
    new: &[StoredRelation],
    relevant: &BTreeSet<&str>,
) -> bool {
    let old = old
        .iter()
        .filter(|relation| relevant.contains(relation.group_did.as_str()))
        .collect::<Vec<_>>();
    let new = new
        .iter()
        .filter(|relation| relevant.contains(relation.group_did.as_str()))
        .collect::<Vec<_>>();
    old.len() == new.len()
        && old.iter().all(|old| {
            new.iter().any(|new| {
                old.group_did == new.group_did
                    && old.members == new.members
                    && old.is_physical == new.is_physical
                    && old.active == new.active
            })
        })
}
