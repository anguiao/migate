//! Private bridge tests for actual light capability presentation.
//!
//! Every case drives `handle_line` with a real registry snapshot and adapter
//! diagnostics, using only existing APIs. Expectations separate declared
//! capabilities, supported-but-unknown values, and absent dimensions.

use super::handle_line;
use crate::{
    device::{
        Availability, Capability, Category, CommandError, CommandReceiver, Composition, DeviceKey,
        DeviceRegistry, DeviceRequest, FunctionDescription, LightDimension, LightMode, LightPatch,
        LightTarget, NumericRange, PhysicalDevice, PhysicalDeviceDescription,
    },
    storage::Store,
    terminal::AuthStatus,
    xiaomi::{
        adapter::{AdapterStatus, ChannelCommands, DeviceStatus, GatewayStatus, PathKind},
        auth::{AuthReport, AuthenticationState, CertificateUpdate},
    },
};
use futures_lite::future::block_on;

fn report() -> AuthReport {
    AuthReport::for_test(
        AuthenticationState::Authenticated,
        None,
        CertificateUpdate::NotNeeded,
        0,
    )
}

fn temperature_range() -> NumericRange {
    NumericRange {
        min: 2700,
        max: 6500,
        step: 1,
    }
}

fn light(
    did: &str,
    label: &str,
    capabilities: Vec<Capability>,
) -> (PhysicalDevice, CommandReceiver) {
    let (physical, mut receivers) = PhysicalDevice::new(PhysicalDeviceDescription {
        key: DeviceKey::new("xiaomi", did),
        label: label.into(),
        functions: vec![FunctionDescription {
            id: "main".into(),
            label: "Light".into(),
            category: Category::Light,
            capabilities,
        }],
    });
    physical.set_availability(Availability::Available);
    (physical, receivers.remove(0))
}

fn status_for(directory: &tempfile::TempDir) -> AuthStatus {
    AuthStatus::new(report(), "/bin/migate".into(), directory.path().into())
}

fn publish_diagnostics(status: &AuthStatus, registry: &DeviceRegistry) {
    let devices = registry
        .snapshot()
        .devices
        .iter()
        .map(|device| {
            let function = device.function("main").expect("fixture light function");
            DeviceStatus {
                did: device.key().id.clone(),
                usable_control_paths: vec![PathKind::Cloud],
                selected_control: Some(PathKind::Cloud),
                notification_source: Some(PathKind::Local),
                unavailable_reason: None,
                sync_limited_reason: None,
                last_switch_reason: None,
                commands: vec![ChannelCommands {
                    light_actions: function.light_action_diagnostics(),
                    channel: "main".into(),
                    inflight: None,
                    last: None,
                }],
            }
        })
        .collect();
    status.update_adapter(AdapterStatus {
        authentication: report(),
        gateway: GatewayStatus::Connected("hub".into()),
        catalog_problem: None,
        pending: Vec::new(),
        routing: Default::default(),
        devices,
    });
}

fn occurrences(haystack: &str, needle: &str) -> usize {
    haystack.match_indices(needle).count()
}

#[test]
fn onoff_only_light_renders_its_capability_and_no_light_values() {
    block_on(async {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let status = status_for(&directory);
        let registry = DeviceRegistry::new();
        let (physical, _receiver) = light("onoff", "Lamp", vec![Capability::OnOff]);
        registry.insert(physical);
        publish_diagnostics(&status, &registry);

        let list = handle_line(&registry, &store.devices(), &status, "list", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(list.contains("main (Light): unknown"), "{list}");
        assert!(list.contains("Capabilities: on/off."), "{list}");
        assert!(!list.contains("Brightness:"), "{list}");
        assert!(!list.contains("temperature:"), "{list}");
        assert!(!list.contains("RGB:"), "{list}");
        assert!(!list.contains("mode:"), "{list}");

        let selected = handle_line(&registry, &store.devices(), &status, "status onoff", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(selected.contains("Capabilities: on/off."), "{selected}");
        assert!(!selected.contains("actions:"), "{selected}");
    });
}

#[test]
fn brightness_only_light_keeps_supported_unknown_and_omits_color_fields() {
    block_on(async {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let status = status_for(&directory);
        let registry = DeviceRegistry::new();
        let (physical, _receiver) = light(
            "brightness",
            "Lamp",
            vec![Capability::OnOff, Capability::Brightness],
        );
        registry.insert(physical);
        publish_diagnostics(&status, &registry);

        let list = handle_line(&registry, &store.devices(), &status, "list", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(list.contains("Capabilities: on/off, brightness."), "{list}");
        assert!(list.contains("Brightness: unknown."), "{list}");
        assert!(!list.contains("temperature:"), "{list}");
        assert!(!list.contains("RGB:"), "{list}");
        assert!(!list.contains("mode:"), "{list}");

        let selected = handle_line(&registry, &store.devices(), &status, "status brightness", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(
            selected.contains(
                "main actions: brightness idle; awaiting startup receipt none; last none."
            ),
            "{selected}"
        );
        assert!(!selected.contains("color idle"), "{selected}");
    });
}

#[test]
fn temperature_only_light_keeps_intrinsic_mode_and_omits_brightness() {
    block_on(async {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let status = status_for(&directory);
        let registry = DeviceRegistry::new();
        let (physical, _receiver) = light(
            "temperature",
            "Lamp",
            vec![
                Capability::OnOff,
                Capability::ColorTemperature(temperature_range()),
            ],
        );
        registry.insert(physical);
        publish_diagnostics(&status, &registry);

        let list = handle_line(&registry, &store.devices(), &status, "list", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(
            list.contains("Capabilities: on/off, temperature 2700–6500 K."),
            "{list}"
        );
        assert!(
            list.contains("temperature: unknown; mode: temperature."),
            "{list}"
        );
        assert!(!list.contains("Brightness:"), "{list}");
        assert!(!list.contains("RGB:"), "{list}");

        let selected = handle_line(
            &registry,
            &store.devices(),
            &status,
            "status temperature",
            0,
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            selected
                .contains("main actions: color idle; awaiting startup receipt none; last none."),
            "{selected}"
        );
        assert!(!selected.contains("brightness idle"), "{selected}");
    });
}

#[test]
fn rgb_only_light_keeps_intrinsic_mode_and_omits_brightness_and_temperature() {
    block_on(async {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let status = status_for(&directory);
        let registry = DeviceRegistry::new();
        let (physical, _receiver) =
            light("rgb", "Lamp", vec![Capability::OnOff, Capability::ColorRgb]);
        registry.insert(physical);
        publish_diagnostics(&status, &registry);

        let list = handle_line(&registry, &store.devices(), &status, "list", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(list.contains("Capabilities: on/off, RGB."), "{list}");
        assert!(list.contains("RGB: unknown; mode: RGB."), "{list}");
        assert!(!list.contains("Brightness:"), "{list}");
        assert!(!list.contains("temperature:"), "{list}");

        let selected = handle_line(&registry, &store.devices(), &status, "status rgb", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(
            selected
                .contains("main actions: color idle; awaiting startup receipt none; last none."),
            "{selected}"
        );
        assert!(!selected.contains("brightness idle"), "{selected}");
    });
}

#[test]
fn light_without_onoff_does_not_invent_power() {
    block_on(async {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let status = status_for(&directory);
        let registry = DeviceRegistry::new();
        let (physical, _receiver) = light(
            "color",
            "Lamp",
            vec![Capability::ColorTemperature(temperature_range())],
        );
        registry.insert(physical);
        publish_diagnostics(&status, &registry);

        let list = handle_line(&registry, &store.devices(), &status, "list", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(
            list.contains("Capabilities: temperature 2700–6500 K."),
            "{list}"
        );
        assert!(
            list.contains("temperature: unknown; mode: temperature."),
            "{list}"
        );
        assert!(!list.contains("(Light): unknown"), "{list}");
        assert!(!list.contains("(Light): on"), "{list}");
        assert!(!list.contains("(Light): off"), "{list}");
        assert!(!list.contains("Brightness:"), "{list}");
    });
}

#[test]
fn mixed_group_members_render_own_capabilities_and_dimensions() {
    block_on(async {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let status = status_for(&directory);
        let registry = DeviceRegistry::new();
        let (one, _one_receiver) = light(
            "one",
            "Lamp one",
            vec![Capability::OnOff, Capability::Brightness],
        );
        let (two, _two_receiver) = light(
            "two",
            "Lamp two",
            vec![
                Capability::OnOff,
                Capability::Brightness,
                Capability::ColorTemperature(temperature_range()),
                Capability::ColorRgb,
            ],
        );
        registry.insert(one);
        registry.insert(two.clone());
        registry
            .update_composition_versioned(
                1,
                vec![Composition {
                    key: DeviceKey::new("xiaomi", "group.1"),
                    label: "Reading lamp".into(),
                    members: vec![
                        DeviceKey::new("xiaomi", "one"),
                        DeviceKey::new("xiaomi", "two"),
                    ],
                }],
                &[],
            )
            .unwrap();
        two.functions()[0]
            .observe_light(LightPatch {
                mode: Some(LightMode::Rgb),
                ..Default::default()
            })
            .unwrap();
        publish_diagnostics(&status, &registry);

        let list = handle_line(&registry, &store.devices(), &status, "list", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(list.contains("Capabilities: on/off, brightness."), "{list}");
        assert!(
            list.contains("Capabilities: on/off, brightness, temperature 2700–6500 K, RGB."),
            "{list}"
        );

        let group = handle_line(&registry, &store.devices(), &status, "status group.1", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(group.contains("RGB: unknown; mode: RGB."), "{group}");
        assert_eq!(occurrences(&group, "color idle"), 1, "{group}");
        assert_eq!(occurrences(&group, "brightness idle"), 2, "{group}");

        let one_status = handle_line(&registry, &store.devices(), &status, "status one", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(one_status.contains("Brightness: unknown."), "{one_status}");
        assert!(!one_status.contains("temperature:"), "{one_status}");
        assert!(!one_status.contains("RGB:"), "{one_status}");

        let two_status = handle_line(&registry, &store.devices(), &status, "status two", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(
            two_status.contains("RGB: unknown; mode: RGB."),
            "{two_status}"
        );
        assert!(two_status.contains("temperature: unknown"), "{two_status}");
    });
}

#[test]
fn diagnostics_without_a_matching_function_keep_only_receipt_and_last_info() {
    block_on(async {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let status = status_for(&directory);
        let registry = DeviceRegistry::new();
        let (physical, mut receiver) = light(
            "one",
            "Lamp",
            vec![Capability::OnOff, Capability::Brightness],
        );
        registry.insert(physical);
        let function = registry
            .get(&DeviceKey::new("xiaomi", "one"))
            .unwrap()
            .function("main")
            .unwrap();
        function
            .begin_light_action(
                LightDimension::Brightness,
                LightTarget {
                    brightness_percent: Some(25.0),
                    ..Default::default()
                },
                false,
            )
            .unwrap();
        let DeviceRequest::Light(frame) = receiver.next_operation().await.unwrap() else {
            panic!("expected a lighting request");
        };
        frame.reject(CommandError::Rejected("denied".into()));
        let light_actions = function.light_action_diagnostics().unwrap();

        status.update_adapter(AdapterStatus {
            authentication: report(),
            gateway: GatewayStatus::Connected("hub".into()),
            catalog_problem: None,
            pending: Vec::new(),
            routing: Default::default(),
            devices: vec![DeviceStatus {
                did: "one".into(),
                usable_control_paths: vec![PathKind::Cloud],
                selected_control: Some(PathKind::Cloud),
                notification_source: Some(PathKind::Local),
                unavailable_reason: None,
                sync_limited_reason: None,
                last_switch_reason: None,
                commands: vec![ChannelCommands {
                    light_actions: Some(light_actions),
                    channel: "ghost".into(),
                    inflight: None,
                    last: None,
                }],
            }],
        });

        let selected = handle_line(&registry, &store.devices(), &status, "status one", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(
            selected.contains(
                "ghost actions: awaiting startup receipt none; last brightness failed: command rejected: denied."
            ),
            "{selected}"
        );
        assert!(!selected.contains("brightness idle"), "{selected}");
        assert!(!selected.contains("color idle"), "{selected}");
    });
}
