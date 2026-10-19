pub(super) async fn next_power(
    receiver: &mut crate::device::CommandReceiver,
) -> Option<crate::device::CommandRequest> {
    match receiver.next_operation().await? {
        crate::device::DeviceRequest::Power(request) => Some(request),
        crate::device::DeviceRequest::Light(_) => panic!("unexpected lighting request"),
    }
}

mod context;
mod reports;
pub(in crate::matter) use context::Context;
pub(super) use context::attr;
pub(super) use reports::{read_bytes, report_dataver, report_value};

use super::*;
use crate::storage::tests::fixtures::{commit, record, scope};

pub(super) fn add_device(
    store: &Store,
    description: &crate::device::PhysicalDeviceDescription,
) -> crate::storage::StoredDevice {
    let mut state = store.devices().load_publication(None).unwrap();
    assert!(
        state
            .records
            .iter()
            .all(|stored| &stored.key != description.key())
    );
    let stored = record(description, store.devices().next_endpoint().unwrap());
    state.records.push(stored.clone());
    store
        .devices()
        .apply_publication(&commit(scope(), Vec::new(), state.records))
        .unwrap();
    stored
}

pub(super) fn set_published(
    store: &Store,
    key: &DeviceKey,
    published: bool,
) -> crate::storage::StoredDevice {
    let mut state = store.devices().load_publication(None).unwrap();
    let stored = state
        .records
        .iter_mut()
        .find(|record| &record.key == key)
        .unwrap();
    stored.published = published;
    let stored = stored.clone();
    store
        .devices()
        .apply_publication(&commit(scope(), Vec::new(), state.records))
        .unwrap();
    stored
}

pub(super) fn physical(
    id: &str,
    multi: bool,
) -> (crate::device::PhysicalDevice, Vec<CommandReceiver>) {
    let functions = if multi {
        vec![
            ("outlet_1", "开关 1"),
            ("outlet_2", "开关 2"),
            ("usb", "USB"),
        ]
    } else {
        vec![("main", "主电源")]
    };
    crate::device::PhysicalDevice::new(
        crate::device::PhysicalDeviceDescription::new(
            DeviceKey::new("xiaomi", id),
            format!("Outlet {id}"),
            functions
                .into_iter()
                .map(|(id, label)| crate::device::FunctionDescription {
                    id: id.into(),
                    label: label.into(),
                    kind: FunctionKind::Outlet,
                })
                .collect(),
        )
        .unwrap(),
    )
}
pub(crate) fn test_registry(
    store: &Store,
    device: &PhysicalDevice,
) -> crate::device::DeviceRegistry {
    let stored = store
        .devices()
        .load_published(device.key())
        .unwrap()
        .unwrap_or_else(|| add_device(store, &device.description()));
    device.set_label(stored.label);
    let registry = crate::device::DeviceRegistry::new();
    registry.insert(device.clone());
    registry
}
pub(crate) fn test_device() -> (PhysicalDevice, CommandReceiver) {
    let (device, mut receivers) = physical("test-outlet", false);
    device.set_label("Test Outlet".into());
    (device, receivers.remove(0))
}

pub(in crate::matter) fn light(rgb: bool) -> (PhysicalDevice, CommandReceiver) {
    let interval = crate::device::TemperatureInterval::new(2700, 6500).unwrap();
    light_with(if rgb {
        LightProfile::ExtendedColor(interval)
    } else {
        LightProfile::ColorTemperature(interval)
    })
}

pub(in crate::matter) fn light_with(profile: LightProfile) -> (PhysicalDevice, CommandReceiver) {
    let outlet = physical("test-light", false).0.description();
    let mut functions = outlet.functions().to_vec();
    functions[0].kind = FunctionKind::Light(profile);
    let description = crate::device::PhysicalDeviceDescription::new(
        outlet.key().clone(),
        outlet.label(),
        functions,
    )
    .unwrap();
    let (device, mut receivers) = PhysicalDevice::new(description);
    (device, receivers.remove(0))
}
pub(super) async fn terminal_command(device: &PhysicalDevice, line: &str) -> Option<String> {
    use crate::{
        terminal::{self, AuthStatus},
        xiaomi::auth::{AuthReport, AuthenticationState, CertificateUpdate},
    };
    let report = AuthReport::for_test(
        AuthenticationState::NotSignedIn,
        None,
        CertificateUpdate::NotNeeded,
        0,
    );
    let status = AuthStatus::new(report, "/bin/migate".into(), "/data".into());
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let registry = test_registry(&store, device);
    let line = if matches!(line, "on" | "off") {
        format!("{line} {}", device.key().id)
    } else {
        line.into()
    };
    terminal::handle_line(&registry, &store.devices(), &status, &line, 0)
        .await
        .unwrap()
}

pub(super) fn startup_error(store: &Store) -> RuntimeError {
    let identity = store.load_identity().unwrap();
    let (device, _receiver) = test_device();
    let registry = test_registry(store, &device);
    let bridge = Bridge::new(&registry, store, &identity);
    block_on(or(bridge.run(0, |_| Ok(())), async {
        async_io::Timer::after(std::time::Duration::from_secs(5)).await;
        panic!("startup did not report invalid storage");
    }))
    .unwrap_err()
}
