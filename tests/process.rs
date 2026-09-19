use migate::storage::{TokenSet, XiaomiRecord};
use rusqlite::Connection;
use std::{
    fs,
    io::Write,
    net::UdpSocket,
    path::Path,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};
use tempfile::NamedTempFile;

const TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(20);

enum Stream {
    Stdout,
    Stderr,
}

struct TestProcess {
    child: Child,
    stdout: NamedTempFile,
    stderr: NamedTempFile,
}

impl TestProcess {
    fn bridge(directory: &Path) -> (Self, u16) {
        let mut process = Self::spawn(
            Command::new(env!("CARGO_BIN_EXE_migate"))
                .env("MIGATE_MATTER_PORT", "0")
                .arg("--data-dir")
                .arg(directory),
        );
        process.wait_for_output(Stream::Stderr, "mDNS services updated");
        let (_, stderr) = process.output();
        let port: u16 = stderr
            .lines()
            .find_map(|line| line.split_once("Matter UDP listening on port "))
            .unwrap()
            .1
            .parse()
            .unwrap();
        assert_ne!(port, 0);
        let mut address = rs_matter::transport::MATTER_SOCKET_BIND_ADDR;
        address.set_port(port);
        assert_eq!(
            UdpSocket::bind(address).unwrap_err().kind(),
            std::io::ErrorKind::AddrInUse,
        );

        // Resolve this process's service to verify the advertised port matches its socket.
        let service_id: u64 = stderr
            .lines()
            .find_map(|line| line.split_once("Registering mDNS service: Commissionable { id: "))
            .unwrap()
            .1
            .split(',')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        let mut lookup = Self::spawn(Command::new("/usr/bin/dns-sd").args([
            "-L",
            &format!("{service_id:016X}"),
            "_matterc._udp",
            "local.",
        ]));
        lookup.wait_for_output(Stream::Stdout, &format!(":{port} (interface"));
        (process, port)
    }

    fn spawn(command: &mut Command) -> Self {
        let stdout = NamedTempFile::new().unwrap();
        let stderr = NamedTempFile::new().unwrap();
        let child = command
            .stdin(Stdio::piped())
            .stdout(stdout.reopen().unwrap())
            .stderr(stderr.reopen().unwrap())
            .spawn()
            .unwrap();
        Self {
            child,
            stdout,
            stderr,
        }
    }

    fn send(&mut self, commands: &str) {
        let input = self.child.stdin.as_mut().unwrap();
        input.write_all(commands.as_bytes()).unwrap();
        input.flush().unwrap();
    }

    fn close_input(&mut self) {
        self.child.stdin.take();
    }

    fn output(&self) -> (String, String) {
        let read = |file: &NamedTempFile| {
            String::from_utf8_lossy(&fs::read(file.path()).unwrap()).into_owned()
        };
        (read(&self.stdout), read(&self.stderr))
    }

    fn wait_for_output(&mut self, stream: Stream, expected: &str) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let (stdout, stderr) = self.output();
            let status = self.child.try_wait().unwrap();
            assert!(
                status.is_none(),
                "unexpected early exit {status:?}: {stderr}"
            );
            let output = match stream {
                Stream::Stdout => &stdout,
                Stream::Stderr => &stderr,
            };
            if output.contains(expected) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {expected:?}\nstdout: {stdout}\nstderr: {stderr}"
            );
            thread::sleep(POLL_INTERVAL);
        }
    }

    fn interrupt(mut self) -> (String, String) {
        assert!(
            self.child.try_wait().unwrap().is_none(),
            "process exited before Ctrl-C: {}",
            self.output().1
        );
        assert!(
            Command::new("kill")
                .args(["-INT", &self.child.id().to_string()])
                .status()
                .unwrap()
                .success()
        );
        let deadline = Instant::now() + TIMEOUT;
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "Ctrl-C did not exit within {TIMEOUT:?}: {}",
                self.output().1
            );
            thread::sleep(POLL_INTERVAL);
        };
        let (stdout, stderr) = self.output();
        assert!(status.success(), "{stderr}");
        (stdout, stderr)
    }

    fn interrupt_failure(mut self) -> (String, String) {
        assert!(
            Command::new("kill")
                .args(["-INT", &self.child.id().to_string()])
                .status()
                .unwrap()
                .success()
        );
        let deadline = Instant::now() + TIMEOUT;
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "Ctrl-C did not stop auth command"
            );
            thread::sleep(POLL_INTERVAL);
        };
        let output = self.output();
        assert!(!status.success(), "auth command unexpectedly succeeded");
        output
    }
}

impl Drop for TestProcess {
    fn drop(&mut self) {
        // Cleanup must also run when an assertion panics or a wait times out.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

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
fn explicit_directory_eof_and_interrupt_keep_identity_and_protocol_state() {
    let peer_directory = tempfile::tempdir().unwrap();
    let (mut peer, peer_port) = TestProcess::bridge(peer_directory.path());
    let directory = tempfile::tempdir().unwrap();
    let mut identity = None;
    for close_input in [false, true] {
        let (mut process, port) = TestProcess::bridge(directory.path());
        assert_ne!(port, peer_port);
        process.send("devices\n");
        if close_input {
            process.close_input();
        }
        process.wait_for_output(Stream::Stdout, "No published features.");
        if close_input {
            process.wait_for_output(
                Stream::Stderr,
                "Terminal input ended; bridge is still running",
            );
        }
        let (output, errors) = process.interrupt();
        assert!(
            output
                .contains("Add MiGate in the Home app. The pairing window is open for 15 minutes."),
            "{output}"
        );
        assert!(output.contains("Manual pairing code: "), "{output}");
        assert!(errors.contains(directory.path().to_str().unwrap()));
        let store = migate::storage::Store::open(directory.path()).unwrap();
        let restored = store.load_identity().unwrap();
        assert!(
            store
                .matter()
                .contains(rs_matter::persist::BASIC_INFO_KEY)
                .unwrap()
        );
        if let Some(identity) = &identity {
            assert_eq!(identity, &restored);
        } else {
            identity = Some(restored);
        }
    }
    peer.send("devices\n");
    peer.wait_for_output(Stream::Stdout, "No published features.");
    peer.interrupt();
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
