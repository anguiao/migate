use super::super::AuthStatus;
use crate::{
    app::{App, ConfigurationAccess, EffectiveConfiguration},
    device::{DeviceKey, PhysicalDevice},
    storage::StorageError,
    xiaomi::adapter::{AdapterStatus, PendingDevice},
};
use std::{fmt::Write as _, rc::Rc};

/// The terminal's one committed configuration view and persistence handle.
/// Every command takes the current effective version once; hidden members stay
/// visible and controllable.
pub(crate) struct Context<'a> {
    pub(super) app: Rc<App>,
    pub(super) status: &'a AuthStatus,
}

impl<'a> Context<'a> {
    pub(crate) fn committed(app: Rc<App>, status: &'a AuthStatus) -> Self {
        Self { app, status }
    }

    pub(super) fn view(&self) -> Result<View, StorageError> {
        let current = self.app.current();
        let access = self.app.configuration_access(&current)?;
        Ok(View {
            current,
            pending: self.app.pending(),
            failure: self.app.failure(),
            access,
        })
    }
}

/// One command observes one committed configuration, including hidden members.
pub(super) struct View {
    pub(super) current: Rc<EffectiveConfiguration>,
    pub(super) pending: bool,
    pub(super) failure: Option<String>,
    pub(super) access: ConfigurationAccess,
}

impl View {
    pub(super) fn description(
        &self,
        key: &DeviceKey,
    ) -> Option<&crate::xiaomi::capability::RawDescription> {
        if !self.access.descriptions.contains(key) {
            return None;
        }
        self.current
            .configuration
            .descriptions
            .iter()
            .find(|source| &source.key == key)
    }

    pub(super) fn has_relation(&self, key: &DeviceKey) -> bool {
        let configuration = &self.current.configuration;
        let did = &key.id;
        self.access.descriptions.contains(key)
            && configuration
                .split_devices
                .iter()
                .any(|split| &split.did == did)
            || configuration.groups.iter().any(|group| {
                self.access.groups.contains(&group.group_did)
                    && (&group.group_did == did
                        || &group.record_did == did
                        || group.members.contains(did))
            })
    }

    pub(super) fn get(&self, key: &DeviceKey) -> Option<PhysicalDevice> {
        if !self.access.descriptions.contains(key) {
            return None;
        }
        self.current
            .entries
            .iter()
            .find(|entry| entry.physical.key() == key)
            .map(|entry| entry.physical.clone())
    }

    pub(super) fn list(&self) -> Vec<PhysicalDevice> {
        self.current
            .devices()
            .into_iter()
            .filter(|device| self.access.descriptions.contains(device.key()))
            .collect()
    }

    pub(super) fn render_status(&self, status: &AuthStatus, diagnostics: &AdapterStatus) -> String {
        let mut report = status.render_adapter(diagnostics);
        let _ = write!(
            report,
            "\nConfiguration: root version {}{}.",
            self.current.configuration.root_version,
            if self.pending {
                "; pending application"
            } else {
                ""
            },
        );
        if let Some(failure) = &self.failure {
            let _ = write!(report, "\nConfiguration apply failed: {failure}.");
        }
        report
    }

    pub(super) fn render_devices(
        &self,
        status: &AuthStatus,
        diagnostics: &AdapterStatus,
        did: Option<&str>,
    ) -> String {
        let mut report = self.render_status(status, diagnostics);
        super::super::status::append_devices(&mut report, diagnostics, did);
        report
    }

    pub(super) fn pending_line(&self, pending: &PendingDevice) -> String {
        let name = self
            .get(&DeviceKey::new("xiaomi", &pending.did))
            .map(|physical| physical.label())
            .unwrap_or_else(|| pending.name.clone());
        format!("{} ({}): pending ({})", pending.did, name, pending.reason)
    }
}
