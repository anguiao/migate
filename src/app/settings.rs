use super::{App, EffectiveConfiguration, installed_function};
use crate::{
    configuration::{CommittedConfiguration, RelayType, Scope, validate_label},
    device::{DeviceKey, FunctionKey, FunctionKind},
    matter::LabelWriter,
    storage::StorageError,
};
use std::rc::Rc;

impl App {
    /// Persist settings before installing their serving changes and notifying consumers.
    pub(super) fn commit_settings(
        &self,
        current: &EffectiveConfiguration,
        configuration: CommittedConfiguration,
        install: impl FnOnce(&mut EffectiveConfiguration),
    ) -> Result<(), StorageError> {
        self.store
            .devices()
            .commit_configuration(&configuration, &[])?;
        let mut next = EffectiveConfiguration {
            configuration: Rc::new(configuration),
            derived: current.derived.clone(),
            entries: current.entries.clone(),
        };
        install(&mut next);
        self.source
            .set_configuration(next.configuration.clone(), next.derived.clone());
        self.replace_current(next);
        self.committed.notify(usize::MAX);
        Ok(())
    }

    /// Rename one published or hidden device through the one application
    /// configuration entry: persist, then switch the saved name and the
    /// serving object together.
    pub(crate) fn rename(&self, key: &DeviceKey, label: &str) -> Result<(), StorageError> {
        validate_label(label)
            .map_err(|error| StorageError::new(self.store.path(), "write device label", error))?;
        let current = self.current();
        let mut configuration = (*current.configuration).clone();
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
        self.commit_settings(&current, configuration, |next| {
            self.source.set_label(key, label);
            if let Some(served) = Rc::make_mut(&mut next.derived).served.get_mut(key) {
                served.description.set_label(label);
            }
        })
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
        let function =
            installed_function(&current.entries, key).ok_or("Function does not exist.")?;
        if function.description().kind != FunctionKind::Relay {
            return Err("Function is not a relay.".into());
        }
        let scope = current
            .configuration
            .scope
            .as_ref()
            .expect("accessible device has a scope");
        let mut configuration = (*current.configuration).clone();
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
        self.commit_settings(&current, configuration, |_| {})
            .map_err(|error| error.to_string())?;
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
        let mut configuration = (*current.configuration).clone();
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
        self.commit_settings(&current, configuration, |_| {})
    }
}
