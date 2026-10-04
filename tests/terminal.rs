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
        store.devices().publish(physical.description()).unwrap();
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
        let physical = registry.get(&DeviceKey::new("xiaomi", "multi")).unwrap();
        physical.function("usb").unwrap().observe(true);
        store
            .devices()
            .set_device_label(&physical.description().key, "Saved Home name")
            .unwrap();
        store
            .devices()
            .set_function_label(
                physical.function("usb").unwrap().function_key(),
                "Saved USB",
            )
            .unwrap();
        let list = handle_line(&registry, &store.devices(), &status, "list", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(list.contains("Saved Home name"));
        assert!(list.contains("usb (Saved USB): on"));
        assert!(list.contains("one (name one)"));
        let selected = handle_line(&registry, &store.devices(), &status, "status multi", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(!selected.contains("one (name one)"));
        assert!(selected.contains("Xiaomi: not signed in"));
        registry.remove(&physical.description().key);
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
fn a_missing_published_identity_returns_a_storage_error() {
    block_on(async {
        let (_dir, store, status, registry, _receivers) = fixture();
        store
            .devices()
            .remove(&DeviceKey::new("xiaomi", "one"))
            .unwrap();
        let error = handle_line(&registry, &store.devices(), &status, "list", 0)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("no published identity"));
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
        let report = AuthService::new(store.xiaomi(), CloudClient::new().unwrap())
            .local_status()
            .unwrap();
        status.update_source(migate::xiaomi::source::SourceStatus {
            authentication: report,
            gateway: migate::xiaomi::source::GatewayStatus::Connected("hub".into()),
            catalog_problem: None,
            pending: vec![migate::xiaomi::source::PendingDevice {
                did: "multi".into(),
                name: "New cloud name".into(),
                reason: "subscription rejected".into(),
            }],
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
fn startup_login_command_is_a_single_prefixed_log_record() {
    let capture = tempfile::NamedTempFile::new().unwrap();
    env_logger::Builder::new()
        .filter_module("migate::terminal::status", log::LevelFilter::Info)
        .write_style(env_logger::WriteStyle::Never)
        .target(env_logger::Target::Pipe(Box::new(
            capture.reopen().unwrap(),
        )))
        .init();
    let (directory, _store, status, _registry, _receivers) = fixture();
    status.log(0);

    let output = std::fs::read_to_string(capture.path()).unwrap();
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
