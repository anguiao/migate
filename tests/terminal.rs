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
            channel: "main".into(),
            inflight: None,
            last: None,
        }],
    });
    status.update_adapter(adapter.clone());
    adapter.devices[0].commands[0].last = Some(CommandStatus {
        id: 1,
        binding: migate::xiaomi::capability::PropertyBinding { siid: 2, piid: 1 },
        target: true,
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
