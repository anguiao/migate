mod configuration;
mod labels;
mod lifecycle;
mod process;

use crate::{
    device::{DeviceKey, FunctionDescription, FunctionKind, PhysicalDeviceDescription},
    storage::Store,
    terminal::AuthStatus,
    xiaomi::{auth::AuthService, cloud::CloudClient},
};

fn outlet() -> PhysicalDeviceDescription {
    PhysicalDeviceDescription::new(
        DeviceKey::new("xiaomi", "test-outlet"),
        "Test Outlet",
        vec![FunctionDescription {
            id: "main".into(),
            label: "Main power".into(),
            kind: FunctionKind::Outlet,
        }],
    )
    .unwrap()
}

fn status(store: &Store) -> AuthStatus {
    AuthStatus::new(
        AuthService::new(store.xiaomi(), CloudClient::new().unwrap())
            .local_status()
            .unwrap(),
        "/bin/migate".into(),
        "/data".into(),
    )
}
