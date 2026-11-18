//! Application assembly: one committed configuration drives every runtime view.
//!
//! The application owns the latest source handle, the effective configuration
//! and the one-time activation resources between planning and installation. It
//! never exposes configuration submission or source slots to read-only views.

mod lifecycle;
mod sensor_binding;
mod settings;
mod shutdown;
mod startup;

pub use lifecycle::run_xiaomi_bridge;
pub use shutdown::shutdown_signal;

use sensor_binding::activate_temperature_bindings;

use crate::{
    configuration::{self, CommittedConfiguration, DerivedConfiguration, Plan, PlanInput},
    device::{DeviceKey, Function, FunctionKey, PhysicalDevice},
    matter,
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
    pub(crate) configuration: Rc<CommittedConfiguration>,
    pub(crate) derived: Rc<DerivedConfiguration>,
    pub(crate) entries: Vec<Rc<SourceEntry>>,
}

pub(crate) struct ConfigurationAccess {
    pub(crate) descriptions: BTreeSet<DeviceKey>,
    pub(crate) groups: BTreeSet<String>,
    pub(crate) configurable_devices: BTreeSet<DeviceKey>,
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
        let configuration = source.configuration();
        // Serving starts from the committed graph with real revocations
        // overlaid. Planning separately compares the unmodified committed graph
        // so withdrawals still receive their version increments.
        let derived = source.committed_graph();
        let entries = source.entries(&configuration);
        activate_temperature_bindings(&configuration, &entries);
        Ok(Self {
            source,
            store,
            current: RefCell::new(Rc::new(EffectiveConfiguration {
                configuration,
                derived,
                entries,
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

    fn replace_current(&self, current: EffectiveConfiguration) {
        *self.current.borrow_mut() = Rc::new(current);
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

    fn revoked_keys(&self) -> Result<BTreeSet<DeviceKey>, StorageError> {
        Ok(self
            .store
            .devices()
            .load_revocations()?
            .into_iter()
            .map(|record| record.key)
            .collect())
    }

    /// Project one committed view through current source authority, including a failed withdrawal save.
    pub(crate) fn configuration_access(
        &self,
        current: &EffectiveConfiguration,
    ) -> Result<ConfigurationAccess, StorageError> {
        let revoked = self.revoked_keys()?;
        let mut descriptions = self.source.accessible_descriptions(&current.configuration);
        descriptions.retain(|key| !revoked.contains(key));
        let groups = self
            .source
            .accessible_groups(&current.configuration, &revoked);
        let configurable_devices = current
            .entries
            .iter()
            .filter(|entry| {
                descriptions.contains(entry.physical.key())
                    && self
                        .source
                        .binding_accessible(&current.configuration.scope, entry.physical.key())
            })
            .map(|entry| entry.physical.key().clone())
            .collect();
        Ok(ConfigurationAccess {
            descriptions,
            groups,
            configurable_devices,
        })
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

    /// Assemble one Matter instance from the same committed configuration.
    /// Only retained sources and visible functions use their reserved endpoints;
    /// historical reservations stay in storage.
    pub(crate) fn matter_configuration(&self) -> matter::MatterConfiguration {
        let current = self.current();
        let mut entries = current
            .entries
            .iter()
            .filter(|entry| {
                let key = entry.physical.key();
                current.derived.published.contains(key)
                    && self
                        .source
                        .source_retained(&current.configuration.scope, key)
            })
            .map(|entry| {
                let key = entry.physical.key();
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
                matter::PublishedEntry {
                    device: entry.physical.clone(),
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
                entries.push(matter::PublishedEntry {
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
        matter::MatterConfiguration {
            entries,
            root_version: current.configuration.root_version,
        }
    }

    /// Collect the latest effective input and produce a pure candidate plan.
    /// No activation resource is created here.
    pub(crate) fn plan(&self) -> Result<Option<Plan>, StorageError> {
        if !self.source.has_scope() {
            return Ok(None);
        }
        let current = self.current();
        let revoked = self.revoked_keys()?;
        let snapshot = self.source.snapshot(&current.configuration, &revoked);
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
                revoked,
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
            let Plan {
                configuration,
                derived,
                operations,
                ..
            } = plan;
            let configuration = Rc::new(configuration);
            let entries =
                self.source
                    .install(&configuration, &derived, &operations, prepared.resources);
            activate_temperature_bindings(&configuration, &entries);
            let root_version = configuration.root_version;
            self.replace_current(EffectiveConfiguration {
                configuration,
                derived,
                entries,
            });
            self.committed.notify(usize::MAX);
            log::info!("Bridge configuration committed at root version {root_version}");
        }
        self.failure.borrow_mut().take();
        self.pending.set(false);
        self.rebuild_pending.set(false);
        Ok(())
    }
}

fn installed_function(entries: &[Rc<SourceEntry>], key: &FunctionKey) -> Option<Function> {
    entries
        .iter()
        .find(|entry| entry.physical.key() == &key.device)?
        .physical
        .function(&key.id)
}

#[cfg(test)]
mod tests;
