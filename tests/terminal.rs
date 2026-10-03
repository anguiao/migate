use futures_lite::{future::block_on, io::Cursor};
use migate::{
    device::{
        Availability, Capability, Category, CommandError, CommandReceiver, Device,
        DeviceDescription, DeviceKey, Power,
    },
    storage::Store,
    terminal::{
        AuthStatus,
        bridge::{handle_line, run_input},
    },
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

fn test_device() -> (Device, CommandReceiver) {
    Device::new(DeviceDescription {
        key: DeviceKey::new("xiaomi", "test-outlet"),
        category: Category::Outlet,
        capabilities: vec![Capability::OnOff],
    })
}
#[test]
fn terminal_recovers_and_reads_shared_state() {
    block_on(async {
        let (device, mut receiver) = test_device();
        let (_directory, status) = auth_status();
        assert_eq!(handle_line(&device, &status, " \n", 0).await, None);
        assert!(
            handle_line(&device, &status, "status", 0)
                .await
                .unwrap()
                .starts_with("test-outlet: unknown\nDevice: initializing.")
        );
        assert!(
            handle_line(&device, &status, "on", 0)
                .await
                .unwrap()
                .contains("unavailable")
        );
        device.observe(false);
        device.set_availability(Availability::Available);
        let (result, _) =
            futures_lite::future::zip(handle_line(&device, &status, " on \n", 0), async {
                receiver.next().await.unwrap().accept().unwrap();
            })
            .await;
        assert_eq!(result.unwrap(), "test-outlet: command accepted");
        assert_eq!(device.snapshot().power, Power::On);
        for invalid in ["toggle", "ON", "on extra", "status extra", "off extra"] {
            assert!(
                handle_line(&device, &status, invalid, 0)
                    .await
                    .unwrap()
                    .contains("on, off, status, help")
            );
            assert_eq!(device.snapshot().power, Power::On);
        }
        device.observe(false);
        assert!(
            handle_line(&device, &status, "status", 0)
                .await
                .unwrap()
                .starts_with("test-outlet: off")
        );
        let (result, _) =
            futures_lite::future::zip(handle_line(&device, &status, "on", 0), async {
                receiver
                    .next()
                    .await
                    .unwrap()
                    .reject(CommandError::Rejected("denied".into()));
            })
            .await;
        assert!(result.unwrap().contains("command rejected: denied"));
        assert_eq!(device.snapshot().power, Power::Off);
        assert!(
            handle_line(&device, &status, "status", 0)
                .await
                .unwrap()
                .contains("Xiaomi: not signed in")
        );
    });
}
#[test]
fn input_recovers_from_unknown_commands_and_returns_at_eof() {
    block_on(async {
        let (device, mut receiver) = test_device();
        device.observe(false);
        device.set_availability(Availability::Available);
        let (_directory, status) = auth_status();
        let mut output = Vec::new();
        futures_lite::future::or(
            run_input(
                &device,
                &status,
                Cursor::new(b" \non\nbad\nstatus\noff"),
                &mut output,
            ),
            async {
                loop {
                    receiver.next().await.unwrap().accept().unwrap();
                }
            },
        )
        .await
        .unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.starts_with(
            "test-outlet: command accepted\nAvailable commands: on, off, status, help"
        ));
        assert!(output.contains("test-outlet: on\nDevice: available."));
        assert!(output.ends_with("test-outlet: command accepted\n"));
        assert_eq!(device.snapshot().power, Power::Off);
    });
}
#[test]
fn input_io_errors_propagate() {
    let (device, _receiver) = test_device();
    let (_directory, status) = auth_status();
    let mut output = Vec::new();
    let error = block_on(run_input(
        &device,
        &status,
        Cursor::new(b"\xff\n"),
        &mut output,
    ))
    .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
}
#[test]
fn output_io_errors_propagate() {
    let (device, _receiver) = test_device();
    let (_directory, status) = auth_status();
    let mut output = Cursor::new(&mut [0u8; 0][..]);
    assert!(
        block_on(run_input(
            &device,
            &status,
            Cursor::new(b"status\n"),
            &mut output
        ))
        .is_err()
    );
}
