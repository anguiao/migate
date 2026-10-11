use super::*;

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
    crate::device::PhysicalDevice::new(crate::device::PhysicalDeviceDescription {
        key: DeviceKey::new("xiaomi", id),
        label: format!("Outlet {id}"),
        functions: functions
            .into_iter()
            .map(|(id, label)| crate::device::FunctionDescription {
                id: id.into(),
                label: label.into(),
                category: Category::Outlet,
                capabilities: vec![Capability::OnOff],
            })
            .collect(),
    })
}
pub(crate) fn test_registry(
    store: &Store,
    device: &PhysicalDevice,
) -> crate::device::DeviceRegistry {
    let stored = store.devices().publish(&device.description()).unwrap();
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

pub(in crate::matter) fn light_caps(
    id: &str,
    capabilities: &[Capability],
) -> (PhysicalDevice, CommandReceiver) {
    let mut description = physical(id, false).0.description();
    let function = &mut description.functions[0];
    function.category = Category::Light;
    function.capabilities = capabilities.to_vec();
    let (device, mut receivers) = PhysicalDevice::new(description);
    (device, receivers.remove(0))
}

pub(in crate::matter) fn light(rgb: bool) -> (PhysicalDevice, CommandReceiver) {
    let mut capabilities = vec![
        Capability::OnOff,
        Capability::Brightness,
        Capability::ColorTemperature(crate::device::NumericRange {
            min: 2700,
            max: 6500,
            step: 1,
        }),
    ];
    if rgb {
        capabilities.push(Capability::ColorRgb);
    }
    light_caps("test-light", &capabilities)
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
