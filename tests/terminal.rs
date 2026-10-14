use futures_lite::{
    future::{self, block_on, poll_once},
    io::Cursor,
};
use migate::{
    device::{
        Availability, Capability, Category, CommandReceiver, DeviceKey, DeviceRegistry,
        FunctionDescription, PhysicalDevice, PhysicalDeviceDescription, Power,
    },
    storage::Store,
    terminal::{
        AuthStatus,
        bridge::{handle_line, run_input},
    },
    xiaomi::{auth::AuthService, cloud::CloudClient},
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
        let (physical, mut consumers) = PhysicalDevice::new(PhysicalDeviceDescription {
            key: DeviceKey::new("xiaomi", did),
            label: format!("name {did}"),
            functions: ids
                .into_iter()
                .map(|id| FunctionDescription {
                    id: id.into(),
                    label: id.into(),
                    category: Category::Outlet,
                    capabilities: vec![Capability::OnOff],
                })
                .collect(),
        });
        physical.set_availability(Availability::Available);
        store.devices().publish(&physical.description()).unwrap();
        registry.insert(physical);
        receivers.append(&mut consumers);
    }
    registry
        .apply_configuration(
            1,
            Vec::new(),
            &[
                DeviceKey::new("xiaomi", "one"),
                DeviceKey::new("xiaomi", "two"),
                DeviceKey::new("xiaomi", "multi"),
            ],
            &[],
        )
        .unwrap();
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
                assert!(poll_once(receiver.next()).await.is_none());
            }
        }
        let (result, ()) = future::zip(
            handle_line(&registry, &store.devices(), &status, "on one", 0),
            async {
                receivers[0].next().await.unwrap().accept().unwrap();
            },
        )
        .await;
        assert_eq!(result.unwrap().unwrap(), "one: command accepted");
        let (result, ()) = future::zip(
            handle_line(&registry, &store.devices(), &status, "off multi usb", 0),
            async {
                let request = receivers[4].next().await.unwrap();
                assert!(!request.target);
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
        store
            .devices()
            .set_device_label(&key, "Saved Home name")
            .unwrap();
        let stored = store.devices().load_published(&key).unwrap().unwrap();
        let (physical, _consumers) = PhysicalDevice::new(stored.description());
        registry.remove(&key);
        registry.insert(physical.clone());
        registry
            .apply_configuration(
                2,
                Vec::new(),
                &[
                    DeviceKey::new("xiaomi", "one"),
                    DeviceKey::new("xiaomi", "two"),
                    DeviceKey::new("xiaomi", "multi"),
                ],
                &[],
            )
            .unwrap();
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
    use migate::{
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
                include_str!("fixtures/miot/devcea-light-ls2307.json"),
            ),
            (
                "ct",
                "yeelink.light.light3",
                include_str!("fixtures/miot/yeelink-light-light3.json"),
            ),
        ] {
            let key = DeviceKey::new("xiaomi", did);
            let parsed =
                parse_device_for_model(key.clone(), model, &serde_json::from_str(source).unwrap())
                    .unwrap();
            store
                .devices()
                .publish(&PhysicalDeviceDescription {
                    key: key.clone(),
                    label: "Imported 米家 name".into(),
                    functions: parsed
                        .functions
                        .into_iter()
                        .map(|function| function.description)
                        .collect(),
                })
                .unwrap();
            store
                .devices()
                .set_device_label(&key, &format!("Saved {did} 名称"))
                .unwrap();
            let saved = store.devices().load_published(&key).unwrap().unwrap();
            let (physical, receivers) = PhysicalDevice::new(saved.description());
            physical.set_availability(Availability::Available);
            registry.insert(physical.clone());
            lights.push((physical, receivers));
        }
        registry
            .apply_configuration(
                2,
                Vec::new(),
                &[
                    DeviceKey::new("xiaomi", "one"),
                    DeviceKey::new("xiaomi", "two"),
                    DeviceKey::new("xiaomi", "multi"),
                    DeviceKey::new("xiaomi", "rgb"),
                    DeviceKey::new("xiaomi", "ct"),
                ],
                &[],
            )
            .unwrap();
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
    use migate::device::{DeviceRequest, LightMode, LightPatch, NumericRange};

    block_on(async {
        let (_dir, store, status, registry, _receivers) = fixture();
        let (physical, mut receivers) = PhysicalDevice::new(PhysicalDeviceDescription {
            key: DeviceKey::new("xiaomi", "light"),
            label: "Original name".into(),
            functions: vec![FunctionDescription {
                id: "main".into(),
                label: "Saved function name".into(),
                category: Category::Light,
                capabilities: vec![
                    Capability::OnOff,
                    Capability::Brightness,
                    Capability::ColorTemperature(NumericRange {
                        min: 2700,
                        max: 6500,
                        step: 1,
                    }),
                ],
            }],
        });
        physical.set_availability(Availability::Available);
        physical.functions()[0]
            .observe_light(LightPatch {
                brightness_percent: Some(23.5),
                temperature_kelvin: Some(4000),
                ..Default::default()
            })
            .unwrap();
        store.devices().publish(&physical.description()).unwrap();
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
                    receivers[0].next().await.unwrap().accept().unwrap();
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
        store
            .devices()
            .set_device_label(&DeviceKey::new("xiaomi", "multi"), "Home edited name")
            .unwrap();
        registry
            .get(&DeviceKey::new("xiaomi", "multi"))
            .unwrap()
            .set_label("Home edited name".into());
        let report = AuthService::new(store.xiaomi(), CloudClient::new().unwrap())
            .local_status()
            .unwrap();
        status.update_adapter(migate::xiaomi::adapter::AdapterStatus {
            authentication: report,
            gateway: migate::xiaomi::adapter::GatewayStatus::Connected("hub".into()),
            catalog_problem: None,
            pending: vec![migate::xiaomi::adapter::PendingDevice {
                did: "multi".into(),
                name: "New cloud name".into(),
                reason: "subscription rejected".into(),
                group: false,
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
    use migate::xiaomi::adapter::*;
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

#[test]
fn startup_login_command_is_a_single_prefixed_log_record() {
    use migate::xiaomi::adapter::*;
    let capture = tempfile::NamedTempFile::new().unwrap();
    env_logger::Builder::new()
        .filter_module("migate::terminal::status", log::LevelFilter::Info)
        .write_style(env_logger::WriteStyle::Never)
        .target(env_logger::Target::Pipe(Box::new(
            capture.reopen().unwrap(),
        )))
        .init();
    let (directory, store, status, _registry, _receivers) = fixture();
    status.log(0);

    let gateway = format!(
        "status-log-{}",
        directory.path().file_name().unwrap().to_string_lossy()
    );
    let report = AuthService::new(store.xiaomi(), CloudClient::new().unwrap())
        .local_status()
        .unwrap();
    let mut adapter = AdapterStatus {
        authentication: report,
        gateway: GatewayStatus::Connected(gateway.clone()),
        catalog_problem: None,
        pending: Vec::new(),
        routing: Default::default(),
        devices: Vec::new(),
    };
    status.update_adapter(adapter.clone());
    adapter.devices.push(DeviceStatus {
        did: "one".into(),
        usable_control_paths: vec![PathKind::Local],
        selected_control: Some(PathKind::Local),
        notification_source: Some(PathKind::Local),
        unavailable_reason: None,
        sync_limited_reason: None,
        last_switch_reason: None,
        commands: vec![ChannelCommands {
            light_actions: None,
            channel: "main".into(),
            inflight: None,
            last: None,
        }],
    });
    status.update_adapter(adapter.clone());
    adapter.devices[0].commands[0].last = Some(CommandStatus {
        id: 1,
        binding: migate::xiaomi::capability::PropertyBinding { siid: 2, piid: 1 },
        target: migate::xiaomi::adapter::CommandTarget::Power(true),
        properties: vec![(
            migate::xiaomi::capability::PropertyBinding { siid: 2, piid: 1 },
            true.into(),
        )],
        actual_path: Some(PathKind::Local),
        result: Some(Ok(())),
    });
    status.update_adapter(adapter);

    let output = std::fs::read_to_string(capture.path()).unwrap();
    assert_eq!(
        output
            .lines()
            .filter(|line| line.contains(&format!("Gateway: connected ({gateway}).")))
            .count(),
        1,
        "{output}"
    );
    let lines: Vec<_> = output
        .lines()
        .filter(|line| {
            line.contains(" migate::terminal::status] ")
                && line.contains(&directory.path().display().to_string())
        })
        .collect();
    let expected_messages = [format!(
        "Sign in with: '/bin/migate' --data-dir '{}' auth login",
        directory.path().display()
    )];
    assert_eq!(lines.len(), expected_messages.len(), "{output}");
    for (line, message) in lines.iter().zip(expected_messages) {
        let (prefix, body) = line.split_once("] ").unwrap();
        assert!(prefix.starts_with('['), "{line}");
        assert!(prefix.contains(" INFO "), "{line}");
        assert_eq!(body, message);
    }
}

fn group_fixture() -> (
    tempfile::TempDir,
    Store,
    AuthStatus,
    DeviceRegistry,
    Vec<CommandReceiver>,
) {
    use migate::device::{GroupConfiguration, NumericRange};
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let report = AuthService::new(store.xiaomi(), CloudClient::new().unwrap())
        .local_status()
        .unwrap();
    let status = AuthStatus::new(report, "/bin/migate".into(), directory.path().into());
    let registry = DeviceRegistry::new();
    let mut receivers = Vec::new();
    for (did, label) in [("one", "Lamp one"), ("two", "Lamp two")] {
        let (physical, mut consumers) = PhysicalDevice::new(PhysicalDeviceDescription {
            key: DeviceKey::new("xiaomi", did),
            label: label.into(),
            functions: vec![FunctionDescription {
                id: "main".into(),
                label: "Light".into(),
                category: Category::Light,
                capabilities: vec![
                    Capability::OnOff,
                    Capability::Brightness,
                    Capability::ColorTemperature(NumericRange {
                        min: 2700,
                        max: 6500,
                        step: 1,
                    }),
                ],
            }],
        });
        physical.set_availability(Availability::Available);
        store.devices().publish(&physical.description()).unwrap();
        registry.insert(physical);
        receivers.append(&mut consumers);
    }
    registry
        .apply_configuration(
            1,
            vec![GroupConfiguration {
                capabilities: registry
                    .get(&DeviceKey::new("xiaomi", "one"))
                    .unwrap()
                    .functions()[0]
                    .description()
                    .capabilities
                    .clone(),
                key: DeviceKey::new("xiaomi", "group.1"),
                label: "Reading lamp".into(),
                members: vec![
                    DeviceKey::new("xiaomi", "one"),
                    DeviceKey::new("xiaomi", "two"),
                ],
            }],
            &[],
            &[],
        )
        .unwrap();
    (directory, store, status, registry, receivers)
}

#[test]
fn group_list_and_status_show_identity_members_capabilities_and_member_state() {
    use migate::device::{GroupConfiguration, LightPatch};
    block_on(async {
        let (_dir, store, status, registry, _receivers) = group_fixture();
        let one = registry.get(&DeviceKey::new("xiaomi", "one")).unwrap();
        let two = registry.get(&DeviceKey::new("xiaomi", "two")).unwrap();
        one.functions()[0]
            .observe_light(LightPatch {
                power: Some(true),
                ..Default::default()
            })
            .unwrap();
        two.functions()[0]
            .observe_light(LightPatch {
                power: Some(false),
                brightness_percent: Some(40.0),
                ..Default::default()
            })
            .unwrap();

        let list = handle_line(&registry, &store.devices(), &status, "list", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(
            list.contains("group.1 (Reading lamp): group; members: one (Lamp one), two (Lamp two)"),
            "{list}"
        );
        assert!(list.contains("one (Lamp one): available"), "{list}");
        assert!(list.contains("two (Lamp two): available"), "{list}");
        assert!(list.contains("main (Light): on"), "{list}");
        assert!(list.contains("main (Light): off"), "{list}");
        assert!(
            list.contains("Capabilities: on/off, brightness, temperature 2700–6500 K."),
            "{list}"
        );
        assert!(list.contains("Group: group.1 (Reading lamp)."), "{list}");

        registry
            .apply_configuration(
                2,
                vec![GroupConfiguration {
                    capabilities: one.functions()[0].description().capabilities.clone(),
                    key: DeviceKey::new("xiaomi", "group.1"),
                    label: "Reading lamp".into(),
                    members: vec![
                        DeviceKey::new("xiaomi", "two"),
                        DeviceKey::new("xiaomi", "one"),
                    ],
                }],
                &[],
                &[],
            )
            .unwrap();
        assert_eq!(
            registry.snapshot().applied.groups[0].members,
            vec![
                DeviceKey::new("xiaomi", "one"),
                DeviceKey::new("xiaomi", "two")
            ],
            "member reordering never renumbers the remaining identities"
        );
        let reordered = handle_line(&registry, &store.devices(), &status, "status group.1", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(
            reordered
                .contains("group.1 (Reading lamp): group; members: one (Lamp one), two (Lamp two)"),
            "{reordered}"
        );
        assert!(
            reordered.contains("one (Lamp one): available"),
            "{reordered}"
        );
        assert!(reordered.contains("main (Light): on"), "{reordered}");
        assert!(reordered.contains("main (Light): off"), "{reordered}");
        assert!(!reordered.contains("name one"), "{reordered}");

        let member = handle_line(&registry, &store.devices(), &status, "status one", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(
            member.contains("Group: group.1 (Reading lamp)."),
            "{member}"
        );
        assert!(member.contains("one (Lamp one)"), "{member}");
        assert!(!member.contains("two (Lamp two)"), "{member}");

        one.set_label("Lamp".into());
        two.set_label("Lamp".into());
        let same_names = handle_line(&registry, &store.devices(), &status, "list", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(
            same_names.contains("members: one (Lamp), two (Lamp)"),
            "{same_names}"
        );
        assert!(same_names.contains("one (Lamp): available"), "{same_names}");
        assert!(same_names.contains("two (Lamp): available"), "{same_names}");

        registry
            .apply_configuration(3, Vec::new(), &[one.key().clone(), two.key().clone()], &[])
            .unwrap();
        let ungrouped = handle_line(&registry, &store.devices(), &status, "list", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(!ungrouped.contains("Reading lamp"), "{ungrouped}");
        assert!(!ungrouped.contains("Group:"), "{ungrouped}");
        let gone = handle_line(&registry, &store.devices(), &status, "status group.1", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(gone.contains("Unknown device DID."), "{gone}");
    })
}

#[test]
fn group_status_includes_member_routes_and_errors_while_member_status_names_its_group() {
    use migate::xiaomi::adapter::*;
    block_on(async {
        let (_dir, store, status, registry, _receivers) = group_fixture();
        let report = AuthService::new(store.xiaomi(), CloudClient::new().unwrap())
            .local_status()
            .unwrap();
        status.update_adapter(AdapterStatus {
            authentication: report,
            gateway: GatewayStatus::Connected("hub".into()),
            catalog_problem: None,
            pending: vec![PendingDevice {
                did: "group.1".into(),
                name: "Reading lamp".into(),
                reason: "cloud subscription pending".into(),
                group: true,
            }],
            routing: Default::default(),
            devices: vec![
                DeviceStatus {
                    did: "one".into(),
                    usable_control_paths: vec![PathKind::Local],
                    selected_control: Some(PathKind::Local),
                    notification_source: Some(PathKind::Local),
                    unavailable_reason: None,
                    sync_limited_reason: None,
                    last_switch_reason: None,
                    commands: Vec::new(),
                },
                DeviceStatus {
                    did: "two".into(),
                    usable_control_paths: Vec::new(),
                    selected_control: None,
                    notification_source: None,
                    unavailable_reason: Some("cloud offline".into()),
                    sync_limited_reason: None,
                    last_switch_reason: None,
                    commands: Vec::new(),
                },
            ],
        });
        let group = handle_line(&registry, &store.devices(), &status, "status group.1", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(
            group
                .contains("group.1 (Reading lamp): group; members: one (Lamp one), two (Lamp two)"),
            "{group}"
        );
        assert!(
            group.contains("pending group (cloud subscription pending)"),
            "{group}"
        );
        assert!(group.contains("one routing:"), "{group}");
        assert!(group.contains("two routing:"), "{group}");
        assert!(
            group.contains("Control unavailable: cloud offline."),
            "{group}"
        );
        let member = handle_line(&registry, &store.devices(), &status, "status one", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(
            member.contains("Group: group.1 (Reading lamp)."),
            "{member}"
        );
        assert!(member.contains("one routing:"), "{member}");
        assert!(!member.contains("two routing:"), "{member}");
        assert!(!member.contains("Control unavailable"), "{member}");
    })
}

#[test]
fn unpublished_numeric_group_status_and_commands_use_the_pending_diagnostic() {
    use migate::xiaomi::adapter::*;
    block_on(async {
        let (_dir, store, status, registry, mut receivers) = group_fixture();
        let report = AuthService::new(store.xiaomi(), CloudClient::new().unwrap())
            .local_status()
            .unwrap();
        status.update_adapter(AdapterStatus {
            authentication: report,
            gateway: GatewayStatus::Connected("hub".into()),
            catalog_problem: None,
            pending: vec![
                PendingDevice {
                    did: "43".into(),
                    name: "Numeric group".into(),
                    reason: "group 43 waits for member descriptions: two".into(),
                    group: true,
                },
                PendingDevice {
                    did: "three".into(),
                    name: "Third light".into(),
                    reason: "description read failed".into(),
                    group: false,
                },
            ],
            routing: Default::default(),
            devices: Vec::new(),
        });
        let list = handle_line(&registry, &store.devices(), &status, "list", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(
            list.contains(
                "43 (Numeric group): pending group (group 43 waits for member descriptions: two)"
            ),
            "{list}"
        );
        let group = handle_line(&registry, &store.devices(), &status, "status 43", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(
            group.contains(
                "43 (Numeric group): pending group (group 43 waits for member descriptions: two)"
            ),
            "{group}"
        );
        assert!(!group.contains("Unknown device DID."), "{group}");
        let device = handle_line(&registry, &store.devices(), &status, "status three", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(
            device.contains("three (Third light): pending (description read failed)"),
            "{device}"
        );
        assert!(!device.contains("Unknown device DID."), "{device}");
        for line in ["on 43", "off 43"] {
            let output = handle_line(&registry, &store.devices(), &status, line, 0)
                .await
                .unwrap()
                .unwrap();
            assert!(output.contains("waiting to apply"), "{line}: {output}");
            assert!(output.contains("not published"), "{line}: {output}");
            assert!(!output.contains("command accepted"), "{line}: {output}");
            for receiver in &mut receivers {
                assert!(poll_once(receiver.next()).await.is_none());
            }
        }
    })
}

fn persisted_group_fixture() -> (
    tempfile::TempDir,
    Store,
    AuthStatus,
    DeviceRegistry,
    Vec<CommandReceiver>,
) {
    use migate::{
        device::{Composition, ConfigurationScope},
        storage::CachedPhysicalDevice,
        xiaomi::{capability::parse_device_for_model, cloud::group_id},
    };
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let report = AuthService::new(store.xiaomi(), CloudClient::new().unwrap())
        .local_status()
        .unwrap();
    let status = AuthStatus::new(report, "/bin/migate".into(), directory.path().into());
    let registry = DeviceRegistry::new();
    let scope = ConfigurationScope {
        uid: "uid".into(),
        home_id: "local".into(),
        group_id: group_id("uid", "local"),
    };
    let mut receivers = Vec::new();
    for (did, label) in [("one", "Lamp one"), ("two", "Lamp two")] {
        let model = "yeelink.light.light3";
        let instance: serde_json::Value =
            serde_json::from_str(include_str!("fixtures/miot/yeelink-light-light3.json")).unwrap();
        let cached = CachedPhysicalDevice {
            uid: "uid".into(),
            home_id: "local".into(),
            group_id: group_id("uid", "local"),
            model: model.into(),
            instance_type: instance["type"].as_str().unwrap().into(),
            device: parse_device_for_model(DeviceKey::new("xiaomi", did), model, &instance)
                .unwrap(),
        };
        store.devices().publish_cached(&cached, label).unwrap();
        let stored = store
            .devices()
            .load_published(&DeviceKey::new("xiaomi", did))
            .unwrap()
            .unwrap();
        let (physical, mut consumers) = PhysicalDevice::new(stored.description());
        physical.set_availability(Availability::Available);
        registry.insert(physical);
        receivers.append(&mut consumers);
    }
    let group = Composition {
        key: DeviceKey::new("xiaomi", "group.1"),
        label: "Committed lamp".into(),
        members: vec![
            DeviceKey::new("xiaomi", "one"),
            DeviceKey::new("xiaomi", "two"),
        ],
    };
    let devices = store.devices();
    registry
        .stage_configuration(scope, vec![group.clone()], Vec::new())
        .unwrap();
    devices
        .apply_pending_configuration(&registry, |_| Ok((Vec::new(), Vec::new())))
        .unwrap();
    store
        .devices()
        .set_group_label(&group.key, "Saved Home lamp")
        .unwrap();
    (directory, store, status, registry, receivers)
}

#[test]
fn pending_group_names_keep_the_saved_identity_after_revocation_and_storage_failure_propagates() {
    use migate::{
        device::ConfigurationScope,
        xiaomi::{adapter::*, cloud::group_id},
    };
    block_on(async {
        let (_dir, store, status, registry, _receivers) = persisted_group_fixture();
        // Revoke the stored relation as well, keeping the reserved identity as
        // a real unpublished group, and publish the returned confirmed version.
        let scope = ConfigurationScope {
            uid: "uid".into(),
            home_id: "local".into(),
            group_id: group_id("uid", "local"),
        };
        let devices = store.devices();
        registry
            .stage_configuration(scope, Vec::new(), Vec::new())
            .unwrap();
        devices
            .apply_pending_configuration(&registry, |_| Ok((Vec::new(), Vec::new())))
            .unwrap();
        let report = AuthService::new(store.xiaomi(), CloudClient::new().unwrap())
            .local_status()
            .unwrap();
        status.update_adapter(AdapterStatus {
            authentication: report,
            gateway: GatewayStatus::Connected("hub".into()),
            catalog_problem: None,
            pending: vec![PendingDevice {
                did: "group.1".into(),
                name: "Cloud rename".into(),
                reason: "group group.1 was removed from the selected home".into(),
                group: true,
            }],
            routing: Default::default(),
            devices: Vec::new(),
        });
        let list = handle_line(&registry, &store.devices(), &status, "list", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(
            list.contains("group.1 (Saved Home lamp): pending group"),
            "{list}"
        );
        assert!(!list.contains("Cloud rename"), "{list}");
        let selected = handle_line(&registry, &store.devices(), &status, "status group.1", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(
            selected.contains("group.1 (Saved Home lamp): pending group"),
            "{selected}"
        );
        assert!(!selected.contains("Cloud rename"), "{selected}");

        // A malformed saved label still propagates instead of being hidden.
        rusqlite::Connection::open(store.path())
            .unwrap()
            .execute(
                "UPDATE group_identity SET label=?3 WHERE source=?1 AND device_id=?2",
                rusqlite::params!["xiaomi", "group.1", "x".repeat(33)],
            )
            .unwrap();
        let error = handle_line(&registry, &store.devices(), &status, "list", 0)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("32"), "{error}");
        assert!(
            handle_line(&registry, &store.devices(), &status, "status group.1", 0)
                .await
                .is_err()
        );
    })
}
