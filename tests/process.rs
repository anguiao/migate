#[path = "support/process.rs"]
mod process_support;
use migate::storage::{TokenSet, XiaomiRecord};
use process_support::{POLL_INTERVAL, Stream, TIMEOUT, TestProcess};
use rusqlite::Connection;
use std::{process::Command, thread, time::Instant};

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
    assert!(stdout.contains("Xiaomi: not signed in (cn).\n"), "{stdout}");
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
            oauth_client_uuid: "550e8400-e29b-41d4-a716-446655440000".into(),
            redirect_uri:
                "http://homeassistant.local:8123/api/webhook/0123456789abcdef0123456789abcdef"
                    .into(),
            tokens: TokenSet {
                access_token: "access".into(),
                refresh_token: "refresh".into(),
                expires_at: 2_000_000_000,
                refresh_at: 1_900_000_000,
            },
            virtual_did: "1".into(),
            private_key_pem: "broken-key".into(),
            certificate_pem: "broken-certificate".into(),
        })
        .unwrap();
    Connection::open(store.path())
        .unwrap()
        .execute("UPDATE xiaomi_auth SET region = 'us'", [])
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

#[test]
fn login_explains_manual_callback_and_eof_fails_without_matter() {
    let directory = tempfile::tempdir().unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_migate"))
        .arg("--data-dir")
        .arg(directory.path())
        .args(["auth", "login"])
        .output()
        .unwrap();
    let stdout = String::from_utf8(result.stdout).unwrap();
    let stderr = String::from_utf8(result.stderr).unwrap();
    assert!(!result.status.success());
    assert!(
        stdout.contains("https://account.xiaomi.com/oauth2/authorize?"),
        "{stdout}"
    );
    assert!(
        stdout.contains("Open this URL in a browser and authorize the HA application."),
        "{stdout}"
    );
    assert!(
        stdout.contains("The final homeassistant.local page may be unreachable."),
        "{stdout}"
    );
    assert!(
        stdout.contains("Paste the complete address-bar URL containing code and state:"),
        "{stdout}"
    );
    assert!(
        stderr.contains("Login cancelled before a callback URL was received"),
        "{stderr}"
    );
    assert!(!stdout.contains("Manual pairing code"), "{stdout}");
    assert!(!stderr.contains("bridge_id="), "{stderr}");
}

#[test]
fn bad_login_callback_fails_without_echoing_sensitive_input() {
    let directory = tempfile::tempdir().unwrap();
    let secret = "sensitive-code-and-state";
    let mut process = TestProcess::spawn(
        Command::new(env!("CARGO_BIN_EXE_migate"))
            .arg("--data-dir")
            .arg(directory.path())
            .args(["auth", "login"]),
    );
    process.send(&format!(
        "http://homeassistant.local:8123/wrong?code={secret}&state={secret}\n"
    ));
    process.close_input();
    let deadline = Instant::now() + TIMEOUT;
    let status = loop {
        if let Some(status) = process.child.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "bad callback did not exit");
        thread::sleep(POLL_INTERVAL);
    };
    let (stdout, stderr) = process.output();
    assert!(!status.success());
    assert!(!stdout.contains(secret), "{stdout}");
    assert!(!stderr.contains(secret), "{stderr}");
}

#[test]
fn ctrl_c_cancels_unfinished_login_with_failure() {
    let directory = tempfile::tempdir().unwrap();
    let mut process = TestProcess::spawn(
        Command::new(env!("CARGO_BIN_EXE_migate"))
            .arg("--data-dir")
            .arg(directory.path())
            .args(["auth", "login"]),
    );
    process.wait_for_output(Stream::Stdout, "Paste the complete address-bar URL");
    let (_, stderr) = process.interrupt_failure();
    assert!(stderr.contains("Login cancelled by Ctrl-C"), "{stderr}");
}

#[test]
fn a_data_directory_cannot_be_used_by_two_processes_and_exit_releases_it() {
    let directory = tempfile::tempdir().unwrap();
    let mut owner = TestProcess::spawn(
        Command::new(env!("CARGO_BIN_EXE_migate"))
            .arg("--data-dir")
            .arg(directory.path())
            .args(["auth", "login"]),
    );
    owner.wait_for_output(Stream::Stdout, "Paste the complete address-bar URL");
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
