pub(crate) mod support;
use support::{add_device, handle_line, next_power, rename, run_input, saved_description};

use crate::{
    device::{
        Availability, CommandReceiver, DeviceKey, DeviceRegistry, FunctionDescription,
        FunctionKind, PhysicalDevice, PhysicalDeviceDescription, Power,
    },
    storage::Store,
    terminal::AuthStatus,
    xiaomi::{auth::AuthService, cloud::CloudClient},
};
use futures_lite::{
    future::{self, block_on, poll_once},
    io::Cursor,
};
fn fixture() -> (
    tempfile::TempDir,
    Store,
    AuthStatus,
    DeviceRegistry,
    Vec<CommandReceiver>,
) {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let report = AuthService::new(store.xiaomi(), CloudClient::new().unwrap())
        .local_status()
        .unwrap();
    let status = AuthStatus::new(report, "/bin/migate".into(), directory.path().into());
    let registry = DeviceRegistry::new();
    let mut receivers = Vec::new();
    for (did, ids) in [
        ("one", vec!["main"]),
        ("two", vec!["main"]),
        ("multi", vec!["outlet_1", "outlet_2", "usb"]),
    ] {
        let (physical, mut consumers) = PhysicalDevice::new(
            PhysicalDeviceDescription::new(
                DeviceKey::new("xiaomi", did),
                format!("name {did}"),
                ids.into_iter()
                    .map(|id| FunctionDescription {
                        id: id.into(),
                        label: id.into(),
                        kind: FunctionKind::Outlet,
                    })
                    .collect(),
            )
            .unwrap(),
        );
        physical.set_availability(Availability::Available);
        add_device(&store, &physical.description());
        registry.insert(physical);
        receivers.append(&mut consumers);
    }
    (directory, store, status, registry, receivers)
}
#[test]
fn addressing_is_explicit_and_invalid_commands_produce_no_writes() {
    block_on(async {
        let (_dir, store, status, registry, mut receivers) = fixture();
        for invalid in [
            "on",
            "off",
            "on multi",
            "on missing",
            "off multi missing",
            "on one main extra",
            "status one extra",
            "list extra",
            "help extra",
            "toggle one",
            "ON one",
        ] {
            let result = handle_line(&registry, &store.devices(), &status, invalid, 0)
                .await
                .unwrap()
                .unwrap();
            assert!(
                result.contains("Available commands:"),
                "{invalid}: {result}"
            );
            for receiver in &mut receivers {
                assert!(poll_once(next_power(receiver)).await.is_none());
            }
        }
        let (result, ()) = future::zip(
            handle_line(&registry, &store.devices(), &status, "on one", 0),
            async {
                let request = next_power(&mut receivers[0]).await.unwrap();
                registry
                    .get(&DeviceKey::new("xiaomi", "one"))
                    .unwrap()
                    .functions()[0]
                    .observe(true);
                request.accept().unwrap();
            },
        )
        .await;
        assert_eq!(result.unwrap().unwrap(), "one: command accepted");
        let (result, ()) = future::zip(
            handle_line(&registry, &store.devices(), &status, "off multi usb", 0),
            async {
                let request = next_power(&mut receivers[4]).await.unwrap();
                assert!(!request.target);
                registry
                    .get(&DeviceKey::new("xiaomi", "multi"))
                    .unwrap()
                    .function("usb")
                    .unwrap()
                    .observe(false);
                request.accept().unwrap();
            },
        )
        .await;
        assert_eq!(result.unwrap().unwrap(), "multi/usb: command accepted");
        assert_eq!(
            registry
                .get(&DeviceKey::new("xiaomi", "one"))
                .unwrap()
                .functions()[0]
                .snapshot()
                .power,
            Power::On
        );
    })
}
#[test]
fn list_and_status_read_shared_power_and_persisted_names() {
    block_on(async {
        let (_dir, store, status, registry, _receivers) = fixture();
        let key = DeviceKey::new("xiaomi", "multi");
        rename(&store, &key, "Saved Home name");
        let (physical, _consumers) = PhysicalDevice::new(saved_description(&store, &key));
        registry.remove(&key);
        registry.insert(physical.clone());
        physical.function("usb").unwrap().observe(true);
        let list = handle_line(&registry, &store.devices(), &status, "list", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(list.contains("Saved Home name"));
        assert!(list.contains("usb (usb): on"));
        assert!(list.contains("one (name one)"));
        physical.set_label("Live Home name".into());
        let selected = handle_line(&registry, &store.devices(), &status, "status multi", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(!selected.contains("one (name one)"));
        assert!(selected.contains("Live Home name"));
        assert!(!selected.contains("Saved Home name"));
        assert!(selected.contains("Xiaomi: not signed in"));
        registry.remove(physical.key());
        assert!(
            handle_line(&registry, &store.devices(), &status, "on multi usb", 0)
                .await
                .unwrap()
                .unwrap()
                .contains("Unknown device")
        );
    })
}

#[test]
fn light_list_and_status_show_capabilities_snapshot_unknowns_and_saved_names() {
    use crate::{
        device::{LightMode, LightPatch, Rgb},
        xiaomi::capability::parse_device_for_model,
    };

    block_on(async {
        let (_dir, store, status, registry, _receivers) = fixture();
        let mut lights = Vec::new();
        for (did, model, source) in [
            (
                "rgb",
                "devcea.light.ls2307",
                include_str!("../../../../tests/fixtures/miot/devcea-light-ls2307.json"),
            ),
            (
                "ct",
                "yeelink.light.light3",
                include_str!("../../../../tests/fixtures/miot/yeelink-light-light3.json"),
            ),
        ] {
            let key = DeviceKey::new("xiaomi", did);
            let parsed =
                parse_device_for_model(key.clone(), model, &serde_json::from_str(source).unwrap())
                    .unwrap();
            add_device(
                &store,
                &PhysicalDeviceDescription::new(
                    key.clone(),
                    "Imported 米家 name",
                    parsed
                        .functions()
                        .iter()
                        .map(|function| function.description())
                        .collect(),
                )
                .unwrap(),
            );
            rename(&store, &key, &format!("Saved {did} 名称"));
            let (physical, receivers) = PhysicalDevice::new(saved_description(&store, &key));
            physical.set_availability(Availability::Available);
            registry.insert(physical.clone());
            lights.push((physical, receivers));
        }
        let list = handle_line(&registry, &store.devices(), &status, "list", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(list.contains("rgb (Saved rgb 名称): available"), "{list}");
        assert!(list.contains("main (Light): unknown"), "{list}");
        assert!(
            list.contains("Capabilities: on/off, brightness, temperature 2700–6500 K, RGB."),
            "{list}"
        );
        assert!(
            list.contains(
                "Brightness: unknown; temperature: unknown; RGB: unknown; mode: unknown."
            ),
            "{list}"
        );
        assert!(
            list.contains("Brightness: unknown; temperature: unknown; mode: temperature."),
            "{list}"
        );
        assert!(!list.contains("Imported 米家 name"));

        lights[0].0.functions()[0]
            .observe_light(LightPatch {
                brightness_percent: Some(23.5),
                temperature_kelvin: Some(4000),
                mode: Some(LightMode::Rgb),
                ..Default::default()
            })
            .unwrap();
        let partial = handle_line(&registry, &store.devices(), &status, "status rgb", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(
            partial.contains("Brightness: 23.5%; temperature: 4000 K; RGB: unknown; mode: RGB."),
            "{partial}"
        );
        lights[0].0.functions()[0]
            .observe_light(LightPatch {
                power: Some(true),
                brightness_percent: Some(72.5),
                temperature_kelvin: Some(4000),
                color: Some(Rgb {
                    red: 255,
                    green: 128,
                    blue: 64,
                }),
                mode: Some(LightMode::Rgb),
            })
            .unwrap();
        lights[1].0.functions()[0]
            .observe_light(LightPatch {
                power: Some(true),
                brightness_percent: Some(0.0),
                temperature_kelvin: Some(4000),
                ..Default::default()
            })
            .unwrap();
        let rgb = handle_line(&registry, &store.devices(), &status, "status rgb", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(rgb.contains("Saved rgb 名称"));
        assert!(rgb.contains("main (Light): on"));
        assert!(
            rgb.contains("Brightness: 72.5%; temperature: 4000 K; RGB: #FF8040; mode: RGB."),
            "{rgb}"
        );
        assert!(!rgb.contains("Saved ct 名称"));
        assert!(!rgb.contains("name one"));
        let ct = handle_line(&registry, &store.devices(), &status, "status ct", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(ct.contains("main (Light): on"));
        assert!(
            ct.contains("Capabilities: on/off, brightness, temperature 2700–6500 K."),
            "{ct}"
        );
        assert!(
            ct.contains("Brightness: 0%; temperature: 4000 K; mode: temperature."),
            "{ct}"
        );
        assert!(!ct.contains("RGB"));
    });
}

#[test]
fn light_terminal_onoff_remains_power_only_and_has_no_dimming_commands() {
    use crate::device::{DeviceRequest, LightMode, LightPatch, LightProfile, TemperatureInterval};

    block_on(async {
        let (_dir, store, status, registry, _receivers) = fixture();
        let (physical, mut receivers) = PhysicalDevice::new(
            PhysicalDeviceDescription::new(
                DeviceKey::new("xiaomi", "light"),
                "Original name",
                vec![FunctionDescription {
                    id: "main".into(),
                    label: "Saved function name".into(),
                    kind: FunctionKind::Light(LightProfile::ColorTemperature(
                        TemperatureInterval::new(2700, 6500).unwrap(),
                    )),
                }],
            )
            .unwrap(),
        );
        physical.set_availability(Availability::Available);
        physical.functions()[0]
            .observe_light(LightPatch {
                brightness_percent: Some(23.5),
                temperature_kelvin: Some(4000),
                ..Default::default()
            })
            .unwrap();
        add_device(&store, &physical.description());
        registry.insert(physical.clone());
        for (line, target) in [("on light", true), ("off light main", false)] {
            let (result, ()) = future::zip(
                handle_line(&registry, &store.devices(), &status, line, 0),
                async {
                    let DeviceRequest::Power(request) =
                        receivers[0].next_operation().await.unwrap()
                    else {
                        panic!("terminal power command changed lighting values")
                    };
                    assert_eq!(request.target, target);
                    request.accept().unwrap();
                },
            )
            .await;
            assert_eq!(result.unwrap().unwrap(), "light: command accepted");
            let snapshot = physical.functions()[0].snapshot().lighting.unwrap();
            assert_eq!(snapshot.brightness_percent, Some(23.5));
            assert_eq!(snapshot.temperature_kelvin, Some(4000));
            assert_eq!(snapshot.mode, Some(LightMode::Temperature));
        }
        let invalid = handle_line(
            &registry,
            &store.devices(),
            &status,
            "brightness light 50",
            0,
        )
        .await
        .unwrap()
        .unwrap();
        assert!(invalid.contains("Invalid command usage."));
        assert!(poll_once(receivers[0].next_operation()).await.is_none());
    });
}
#[test]
fn input_recovers_and_returns_at_eof() {
    block_on(async {
        let (_dir, store, status, registry, mut receivers) = fixture();
        let mut output = Vec::new();
        future::or(
            run_input(
                &registry,
                &store.devices(),
                &status,
                Cursor::new(b" \non one\nbad\nstatus\noff one"),
                &mut output,
            ),
            async {
                loop {
                    let request = next_power(&mut receivers[0]).await.unwrap();
                    registry
                        .get(&DeviceKey::new("xiaomi", "one"))
                        .unwrap()
                        .functions()[0]
                        .observe(request.target);
                    request.accept().unwrap();
                }
            },
        )
        .await
        .unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("Available commands:"));
        assert!(output.ends_with("one: command accepted\n"));
        assert_eq!(
            registry
                .get(&DeviceKey::new("xiaomi", "one"))
                .unwrap()
                .functions()[0]
                .snapshot()
                .power,
            Power::Off
        );
    })
}
#[test]
fn input_and_output_errors_propagate() {
    block_on(async {
        let (_dir, store, status, registry, _receivers) = fixture();
        let error = run_input(
            &registry,
            &store.devices(),
            &status,
            Cursor::new(b"\xff\n"),
            Vec::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(
            error.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::InvalidData
        );
        assert!(
            run_input(
                &registry,
                &store.devices(),
                &status,
                Cursor::new(b"status\n"),
                Cursor::new(&mut [0u8; 0][..])
            )
            .await
            .is_err()
        );
    })
}
#[test]
fn pending_diagnostics_keep_the_saved_name_and_help_explains_stable_channels() {
    block_on(async {
        let (_dir, store, status, registry, _receivers) = fixture();
        rename(
            &store,
            &DeviceKey::new("xiaomi", "multi"),
            "Home edited name",
        );
        registry
            .get(&DeviceKey::new("xiaomi", "multi"))
            .unwrap()
            .set_label("Home edited name".into());
        let report = AuthService::new(store.xiaomi(), CloudClient::new().unwrap())
            .local_status()
            .unwrap();
        status.update_adapter(crate::xiaomi::adapter::AdapterStatus {
            authentication: report,
            gateway: crate::xiaomi::adapter::GatewayStatus::Connected("hub".into()),
            catalog_problem: None,
            pending: vec![crate::xiaomi::adapter::PendingDevice {
                did: "multi".into(),
                name: "New cloud name".into(),
                reason: "subscription rejected".into(),
            }],
            routing: Default::default(),
            devices: Vec::new(),
        });
        let list = handle_line(&registry, &store.devices(), &status, "list", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(list.contains("Home edited name"));
        assert!(!list.contains("New cloud name"));
        assert!(list.contains("subscription rejected"));
        let help = handle_line(&registry, &store.devices(), &status, "help", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(help.contains("channel"));
        assert!(help.contains("list"));
        assert!(help.contains("required"));
    })
}

#[test]
fn detailed_status_filters_device_routes_while_list_and_help_remain_concise() {
    use crate::xiaomi::adapter::*;
    block_on(async {
        let (_dir, store, status, registry, _receivers) = fixture();
        let report = AuthService::new(store.xiaomi(), CloudClient::new().unwrap())
            .local_status()
            .unwrap();
        status.update_adapter(AdapterStatus {
            authentication: report,
            gateway: GatewayStatus::Connected("hub".into()),
            catalog_problem: None,
            pending: Vec::new(),
            routing: RoutingStatus {
                priority: PathPriority::CloudFirst,
                local_connected: true,
                business_degraded: true,
                recovery: RecoveryStatus::Scheduled(1_700_000_060),
                last_switch_reason: None,
            },
            devices: ["one", "multi"]
                .into_iter()
                .map(|did| DeviceStatus {
                    did: did.into(),
                    usable_control_paths: vec![PathKind::Local, PathKind::Cloud],
                    selected_control: Some(PathKind::Cloud),
                    notification_source: Some(PathKind::Local),
                    unavailable_reason: None,
                    sync_limited_reason: None,
                    last_switch_reason: None,
                    commands: Vec::new(),
                })
                .collect(),
        });
        let selected = handle_line(
            &registry,
            &store.devices(),
            &status,
            "status multi",
            1_700_000_000,
        )
        .await
        .unwrap()
        .unwrap();
        assert!(selected.contains("multi (name multi)"), "{selected}");
        assert!(selected.contains("multi routing:"), "{selected}");
        assert!(!selected.contains("one routing:"), "{selected}");
        assert!(!selected.contains("one (name one)"), "{selected}");
        assert!(selected.contains("Gateway: connected (hub)."), "{selected}");
        assert!(selected.contains("Routing: cloud-first"), "{selected}");
        assert!(selected.contains("2023-11-14T22:14:20Z"), "{selected}");
        let all = handle_line(
            &registry,
            &store.devices(),
            &status,
            "status",
            1_700_000_000,
        )
        .await
        .unwrap()
        .unwrap();
        assert!(all.contains("one routing:"));
        assert!(all.contains("multi routing:"));
        let list = handle_line(&registry, &store.devices(), &status, "list", 1_700_000_000)
            .await
            .unwrap()
            .unwrap();
        assert!(list.contains("one (name one)"));
        assert!(!list.contains("routing:"));
        assert!(!list.contains("Selected control:"));
        let help = handle_line(&registry, &store.devices(), &status, "help", 1_700_000_000)
            .await
            .unwrap()
            .unwrap();
        assert!(help.contains("Available commands:"));
        assert!(help.contains("Routing: cloud-first"));
        assert!(!help.contains("one routing:"));
        assert!(!help.contains("multi routing:"));
    });
}
