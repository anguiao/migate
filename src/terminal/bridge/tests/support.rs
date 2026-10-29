//! Terminal rule fixtures use the real committed model and ordinary Functions.
//! Application lifecycle and configuration switching are tested through App.

use super::super::{View, read_commands};
use crate::{
    RuntimeError,
    app::EffectiveConfiguration,
    device::{
        CommandReceiver, CommandRequest, DeviceKey, DeviceRegistry, DeviceRequest, PhysicalDevice,
        PhysicalDeviceDescription,
    },
    storage::{DeviceStore, StorageError, Store},
    terminal::AuthStatus,
    xiaomi::{
        adapter::publication::SourceEntry,
        auth::{AuthReport, AuthenticationState, CertificateUpdate},
        capability::tests::model_fixture::native_for,
    },
};
use std::rc::Rc;

pub(super) async fn next_power(receiver: &mut CommandReceiver) -> Option<CommandRequest> {
    match receiver.next_operation().await? {
        DeviceRequest::Power(request) => Some(request),
        _ => panic!("unexpected lighting request"),
    }
}

pub(super) fn add_device(store: &Store, description: &PhysicalDeviceDescription) {
    let mut configuration = store.devices().load_configuration().unwrap();
    assert!(configuration.device(description.key()).is_none());
    let endpoint = configuration
        .devices
        .iter()
        .flat_map(|device| {
            std::iter::once(device.endpoint)
                .chain(device.functions.iter().map(|function| function.endpoint))
        })
        .max()
        .unwrap_or(1)
        + 1;
    let native = native_for(description);
    let native_description = native.physical_description(description.label());
    let mut device = crate::storage::tests::fixtures::identity(&native_description, endpoint);
    device.native = Some(native);
    configuration.devices.push(device);
    configuration.scope = Some(crate::storage::tests::fixtures::scope());
    store
        .devices()
        .commit_configuration(&configuration, &[])
        .unwrap();
}

pub(super) fn rename(store: &Store, key: &DeviceKey, label: &str) {
    let mut configuration = store.devices().load_configuration().unwrap();
    configuration
        .devices
        .iter_mut()
        .find(|device| &device.key == key)
        .unwrap()
        .label = label.into();
    store
        .devices()
        .commit_configuration(&configuration, &[])
        .unwrap();
}

pub(super) fn saved_description(store: &Store, key: &DeviceKey) -> PhysicalDeviceDescription {
    store
        .devices()
        .load_configuration()
        .unwrap()
        .derive()
        .served[key]
        .description
        .clone()
}

fn view(registry: &DeviceRegistry, store: &DeviceStore) -> Result<View, StorageError> {
    let configuration = store.load_configuration()?;
    let derived = configuration.derive();
    let entries = registry
        .list()
        .into_iter()
        .map(|physical| {
            let native = configuration
                .device(physical.key())
                .unwrap()
                .native
                .clone()
                .unwrap();
            Rc::new(SourceEntry::for_test(native, physical))
        })
        .collect();
    Ok(View {
        current: Rc::new(EffectiveConfiguration {
            configuration,
            derived: derived.into(),
            entries,
            published: registry.clone(),
        }),
        pending: false,
        failure: None,
    })
}

pub(crate) async fn handle_line(
    registry: &DeviceRegistry,
    store: &DeviceStore,
    status: &AuthStatus,
    line: &str,
    now: i64,
) -> Result<Option<String>, StorageError> {
    view(registry, store)?.handle_line(line, now, status).await
}

pub(super) async fn run_input(
    registry: &DeviceRegistry,
    store: &DeviceStore,
    status: &AuthStatus,
    input: impl futures_lite::io::AsyncBufRead + Unpin,
    output: impl futures_lite::io::AsyncWrite + Unpin,
) -> Result<(), RuntimeError> {
    read_commands(input, output, async |line| {
        handle_line(registry, store, status, line, 0).await
    })
    .await
}

/// Exercise terminal commands against the same Function used by a protocol test.
pub(crate) async fn terminal_command(device: &PhysicalDevice, line: &str) -> Option<String> {
    let status = AuthStatus::new(
        AuthReport::for_test(
            AuthenticationState::NotSignedIn,
            None,
            CertificateUpdate::NotNeeded,
            0,
        ),
        "/bin/migate".into(),
        "/data".into(),
    );
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    add_device(&store, &device.description());
    let registry = DeviceRegistry::new();
    registry.insert(device.clone());
    let line = if matches!(line, "on" | "off") {
        format!(
            "{line} {} {}",
            device.key().id,
            device.functions()[0].function_key().id
        )
    } else {
        line.into()
    };
    handle_line(&registry, &store.devices(), &status, &line, 0)
        .await
        .unwrap()
}
