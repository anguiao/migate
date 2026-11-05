#[path = "support/process.rs"]
mod process_support;
use migate::storage::{TokenSet, XiaomiRecord};
use process_support::{Stream, TIMEOUT, TestProcess};
use rusqlite::Connection;
use std::{process::Command, thread};

#[test]
fn invalid_arguments_exit_without_starting_services() {
    let result = Command::new(env!("CARGO_BIN_EXE_migate"))
        .arg("--unknown")
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("Usage:"));
    assert!(result.stdout.is_empty());
}

#[test]
fn auth_arguments_require_the_global_option_first() {
    for args in [
        vec!["auth"],
        vec!["auth", "unknown"],
        vec!["auth", "check", "--data-dir", "late"],
    ] {
        let result = Command::new(env!("CARGO_BIN_EXE_migate"))
            .args(args)
            .output()
            .unwrap();
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains("Usage:"));
        assert!(result.stdout.is_empty());
    }
}

#[test]
fn every_auth_mode_requires_the_stored_bridge_identity() {
    for auth_command in ["login", "check", "logout"] {
        let directory = tempfile::tempdir().unwrap();
        let store = migate::storage::Store::open(directory.path()).unwrap();
        Connection::open(store.path())
            .unwrap()
            .execute("DELETE FROM identity", [])
            .unwrap();
        drop(store);
        let result = Command::new(env!("CARGO_BIN_EXE_migate"))
            .env("https_proxy", "http://127.0.0.1:9")
            .env("HTTPS_PROXY", "http://127.0.0.1:9")
            .env("no_proxy", "")
            .env("NO_PROXY", "")
            .arg("--data-dir")
            .arg(directory.path())
            .args(["auth", auth_command])
            .output()
            .unwrap();
        assert!(!result.status.success(), "{auth_command}");
        assert!(result.stdout.is_empty(), "{auth_command}");
        assert!(
            String::from_utf8_lossy(&result.stderr).contains("Failed to load identity"),
            "{auth_command}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
}

#[test]
fn auth_check_without_credentials_reports_both_states_and_quoted_login_command() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("data dir's");
    let result = Command::new(env!("CARGO_BIN_EXE_migate"))
        .arg("--data-dir")
        .arg(&directory)
        .args(["auth", "check"])
        .output()
        .unwrap();
    let stdout = String::from_utf8(result.stdout).unwrap();
    let stderr = String::from_utf8(result.stderr).unwrap();
    assert!(!result.status.success());
    assert!(
        stdout.contains("Xiaomi account: not signed in (cn).\n"),
        "{stdout}"
    );
    assert!(
        stdout.contains("Gateway certificate: not prepared.\n"),
        "{stdout}"
    );
    assert!(stdout.contains("'\"'\"'"), "shell quote missing: {stdout}");
    assert!(stdout.contains(" auth login"), "{stdout}");
    assert!(!stdout.contains("Manual pairing code"), "{stdout}");
    assert!(!stderr.contains("bridge_id="), "{stderr}");
    assert!(!stderr.contains("endpoint 0="), "{stderr}");
    assert!(!stderr.contains("Xiaomi:"), "{stderr}");
    assert!(!stderr.contains("Gateway certificate:"), "{stderr}");
}

#[test]
fn logout_is_idempotent_and_auth_modes_do_not_start_matter() {
    let directory = tempfile::tempdir().unwrap();
    let identity = migate::storage::Store::open(directory.path())
        .unwrap()
        .load_identity()
        .unwrap();
    for _ in 0..2 {
        let result = Command::new(env!("CARGO_BIN_EXE_migate"))
            .arg("--data-dir")
            .arg(directory.path())
            .args(["auth", "logout"])
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&result.stdout),
            "Xiaomi credentials removed.\n"
        );
        assert!(!String::from_utf8_lossy(&result.stderr).contains("bridge_id="));
    }
    assert_eq!(
        migate::storage::Store::open(directory.path())
            .unwrap()
            .load_identity()
            .unwrap(),
        identity
    );
}

#[test]
fn logout_clears_corrupt_credentials_and_preserves_identity_and_matter_data() {
    let directory = tempfile::tempdir().unwrap();
    let store = migate::storage::Store::open(directory.path()).unwrap();
    let identity = store.load_identity().unwrap();
    store.matter().put(7, b"paired").unwrap();
    store
        .xiaomi()
        .replace(&XiaomiRecord {
            uid: "uid".into(),
            region: "cn".into(),
            recovery: None,
            mijia: None,
            ha: Some(migate::storage::HaClient {
                client_uuid: "550e8400-e29b-41d4-a716-446655440000".into(),
                authorization: Some(migate::storage::HaAuthorization {
                    redirect_uri: "http://homeassistant.local:8123/api/webhook/0123456789abcdef0123456789abcdef".into(),
                    tokens: TokenSet {
                        access_token: "access".into(),
                        refresh_token: "refresh".into(),
                        expires_at: 2_000_000_000,
                        refresh_at: 1_900_000_000,
                    },
                }),
                stopped: false,
            }),
            hub: Some(migate::storage::HubIdentity {
                virtual_did: "1".into(),
                private_key_pem: "broken-key".into(),
                certificate_pem: Some("broken-certificate".into()),
            }),
        })
        .unwrap();
    Connection::open(store.path())
        .unwrap()
        .execute("UPDATE xiaomi_account SET region = 'us'", [])
        .unwrap();
    drop(store);

    let result = Command::new(env!("CARGO_BIN_EXE_migate"))
        .arg("--data-dir")
        .arg(directory.path())
        .args(["auth", "logout"])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let store = migate::storage::Store::open(directory.path()).unwrap();
    assert!(store.xiaomi().load().unwrap().is_none());
    assert_eq!(store.load_identity().unwrap(), identity);
    assert_eq!(
        store.matter().get(7).unwrap().as_deref(),
        Some(b"paired".as_slice())
    );
}

fn blocked_login(directory: &std::path::Path) -> TestProcess {
    use std::io::Read;
    use std::net::TcpListener;
    use std::sync::mpsc;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buffer = [0; 4096];
        let size = stream.read(&mut buffer).unwrap();
        sender
            .send(String::from_utf8_lossy(&buffer[..size]).into_owned())
            .unwrap();
        while stream.read(&mut buffer).unwrap_or(0) > 0 {}
    });
    let proxy = format!("http://{address}");
    let mut command = Command::new(env!("CARGO_BIN_EXE_migate"));
    command
        .arg("--data-dir")
        .arg(directory)
        .args(["auth", "login"])
        .env("https_proxy", &proxy)
        .env("HTTPS_PROXY", &proxy)
        .env("all_proxy", &proxy)
        .env("ALL_PROXY", &proxy)
        .env("no_proxy", "")
        .env("NO_PROXY", "");
    let mut process = TestProcess::spawn(&mut command);
    process.wait_for_output(Stream::Stdout, "Mijia session for device HTTP");
    let request = receiver.recv_timeout(TIMEOUT).unwrap();
    assert!(
        request.starts_with("CONNECT account.xiaomi.com:443"),
        "{request}"
    );
    process
}

#[test]
fn login_signals_cancel_the_isolated_pending_request_without_matter() {
    for signal in ["-INT", "-TERM"] {
        let directory = tempfile::tempdir().unwrap();
        let process = blocked_login(directory.path());
        let (status, stdout, stderr) = process.signal_failure(signal);
        assert!(!status.success());
        assert!(stderr.contains("Login cancelled by"), "{stderr}");
        assert!(
            stdout.contains("Xiaomi account: not signed in (cn)."),
            "{stdout}"
        );
        assert!(stdout.contains("Mijia session: not signed in."), "{stdout}");
        assert!(stdout.contains("HA OAuth: not signed in."), "{stdout}");
        assert!(
            stdout.contains("Gateway certificate: not prepared."),
            "{stdout}"
        );
        assert!(stdout.contains("auth login"), "{stdout}");
        assert!(!stdout.contains("Manual pairing code"));
        assert!(!stderr.contains("bridge_id="));
    }
}

#[test]
fn login_signal_reports_and_preserves_existing_credential_groups() {
    use migate::{
        storage::{AccountRecovery, HaAuthorization, HaClient, RecoveryStop},
        xiaomi::{
            auth::{AccountClientIdentity, AccountCredentials},
            mijia::MijiaSession,
        },
    };
    let directory = tempfile::tempdir().unwrap();
    let store = migate::storage::Store::open(directory.path()).unwrap();
    let previous = XiaomiRecord {
        uid: "uid".into(),
        region: "cn".into(),
        recovery: Some(AccountRecovery {
            credentials: AccountCredentials::new(
                "uid".into(),
                "cuser-secret".into(),
                "pass-secret".into(),
                AccountClientIdentity::new("device-id".into(), "agent".into()).unwrap(),
            )
            .unwrap(),
            stop: Some(RecoveryStop::VerificationRequired),
        }),
        mijia: Some(
            MijiaSession::new(
                "uid".into(),
                "service-secret".into(),
                "c2VjdXJpdHk=".into(),
                "agent".into(),
            )
            .unwrap(),
        ),
        ha: Some(HaClient {
            client_uuid: "550e8400-e29b-41d4-a716-446655440000".into(),
            authorization: Some(HaAuthorization {
                redirect_uri:
                    "http://homeassistant.local:8123/api/webhook/0123456789abcdef0123456789abcdef"
                        .into(),
                tokens: TokenSet {
                    access_token: "access-secret".into(),
                    refresh_token: "refresh-secret".into(),
                    expires_at: 2_000_000_000,
                    refresh_at: 1_900_000_000,
                },
            }),
            stopped: true,
        }),
        hub: None,
    };
    store.xiaomi().replace(&previous).unwrap();
    drop(store);
    let process = blocked_login(directory.path());
    let (status, stdout, stderr) = process.signal_failure("-TERM");
    assert!(!status.success());
    assert!(stderr.contains("Login cancelled by SIGTERM"), "{stderr}");
    assert!(stdout.contains("Xiaomi account: uid (cn)."), "{stdout}");
    assert!(stdout.contains("Mijia session: checking."), "{stdout}");
    assert!(stdout.contains("HA OAuth: sign-in required."), "{stdout}");
    assert!(
        stdout.contains("Gateway certificate: not prepared."),
        "{stdout}"
    );
    assert!(stdout.contains("Account recovery: stopped."), "{stdout}");
    assert!(
        stdout.contains("auth check") && stdout.contains("auth login"),
        "{stdout}"
    );
    assert!(
        !stdout.contains("Mijia session: available.") && !stdout.contains("HA OAuth: available.")
    );
    for secret in [
        "cuser-secret",
        "pass-secret",
        "service-secret",
        "access-secret",
        "refresh-secret",
    ] {
        assert!(!stdout.contains(secret) && !stderr.contains(secret));
    }
    let reopened = migate::storage::Store::open(directory.path()).unwrap();
    assert_eq!(reopened.xiaomi().load().unwrap(), Some(previous));
}

#[test]
fn a_data_directory_cannot_be_used_by_two_processes_and_exit_releases_it() {
    let directory = tempfile::tempdir().unwrap();
    let mut owner = blocked_login(directory.path());
    for arguments in [vec![], vec!["auth", "check"], vec!["auth", "logout"]] {
        let result = Command::new(env!("CARGO_BIN_EXE_migate"))
            .arg("--data-dir")
            .arg(directory.path())
            .args(arguments)
            .output()
            .unwrap();
        assert!(!result.status.success());
        assert!(result.stdout.is_empty());
        assert!(
            String::from_utf8_lossy(&result.stderr)
                .contains("Another MiGate process is using this data directory")
        );
    }
    // Even an ungraceful exit releases the OS lock; its file can remain in place.
    owner.child.kill().unwrap();
    owner.child.wait().unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_migate"))
        .arg("--data-dir")
        .arg(directory.path())
        .args(["auth", "logout"])
        .output()
        .unwrap();
    assert!(result.status.success());
    assert!(directory.path().join("instance.lock").is_file());
}

#[test]
fn log_filter_controls_diagnostics_without_changing_command_output() {
    let directory = tempfile::tempdir().unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_migate"))
        .env("RUST_LOG", "off")
        .arg("--data-dir")
        .arg(directory.path())
        .args(["auth", "logout"])
        .output()
        .unwrap();
    assert!(result.status.success());
    assert!(result.stderr.is_empty());
    assert!(!result.stdout.is_empty());
}

#[test]
fn test_process_is_reaped_when_an_assertion_panics() {
    let process = TestProcess::spawn(Command::new("sleep").arg("30"));
    let pid = process.child.id();
    let result = std::panic::catch_unwind(move || {
        let _process = process;
        panic!("simulated test failure");
    });
    assert!(result.is_err());
    let probe = Command::new("kill")
        .args(["-0", &pid.to_string()])
        .output()
        .unwrap();
    assert!(!probe.status.success(), "child survived test failure");
}

#[test]
fn production_without_credentials_fails_without_pairing_or_fake_device() {
    let directory = tempfile::tempdir().unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_migate"))
        .env("RUST_LOG", "off")
        .arg("--data-dir")
        .arg(directory.path())
        .output()
        .unwrap();
    assert!(!result.status.success());
    let stdout = String::from_utf8(result.stdout).unwrap();
    let stderr = String::from_utf8(result.stderr).unwrap();
    assert!(!stdout.contains("pairing code"), "{stdout}");
    assert!(stderr.contains("Xiaomi sign-in is required"), "{stderr}");
    assert!(stderr.contains("auth login"), "{stderr}");
    assert!(!stderr.contains("Matter UDP listening"), "{stderr}");
}
