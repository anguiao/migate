//! Run logging assertions in a child test process to isolate the global logger.
use crate::{
    storage::Store,
    terminal::AuthStatus,
    xiaomi::{auth::AuthService, cloud::CloudClient},
};

fn run_logger_child(test: &str) -> bool {
    const CHILD: &str = "MIGATE_TEST_LOGGER_CHILD";
    if std::env::var_os(CHILD).is_some() {
        return false;
    }
    let module = module_path!().split_once("::").unwrap().1;
    let name = format!("{module}::{test}");
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &name, "--nocapture"])
        .env(CHILD, "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "logger child failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("test result: ok. 1 passed; 0 failed; 0 ignored;"),
        "child test did not run exactly once:\n{stdout}"
    );
    true
}

#[test]
fn login_failure_preserves_the_report_in_logs_and_terminal() {
    if run_logger_child("login_failure_preserves_the_report_in_logs_and_terminal") {
        return;
    }
    use crate::{
        storage::RecoveryStop,
        xiaomi::{
            auth::{
                AuthReport, AuthenticationState, CertificateUpdate, FailureReason, LoginFailure,
            },
            certificate::CertificateValidity,
        },
    };
    use std::io::Write;

    let capture = tempfile::NamedTempFile::new().unwrap();
    env_logger::Builder::new()
        .filter_module("migate::terminal::status", log::LevelFilter::Info)
        .format(|buffer, record| writeln!(buffer, "{}|{}", record.level(), record.args()))
        .target(env_logger::Target::Pipe(Box::new(
            capture.reopen().unwrap(),
        )))
        .init();
    let mut report = AuthReport::for_test(
        AuthenticationState::Authenticated,
        Some(CertificateValidity {
            not_before: 1,
            not_after: 2,
        }),
        CertificateUpdate::Failed(FailureReason::InvalidCertificate),
        3,
    );
    report.login_failure = Some(LoginFailure {
        stage: "HA OAuth authorization",
        reason: FailureReason::RecoveryStopped(RecoveryStop::VerificationRequired),
    });
    let status = AuthStatus::new(report, "/bin/migate".into(), "/data".into());
    status.log(3);
    let output = std::fs::read_to_string(capture.path()).unwrap();
    let messages: Vec<_> = output
        .lines()
        .map(|line| line.split_once('|').unwrap().1)
        .collect();
    let terminal = status.render(3).replace("with:\n  ", "with: ");
    assert_eq!(messages, terminal.lines().collect::<Vec<_>>(), "{output}");
    for message in [
        "Login failed during HA OAuth authorization:",
        "Gateway certificate: expired",
        "Gateway certificate update failed:",
        "Complete the requested verification",
    ] {
        assert!(
            output
                .lines()
                .any(|line| line.starts_with(&format!("WARN|{message}"))),
            "{output}"
        );
    }
    assert!(output.contains("INFO|Sign in with: '/bin/migate' --data-dir '/data' auth login"));
    assert!(
        output.contains("INFO|Retry maintenance with: '/bin/migate' --data-dir '/data' auth check")
    );
}

#[test]
fn startup_login_command_is_a_single_prefixed_log_record() {
    if run_logger_child("startup_login_command_is_a_single_prefixed_log_record") {
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
            .unwrap()
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
        .unwrap()
        .local_status()
        .unwrap();
    let mut adapter = AdapterStatus {
        certificate_status: None,
        auth: report,
        gateway: GatewayStatus::Connected(gateway.clone()),
        catalog_problem: None,
        pending: Vec::new(),
        routing: Default::default(),
        devices: Vec::new(),
    };
    status.update_adapter(adapter.clone());
    adapter.devices.push(DeviceStatus {
        functions: vec![],
        lan: None,
        direct_unavailable_reason: Some("device model not supported".into()),
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
        standard_mode: None,
        shared_feedback: None,
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
        properties: super::command_properties(
            vec![(
                crate::xiaomi::capability::PropertyBinding { siid: 2, piid: 1 },
                true.into(),
            )],
            Some(PathKind::Local),
        ),
        steps: Vec::new(),
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
