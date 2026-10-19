use super::super::AuthStatus;
use crate::{
    device::{DeviceKey, DeviceRegistry, PhysicalDevice},
    publication::{EffectiveConfiguration, Publication},
    storage::DeviceStore,
};
use std::{fmt::Write as _, rc::Rc};

enum Source {
    Registry(DeviceRegistry),
    Configured(Rc<Publication>),
}

/// The terminal's explicit device source, persistence and diagnostics.
pub(crate) struct Context<'a> {
    source: Source,
    pub(super) store: DeviceStore,
    pub(super) status: &'a AuthStatus,
}

impl<'a> Context<'a> {
    pub(crate) fn fixed(
        registry: &DeviceRegistry,
        store: &DeviceStore,
        status: &'a AuthStatus,
    ) -> Self {
        Self {
            source: Source::Registry(registry.clone()),
            store: store.clone(),
            status,
        }
    }

    pub(crate) fn configured(
        publication: Rc<Publication>,
        store: &DeviceStore,
        status: &'a AuthStatus,
    ) -> Self {
        Self {
            source: Source::Configured(publication),
            store: store.clone(),
            status,
        }
    }

    pub(super) fn view(&self) -> View<'_> {
        match &self.source {
            Source::Registry(registry) => View::Registry(registry),
            Source::Configured(publication) => View::Configured {
                current: publication.current(),
                pending: publication.pending(),
                failure: publication.failure(),
            },
        }
    }
}

/// One command observes one committed configuration, including hidden members.
pub(super) enum View<'a> {
    Registry(&'a DeviceRegistry),
    Configured {
        current: Rc<EffectiveConfiguration>,
        pending: bool,
        failure: Option<String>,
    },
}

impl View<'_> {
    pub(super) fn configuration(&self) -> Option<&EffectiveConfiguration> {
        match self {
            Self::Registry(_) => None,
            Self::Configured { current, .. } => Some(current),
        }
    }

    pub(super) fn get(&self, key: &DeviceKey) -> Option<PhysicalDevice> {
        match self {
            Self::Registry(registry) => registry.get(key),
            Self::Configured { current, .. } => current
                .source
                .entries
                .iter()
                .find(|entry| entry.physical.key() == key)
                .map(|entry| entry.physical.clone()),
        }
    }

    pub(super) fn list(&self) -> Vec<PhysicalDevice> {
        match self {
            Self::Registry(registry) => registry.list(),
            Self::Configured { current, .. } => current.devices(),
        }
    }

    pub(super) fn render_status(&self, status: &AuthStatus, now: i64) -> String {
        let mut report = status.render(now);
        if let Self::Configured {
            current,
            pending,
            failure,
        } = self
        {
            let _ = write!(
                report,
                "\nConfiguration: root version {}{}.",
                current.root_version,
                if *pending {
                    "; pending application"
                } else {
                    ""
                },
            );
            if let Some(failure) = failure {
                let _ = write!(report, "\nConfiguration apply failed: {failure}.");
            }
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
