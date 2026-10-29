//! Run logging assertions in a child test process to isolate the global logger.
use crate::{
    storage::Store,
    terminal::AuthStatus,
    xiaomi::{auth::AuthService, cloud::CloudClient},
};

#[test]
fn startup_login_command_is_a_single_prefixed_log_record() {
    if std::env::var_os("MIGATE_TEST_LOGGER_CHILD").is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "terminal::status::tests::logging::startup_login_command_is_a_single_prefixed_log_record",
                "--nocapture",
            ])
            .env("MIGATE_TEST_LOGGER_CHILD", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "logger child failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    use crate::xiaomi::adapter::*;
    let capture = tempfile::NamedTempFile::new().unwrap();
    env_logger::Builder::new()
        .filter_module("migate::terminal::status", log::LevelFilter::Info)
        .write_style(env_logger::WriteStyle::Never)
        .target(env_logger::Target::Pipe(Box::new(
            capture.reopen().unwrap(),
        )))
        .init();
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let status = AuthStatus::new(
        AuthService::new(store.xiaomi(), CloudClient::new().unwrap())
            .local_status()
            .unwrap(),
        "/bin/migate".into(),
        directory.path().into(),
    );
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
        functions: vec![],
        did: "one".into(),
        read_only: false,
        usable_control_paths: vec![PathKind::Local],
        selected_control: Some(PathKind::Local),
        usable_read_paths: Vec::new(),
        selected_read: None,
        physical_online: None,
        notification_source: Some(PathKind::Local),
        unavailable_reason: None,
        sync_limited_reason: None,
        battery_problem: None,
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
        binding: crate::xiaomi::capability::PropertyBinding { siid: 2, piid: 1 },
        target: crate::xiaomi::adapter::CommandTarget::Power(true),
        properties: vec![(
            crate::xiaomi::capability::PropertyBinding { siid: 2, piid: 1 },
            true.into(),
        )],
        fan_paths: Vec::new(),
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
