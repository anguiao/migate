//! Application assembly: one committed configuration drives every runtime view.
//!
//! The application owns the latest source handle, the effective configuration
//! and the one-time activation resources between planning and installation. It
//! never exposes configuration submission or source slots to read-only views.

mod lifecycle;
mod sensor_binding;
mod shutdown;
mod startup;

pub use lifecycle::run_xiaomi_bridge;
pub use shutdown::shutdown_signal;

use sensor_binding::activate_temperature_bindings;

use crate::{
    configuration::{
        self, CommittedConfiguration, DerivedConfiguration, Plan, PlanInput, RelayType, Scope,
        validate_label,
    },
    device::{DeviceKey, DeviceRegistry, FunctionKey, PhysicalDevice},
    matter::{self, LabelWriter},
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
    committed: event_listener::Event,
    rebuild_requested: event_listener::Event,
    rebuild_pending: Cell<bool>,
    failure: RefCell<Option<String>>,
    pending: Cell<bool>,
}

impl App {
    pub(crate) fn open(source: SourceHandle, store: Store) -> Result<Self, StorageError> {
        let configuration = store.devices().load_configuration()?;
        // Serving starts from the committed graph with real revocations
        // overlaid. Planning separately compares the unmodified committed graph
        // so withdrawals still receive their version increments.
        let derived = source.committed_graph();
        let entries = source.entries(&configuration);
        let published = serving_registry(&derived.published, &entries);
        activate_temperature_bindings(&configuration, &entries);
        Ok(Self {
            source,
            store,
            current: RefCell::new(Rc::new(EffectiveConfiguration {
                configuration,
                derived,
                entries,
                published,
            })),
            committed: event_listener::Event::new(),
            rebuild_requested: event_listener::Event::new(),
            rebuild_pending: Cell::new(false),
            failure: RefCell::new(None),
            pending: Cell::new(false),
        })
    }

    pub(crate) fn current(&self) -> Rc<EffectiveConfiguration> {
        self.current.borrow().clone()
    }

    pub(crate) fn diagnostics(&self) -> crate::xiaomi::adapter::AdapterStatus {
        self.source.diagnostics()
    }

    pub(crate) fn failure(&self) -> Option<String> {
        self.failure.borrow().clone()
    }

    pub(crate) fn pending(&self) -> bool {
        self.pending.get()
    }

    pub(crate) fn accessible_descriptions(&self) -> BTreeSet<DeviceKey> {
        self.source.accessible_descriptions()
    }

    pub(crate) fn configurable_devices(
        &self,
        current: &EffectiveConfiguration,
    ) -> BTreeSet<DeviceKey> {
        current
            .entries
            .iter()
            .filter(|entry| {
                self.source
                    .binding_accessible(&current.configuration.scope, entry.physical.key())
            })
            .map(|entry| entry.physical.key().clone())
            .collect()
    }

    /// Record a non-fatal prepare/save failure for the terminal; the committed
    /// configuration and runner stay untouched.
    pub(crate) fn fail(&self, reason: String) {
        *self.failure.borrow_mut() = Some(reason);
        self.pending.set(true);
    }

    pub(crate) fn listen(&self) -> impl std::future::Future<Output = ()> {
        let source = self.source.listen();
        let requested = self.rebuild_requested.listen();
        async move { futures_lite::future::or(source, requested).await }
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
            .filter(|device| {
                self.source
                    .source_retained(&current.configuration.scope, device.key())
            })
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
                        relay: None,
                    },
                }
            })
            .collect::<Vec<_>>();
        for key in &current.derived.published_relays {
            let channel = current
                .configuration
                .relay_channels
                .iter()
                .find(|channel| {
                    current.configuration.scope.as_ref() == Some(&channel.scope)
                        && &channel.key == key
                })
                .expect("a published relay keeps its identity");
            if let Some(source) = current
                .entries
                .iter()
                .find(|entry| entry.physical.key() == &key.device)
                && self
                    .source
                    .source_retained(&current.configuration.scope, &key.device)
            {
                entries.push(matter::FrozenEntry {
                    device: source.physical.clone(),
                    identity: matter::PublishedIdentity {
                        endpoint: channel.endpoint,
                        unique_id: channel.unique_id.clone(),
                        configuration_version: channel.configuration_version,
                        functions: Vec::new(),
                        relay: Some(matter::PublishedRelay {
                            scope: channel.scope.clone(),
                            function_id: key.id.clone(),
                            device_type: channel.device_type,
                        }),
                    },
                });
            }
        }
        entries.sort_by_key(|entry| entry.identity.endpoint);
        entries
    }

    pub(crate) fn root_version(&self) -> u32 {
        self.current().configuration.root_version
    }

    /// Collect the latest effective input and produce a pure candidate plan.
    /// No activation resource is created here.
    pub(crate) fn plan(&self) -> Result<Option<Plan>, StorageError> {
        let current = self.current();
        let snapshot = self.source.snapshot();
        if snapshot.scope.is_none() {
            return Ok(None);
        }
        let revocations = self.store.devices().load_revocations()?;
        let committed_derived = current.configuration.derive();
        let mut new_identities = BTreeMap::new();
        for device in &snapshot.devices {
            let description = &device.description;
            if description.native.is_some()
                && current.configuration.device(&description.raw.key).is_none()
            {
                new_identities.insert(
                    description.raw.key.clone(),
                    uuid::Uuid::new_v4().simple().to_string(),
                );
            }
        }
        let mut plan = match configuration::plan(
            PlanInput {
                current: &current.configuration,
                current_derived: &committed_derived,
                facts: snapshot,
                revocations,
                new_identities,
            },
            |functions, battery| {
                matter::fixed_shape_with_battery(functions.iter().copied(), battery)
            },
        ) {
            Ok(plan) => plan,
            Err(error) => {
                if let configuration::PlanError::EndpointExhausted { key }
                | configuration::PlanError::VersionExhausted { key } = &error
                {
                    self.source
                        .pending(key, &current.configuration, error.to_string());
                }
                return Err(StorageError::new(
                    self.store.path(),
                    "prepare bridge configuration",
                    error.to_string(),
                ));
            }
        };
        self.source.planned();
        plan.rebuild |= self.rebuild_pending.get();
        if !plan.changed && !plan.rebuild {
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

    /// Install new source facts, or finish rebuilding an already committed mapping.
    /// The caller guarantees that no await happens between `prepare` and `apply`.
    pub(crate) fn apply(&self, prepared: Prepared) -> Result<(), StorageError> {
        let plan = prepared.plan;
        if plan.changed {
            // Subscriptions identify a Matter peer and requested paths. The new
            // model resolves those paths with current ACLs; a different Xiaomi
            // account or home must not inherit the previous scope's requests.
            let scope_changed = self.current().configuration.scope != plan.configuration.scope;
            if let Err(error) = self.store.devices().commit_configuration_with_reset(
                &plan.configuration,
                &plan.clear_revocations,
                scope_changed.then(matter::subscription_keys),
            ) {
                *self.failure.borrow_mut() = Some(error.to_string());
                return Err(error);
            }
            let entries = self.source.install(&plan, prepared.resources);
            let published = published_registry(&plan.derived.published, &entries);
            activate_temperature_bindings(&plan.configuration, &entries);
            let root_version = plan.configuration.root_version;
            *self.current.borrow_mut() = Rc::new(EffectiveConfiguration {
                configuration: plan.configuration,
                derived: plan.derived,
                entries,
                published,
            });
            self.committed.notify(usize::MAX);
            log::info!("Bridge configuration committed at root version {root_version}");
        }
        self.failure.borrow_mut().take();
        self.pending.set(false);
        self.rebuild_pending.set(false);
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
            served.description.set_label(label);
        }
        *self.current.borrow_mut() = Rc::new(EffectiveConfiguration {
            configuration,
            derived: Rc::new(derived),
            entries: current.entries.clone(),
            published: current.published.clone(),
        });
        self.committed.notify(usize::MAX);
        Ok(())
    }

    /// Save a mapping only after validating its current source authority.
    /// A visible change wakes the lifecycle to replace the Matter instance.
    pub(crate) fn map_relay_type(
        &self,
        key: &FunctionKey,
        device_type: RelayType,
    ) -> Result<bool, String> {
        let current = self.current();
        if !self
            .source
            .binding_accessible(&current.configuration.scope, &key.device)
        {
            return Err("Device is not in the current home or has no current access.".into());
        }
        let function = current
            .entries
            .iter()
            .find(|entry| entry.physical.key() == &key.device)
            .and_then(|entry| entry.physical.function(&key.id))
            .ok_or("Function does not exist.")?;
        if function.description().kind != crate::device::FunctionKind::Relay {
            return Err("Function is not a relay.".into());
        }
        let scope = current
            .configuration
            .scope
            .as_ref()
            .expect("accessible device has a scope");
        let mut configuration = current.configuration.clone();
        let channel = configuration
            .relay_channels
            .iter_mut()
            .find(|channel| &channel.scope == scope && &channel.key == key)
            .ok_or("Relay is not recognized in the current home.")?;
        if channel.device_type == device_type {
            return Ok(false);
        }
        let visible = current.derived.published_relays.contains(key);
        if visible {
            channel.configuration_version = channel
                .configuration_version
                .checked_add(1)
                .ok_or("Relay configuration version is exhausted.")?;
            configuration.root_version = configuration
                .root_version
                .checked_add(1)
                .ok_or("Root configuration version is exhausted.")?;
        }
        channel.device_type = device_type;
        self.store
            .devices()
            .commit_configuration(&configuration, &[])
            .map_err(|error| error.to_string())?;
        *self.current.borrow_mut() = Rc::new(EffectiveConfiguration {
            configuration,
            derived: current.derived.clone(),
            entries: current.entries.clone(),
            published: current.published.clone(),
        });
        self.committed.notify(usize::MAX);
        if visible {
            self.rebuild_pending.set(true);
            self.pending.set(true);
            self.rebuild_requested.notify(usize::MAX);
        }
        Ok(true)
    }
}

impl LabelWriter for App {
    fn write_label(&self, key: &DeviceKey, label: &str) -> Result<(), StorageError> {
        self.rename(key, label)
    }

    fn relay_label(&self, scope: &Scope, key: &FunctionKey) -> Option<String> {
        self.current()
            .configuration
            .relay_channels
            .iter()
            .find(|channel| &channel.scope == scope && &channel.key == key)
            .map(|channel| channel.label.clone())
    }

    fn write_relay_label(
        &self,
        scope: &Scope,
        key: &FunctionKey,
        label: &str,
    ) -> Result<(), StorageError> {
        validate_label(label)
            .map_err(|error| StorageError::new(self.store.path(), "write relay label", error))?;
        let current = self.current();
        let mut configuration = current.configuration.clone();
        let channel = configuration
            .relay_channels
            .iter_mut()
            .find(|channel| &channel.scope == scope && &channel.key == key)
            .ok_or_else(|| {
                StorageError::new(
                    self.store.path(),
                    "write relay label",
                    "Relay identity does not exist",
                )
            })?;
        channel.label = label.to_owned();
        self.store
            .devices()
            .commit_configuration(&configuration, &[])?;
        *self.current.borrow_mut() = Rc::new(EffectiveConfiguration {
            configuration,
            derived: current.derived.clone(),
            entries: current.entries.clone(),
            published: current.published.clone(),
        });
        self.committed.notify(usize::MAX);
        Ok(())
    }

    fn relay_accessible(&self, scope: &Scope, key: &FunctionKey) -> bool {
        self.source.relay_accessible(scope, key)
    }
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
