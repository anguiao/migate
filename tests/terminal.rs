use futures_lite::{future::block_on, io::Cursor};
use migate::{
    device::Command,
    storage::Store,
    terminal::{
        AuthStatus,
        bridge::{handle_line, run_input},
    },
    virtual_device::VirtualLight,
    xiaomi::{auth::AuthService, cloud::CloudClient},
};

fn auth_status() -> (tempfile::TempDir, AuthStatus) {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let report = AuthService::new(store.xiaomi(), CloudClient::new().unwrap())
        .local_status()
        .unwrap();
    let status = AuthStatus::new(report, "/bin/migate".into(), directory.path().into());
    (directory, status)
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
    let (directory, status) = auth_status();
    status.log(0);

    let output = std::fs::read_to_string(capture.path()).unwrap();
    let lines: Vec<_> = output
        .lines()
        .filter(|line| line.contains(" migate::terminal::status] "))
        .collect();
    let expected_messages = [
        "Xiaomi: not signed in (cn).".to_owned(),
        "Gateway certificate: not prepared.".to_owned(),
        format!(
            "Sign in with: '/bin/migate' --data-dir '{}' auth login",
            directory.path().display()
        ),
    ];
    assert_eq!(lines.len(), expected_messages.len(), "{output}");
    for (line, message) in lines.iter().zip(expected_messages) {
        let (prefix, body) = line.split_once("] ").unwrap();
        assert!(prefix.starts_with('['), "{line}");
        assert!(prefix.contains(" INFO "), "{line}");
        assert_eq!(body, message);
    }
}

#[test]
fn terminal_recovers_and_reads_shared_state() {
    let light = VirtualLight::new();
    let (_directory, status) = auth_status();
    assert_eq!(handle_line(&light, &status, " \n", 0), None);
    assert_eq!(
        handle_line(&light, &status, " on \n", 0).unwrap(),
        "virtual-light-1: on"
    );
    for invalid in ["toggle", "ON", "on extra", "status extra", "off extra"] {
        assert!(
            handle_line(&light, &status, invalid, 0)
                .unwrap()
                .contains("on, off, status, help")
        );
        assert!(light.snapshot().power);
    }
    light.execute(Command::Off);
    assert_eq!(
        handle_line(&light, &status, "status", 0).unwrap(),
        "virtual-light-1: off"
    );
    assert_eq!(
        handle_line(&light, &status, "on", 0).unwrap(),
        "virtual-light-1: on"
    );
    assert_eq!(
        handle_line(&light, &status, "off", 0).unwrap(),
        "virtual-light-1: off"
    );
}

#[test]
fn input_recovers_from_unknown_commands_and_returns_at_eof() {
    let light = VirtualLight::new();
    let (_directory, status) = auth_status();
    let mut output = Vec::new();
    block_on(run_input(
        &light,
        &status,
        Cursor::new(b" \non\nbad\nstatus\noff"),
        &mut output,
    ))
    .unwrap();
    let output = String::from_utf8(output).unwrap();
    assert!(output.starts_with(
        "virtual-light-1: on\nAvailable commands: on, off, status, help\nXiaomi: not signed in (cn).\nGateway certificate: not prepared.\nSign in with:\n  '/bin/migate' --data-dir '"
    ));
    assert!(output.ends_with(" auth login\nvirtual-light-1: on\nvirtual-light-1: off\n"));
    assert!(!light.snapshot().power);
}

#[test]
fn input_io_errors_propagate() {
    let light = VirtualLight::new();
    let (_directory, status) = auth_status();
    let mut output = Vec::new();
    let error = block_on(run_input(
        &light,
        &status,
        Cursor::new(b"\xff\n"),
        &mut output,
    ))
    .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn output_io_errors_propagate() {
    let light = VirtualLight::new();
    let (_directory, status) = auth_status();
    let mut output = Cursor::new(&mut [0u8; 0][..]);
    assert!(
        block_on(run_input(
            &light,
            &status,
            Cursor::new(b"status\n"),
            &mut output
        ))
        .is_err()
    );
}
