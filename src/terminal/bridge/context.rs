use super::super::AuthStatus;
use crate::{
    app::{App, EffectiveConfiguration},
    device::{DeviceKey, PhysicalDevice},
};
use std::{fmt::Write as _, rc::Rc};

/// The terminal's one committed configuration view and persistence handle.
/// Every command takes the current effective version once; hidden members stay
/// visible and controllable.
pub(crate) struct Context<'a> {
    app: Rc<App>,
    pub(super) status: &'a AuthStatus,
}

impl<'a> Context<'a> {
    pub(crate) fn committed(app: Rc<App>, status: &'a AuthStatus) -> Self {
        Self { app, status }
    }

    pub(super) fn view(&self) -> View {
        View {
            current: self.app.current(),
            pending: self.app.pending(),
            failure: self.app.failure(),
        }
    }
}

/// One command observes one committed configuration, including hidden members.
pub(super) struct View {
    pub(super) current: Rc<EffectiveConfiguration>,
    pub(super) pending: bool,
    pub(super) failure: Option<String>,
}

impl View {
    pub(super) fn get(&self, key: &DeviceKey) -> Option<PhysicalDevice> {
        self.current
            .entries
            .iter()
            .find(|entry| entry.physical.key() == key)
            .map(|entry| entry.physical.clone())
    }

    pub(super) fn list(&self) -> Vec<PhysicalDevice> {
        self.current.devices()
    }

    pub(super) fn render_status(&self, status: &AuthStatus, now: i64) -> String {
        let mut report = status.render(now);
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
        now: i64,
        did: Option<&str>,
    ) -> String {
        let mut report = self.render_status(status, now);
        status.append_devices(&mut report, did);
        report
    }
}
