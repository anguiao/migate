use crate::{
    device::{DeviceKey, FunctionDescription, FunctionKind, PhysicalDeviceDescription},
    storage::Store,
    terminal::AuthStatus,
    xiaomi::{auth::AuthService, cloud::CloudClient},
};

pub(in crate::app) fn outlet() -> PhysicalDeviceDescription {
    PhysicalDeviceDescription::new(
        DeviceKey::new("xiaomi", "test-outlet"),
        "Test Outlet",
        vec![FunctionDescription {
            id: "outlet:2".into(),
            label: "Main power".into(),
            kind: FunctionKind::Outlet,
        }],
    )
    .unwrap()
}

pub(in crate::app) fn status(store: &Store) -> AuthStatus {
    AuthStatus::new(
        AuthService::new(store.xiaomi(), CloudClient::new().unwrap())
            .unwrap()
            .local_status()
            .unwrap(),
        "/bin/migate".into(),
        "/data".into(),
    )
}

impl crate::app::EffectiveConfiguration {
    pub(crate) fn published_device(
        &self,
        key: &DeviceKey,
    ) -> Option<crate::device::PhysicalDevice> {
        if !self.derived.published.contains(key) {
            return None;
        }
        self.entries
            .iter()
            .find(|entry| entry.physical.key() == key)
            .map(|entry| entry.physical.clone())
    }

    pub(crate) fn published_devices(&self) -> Vec<crate::device::PhysicalDevice> {
        self.entries
            .iter()
            .filter(|entry| self.derived.published.contains(entry.physical.key()))
            .map(|entry| entry.physical.clone())
            .collect()
    }
}
