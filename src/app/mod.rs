//! Application assembly: one committed configuration drives every runtime view.
//!
//! The application owns the latest source handle, the effective configuration
//! and the one-time activation resources between planning and installation. It
//! never exposes configuration submission or source slots to read-only views.

mod lifecycle;
mod shutdown;

pub use lifecycle::run_xiaomi_bridge;
pub use shutdown::shutdown_signal;

use crate::{
    configuration::{
        self, CommittedConfiguration, DerivedConfiguration, Plan, PlanInput, validate_label,
    },
    device::{DeviceKey, DeviceRegistry, PhysicalDevice},
    matter::{self, FixedShape, LabelWriter},
    storage::{Identity, StorageError, Store},
    terminal::AuthStatus,
    xiaomi::adapter::publication::{PreparedSources, SourceEntry, SourceHandle},
};
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet},
    rc::Rc,
};

pub struct BridgeOptions<'a> {
    pub store: &'a Store,
    pub identity: &'a Identity,
    pub status: &'a AuthStatus,
    pub port: u16,
}

/// One committed configuration with its derived graph and serving devices.
pub(crate) struct EffectiveConfiguration {
    pub(crate) configuration: CommittedConfiguration,
    pub(crate) derived: Rc<DerivedConfiguration>,
    pub(crate) entries: Vec<Rc<SourceEntry>>,
    pub(crate) published: DeviceRegistry,
}

impl EffectiveConfiguration {
    pub(crate) fn devices(&self) -> Vec<PhysicalDevice> {
        self.entries
            .iter()
            .map(|entry| entry.physical.clone())
            .collect()
    }
}

/// A complete, not yet committed plan together with its owned activation
/// resources. The plan contains no running object; the resources are consumed
/// exactly once by `apply`.
pub(crate) struct Prepared {
    plan: Plan,
    resources: PreparedSources,
}

/// The single application configuration owner.
pub(crate) struct App {
    source: SourceHandle,
    store: Store,
    current: RefCell<Rc<EffectiveConfiguration>>,
    failure: RefCell<Option<String>>,
    pending: Cell<bool>,
}

impl App {
    pub(crate) fn open(source: SourceHandle, store: Store) -> Result<Self, StorageError> {
        let configuration = store.devices().load_configuration()?;
        // The committed derived graph is never overlaid with the recovery
        // state: planning must still see revoked devices as previously
        // published so they can be withdrawn with a real version bump. The
        // initial serving view is the intersection with the restored entries;
        // revocation and account switches legitimately lack old references.
        let derived = Rc::new(configuration.derive());
        let entries = source.entries(&configuration);
        let published = serving_registry(&derived.published, &entries);
        Ok(Self {
            source,
            store,
            current: RefCell::new(Rc::new(EffectiveConfiguration {
                configuration,
                derived,
                entries,
                published,
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

    pub(crate) fn pending(&self) -> bool {
        self.pending.get()
    }

    /// Record a non-fatal prepare/save failure for the terminal; the committed
    /// configuration and runner stay untouched.
    pub(crate) fn fail(&self, reason: String) {
        *self.failure.borrow_mut() = Some(reason);
        self.pending.set(true);
    }

    pub(crate) fn listen(&self) -> event_listener::EventListener {
        self.source.listen()
    }

    pub(crate) fn stop_actions(&self) {
        self.source.stop_actions();
    }

    /// Frozen published references for one Matter instance. Only the current
    /// logical functions and their reserved endpoints are published;
    /// historical reservations stay in storage. Matter never rereads the
    /// configuration tables.
    pub(crate) fn frozen(&self) -> Vec<matter::FrozenEntry> {
        let current = self.current();
        let mut entries = current
            .published
            .list()
            .into_iter()
            .map(|device| {
                let key = device.key();
                let committed = current
                    .configuration
                    .device(key)
                    .expect("a published device keeps its committed identity");
                // The served function order is the stable directory structure
                // order; every current function keeps its reserved endpoint.
                let functions = current
                    .derived
                    .visible_functions(key)
                    .into_iter()
                    .map(|function| matter::PublishedFunction {
                        id: function.id.clone(),
                        endpoint: committed
                            .functions
                            .iter()
                            .find(|entry| entry.id == function.id)
                            .expect("a current function keeps its reserved endpoint")
                            .endpoint,
                    })
                    .collect::<Vec<_>>();
                matter::FrozenEntry {
                    device,
                    identity: matter::PublishedIdentity {
                        endpoint: committed.endpoint,
                        unique_id: committed.unique_id.clone(),
                        configuration_version: committed.configuration_version,
                        functions,
                    },
                }
            })
            .collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.identity.endpoint);
        entries
    }

    pub(crate) fn root_version(&self) -> u32 {
        self.current().configuration.root_version
    }

    /// Collect the latest effective input and produce a pure candidate plan.
    /// No activation resource is created here.
    ///
    /// Source facts with an uncleared non-fresh revocation are removed before
    /// Matter shapes are derived, so external shapes always match the planned
    /// candidates.
    pub(crate) fn plan(&self) -> Result<Option<Plan>, StorageError> {
        let current = self.current();
        let snapshot = self.source.snapshot();
        if snapshot.scope.is_none() {
            return Ok(None);
        }
        let revocations = self.store.devices().load_revocations()?;
        let revoked = revocations
            .iter()
            .map(|revocation| revocation.key.clone())
            .collect::<BTreeSet<_>>();
        let mut facts = snapshot;
        facts
            .devices
            .retain(|device| !revoked.contains(&device.key) || device.fresh);
        let facts_derived = facts.derive();
        let facts_shapes = shapes(&facts_derived);
        let current_shapes = shapes(&current.derived);
        let mut new_identities = BTreeMap::new();
        for device in &facts.devices {
            if current.configuration.device(&device.key).is_none() {
                new_identities.insert(
                    device.key.clone(),
                    uuid::Uuid::new_v4().simple().to_string(),
                );
            }
        }
        let plan = match configuration::plan(PlanInput {
            current: current.configuration.clone(),
            current_derived: (*current.derived).clone(),
            current_shapes,
            facts,
            facts_shapes,
            revocations,
            new_identities,
        }) {
            Ok(plan) => plan,
            Err(error) => {
                if let configuration::PlanError::EndpointExhausted { key }
                | configuration::PlanError::VersionExhausted { key } = &error
                {
                    self.source.pending(key, error.to_string());
                }
                return Err(StorageError::new(
                    self.store.path(),
                    "prepare bridge configuration",
                    error.to_string(),
                ));
            }
        };
        if !plan.changed {
            self.pending.set(false);
            self.failure.borrow_mut().take();
            return Ok(None);
        }
        self.pending.set(true);
        Ok(Some(plan))
    }

    /// Prepare the one-time activation resources for the final plan. They are
    /// owned here until `apply` consumes them in the same synchronous turn.
    pub(crate) fn prepare(&self, plan: Plan) -> Prepared {
        let resources = self.source.prepare(&plan);
        Prepared { plan, resources }
    }

    /// Commit the prepared configuration, install the prepared source
    /// instances and switch the effective view. The caller guarantees that no
    /// await happens between `prepare` and `apply`.
    pub(crate) fn apply(&self, prepared: Prepared) -> Result<(), StorageError> {
        let result = (|| {
            if prepared.plan.rebuild {
                matter::discard_subscriptions(&self.store.matter())?;
            }
            self.store.devices().commit_configuration(
                &prepared.plan.configuration,
                &prepared.plan.clear_revocations,
            )
        })();
        if let Err(error) = result {
            *self.failure.borrow_mut() = Some(error.to_string());
            return Err(error);
        }
        let entries = self.source.install(&prepared.plan, prepared.resources);
        let published = published_registry(&prepared.plan.derived.published, &entries);
        let root_version = prepared.plan.configuration.root_version;
        *self.current.borrow_mut() = Rc::new(EffectiveConfiguration {
            configuration: prepared.plan.configuration,
            derived: prepared.plan.derived,
            entries,
            published,
        });
        self.failure.borrow_mut().take();
        self.pending.set(false);
        log::info!("Bridge configuration committed at root version {root_version}");
        Ok(())
    }

    /// Rename one published or hidden device through the one application
    /// configuration entry: persist, then switch the saved name and the
    /// serving object together.
    pub(crate) fn rename(&self, key: &DeviceKey, label: &str) -> Result<(), StorageError> {
        validate_label(label)
            .map_err(|error| StorageError::new(self.store.path(), "write device label", error))?;
        let current = self.current();
        let mut configuration = current.configuration.clone();
        let device = configuration
            .devices
            .iter_mut()
            .find(|device| &device.key == key)
            .ok_or_else(|| {
                StorageError::new(
                    self.store.path(),
                    "write device label",
                    "Device identity does not exist",
                )
            })?;
        device.label = label.to_owned();
        self.store
            .devices()
            .commit_configuration(&configuration, &[])?;
        self.source.set_label(key, label);
        let mut derived = (*current.derived).clone();
        if let Some(served) = derived.served.get_mut(key) {
            let battery = served.description.has_battery();
            let description = crate::device::PhysicalDeviceDescription::new(
                key.clone(),
                label,
                served.description.functions().to_vec(),
            )
            .expect("a served description keeps its functions");
            served.description = if battery {
                description.with_battery()
            } else {
                description
            };
        }
        *self.current.borrow_mut() = Rc::new(EffectiveConfiguration {
            configuration,
            derived: Rc::new(derived),
            entries: current.entries.clone(),
            published: current.published.clone(),
        });
        Ok(())
    }
}

impl LabelWriter for App {
    fn write_label(&self, key: &DeviceKey, label: &str) -> Result<(), StorageError> {
        self.rename(key, label)
    }
}

fn shapes(derived: &DerivedConfiguration) -> BTreeMap<DeviceKey, FixedShape> {
    derived
        .served
        .iter()
        .map(|(key, served)| {
            (
                key.clone(),
                matter::fixed_shape_with_battery(
                    &derived
                        .visible_functions(key)
                        .into_iter()
                        .cloned()
                        .collect::<Vec<_>>(),
                    served.description.has_battery(),
                ),
            )
        })
        .collect()
}

/// The initial serving set only keeps devices with a restored reference.
fn serving_registry(
    published: &BTreeSet<DeviceKey>,
    entries: &[Rc<SourceEntry>],
) -> DeviceRegistry {
    let registry = DeviceRegistry::new();
    for key in published {
        if let Some(entry) = entries.iter().find(|entry| entry.physical.key() == key) {
            registry.insert(entry.physical.clone());
        }
    }
    registry
}

/// After a successful commit every published device has an installed entry.
fn published_registry(
    published: &BTreeSet<DeviceKey>,
    entries: &[Rc<SourceEntry>],
) -> DeviceRegistry {
    let registry = DeviceRegistry::new();
    for key in published {
        let entry = entries
            .iter()
            .find(|entry| entry.physical.key() == key)
            .expect("a published device keeps its serving entry");
        registry.insert(entry.physical.clone());
    }
    registry
}
