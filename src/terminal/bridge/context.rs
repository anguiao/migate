use super::super::AuthStatus;
use crate::{
    app::{App, EffectiveConfiguration},
    device::{DeviceKey, PhysicalDevice},
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

    pub(super) fn view(&self) -> View {
        let current = self.app.current();
        let configurable_devices = self.app.configurable_devices(&current);
        let accessible_descriptions = self.app.accessible_descriptions(&current);
        let accessible_groups = self.app.accessible_groups(&current);
        View {
            current,
            pending: self.app.pending(),
            failure: self.app.failure(),
            accessible_descriptions,
            accessible_groups,
            configurable_devices,
        }
    }
}

/// One command observes one committed configuration, including hidden members.
pub(super) struct View {
    pub(super) current: Rc<EffectiveConfiguration>,
    pub(super) pending: bool,
    pub(super) failure: Option<String>,
    pub(super) accessible_descriptions: std::collections::BTreeSet<DeviceKey>,
    pub(super) configurable_devices: std::collections::BTreeSet<DeviceKey>,
    pub(super) accessible_groups: std::collections::BTreeSet<String>,
}

impl View {
    pub(super) fn contains_did(&self, did: &str) -> bool {
        let configuration = &self.current.configuration;
        let key = DeviceKey::new("xiaomi", did);
        self.accessible_descriptions.contains(&key)
            && (configuration
                .descriptions
                .iter()
                .any(|source| source.key == key)
                || configuration
                    .split_devices
                    .iter()
                    .any(|split| split.did == did))
            || configuration.groups.iter().any(|group| {
                self.accessible_groups.contains(&group.group_did)
                    && (group.group_did == did
                        || group.record_did == did
                        || group.members.iter().any(|member| member == did))
            })
    }

    pub(super) fn get(&self, key: &DeviceKey) -> Option<PhysicalDevice> {
        if !self.accessible_descriptions.contains(key) {
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
            .filter(|device| self.accessible_descriptions.contains(device.key()))
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
