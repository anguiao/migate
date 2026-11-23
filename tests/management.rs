mod support;

use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    os::unix::{
        fs::{FileTypeExt, MetadataExt, PermissionsExt, symlink},
        net::{SocketAddr, UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    process::Command,
    thread,
};
use support::process::{Stream, TIMEOUT, TestProcess};

const SOCKET: &str = "migate.sock";
const WAITING: &str = "Waiting for Xiaomi home confirmation.";

fn cli(directory: &Path, args: &[&str]) -> Command {
    let mut process = Command::new(env!("CARGO_BIN_EXE_migate"));
    process
        .env("RUST_LOG", "info")
        .arg("--data-dir")
        .arg(directory);
    process.args(args);
    process
}

fn bridge(directory: &Path) -> (TestProcess, TcpListener) {
    bridge_with_cli(directory, cli(directory, &[]))
}

fn bridge_with_cli(directory: &Path, mut command: Command) -> (TestProcess, TcpListener) {
    let store = migate::storage::Store::open(directory).unwrap();
    if store.xiaomi().load().unwrap().is_none() {
        support::xiaomi_fixture::seed(&store.xiaomi(), &support::xiaomi_fixture::record());
    }
    drop(store);
    // Leave the home lookup pending without contacting Xiaomi or starting Matter.
    let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = format!("http://{}", proxy.local_addr().unwrap());
    let mut process = TestProcess::spawn(
        command
            .env("https_proxy", &address)
            .env("HTTPS_PROXY", &address)
            .env("all_proxy", &address)
            .env("ALL_PROXY", &address)
            .env("no_proxy", "")
            .env("NO_PROXY", ""),
    );
    process.wait_for_output(Stream::Stderr, "Gateway: discovering hub.");
    (process, proxy)
}

fn long_directory(root: &Path) -> PathBuf {
    let path = root.join(format!("data directory {}", "long".repeat(32)));
    assert!(SocketAddr::from_pathname(path.join(SOCKET)).is_err());
    path
}

#[test]
fn a_long_data_directory_supports_the_bridge_attach_and_single_commands() {
    let root = tempfile::tempdir().unwrap();
    let directory = long_directory(root.path());
    let relative = directory.strip_prefix(root.path()).unwrap();
    let mut command = cli(relative, &[]);
    command.current_dir(root.path());
    let (bridge, _proxy) = bridge_with_cli(&directory, command);

    let socket = directory.join(SOCKET);
    let metadata = fs::symlink_metadata(&socket).unwrap();
    assert!(metadata.file_type().is_socket());
    assert_eq!(metadata.permissions().mode() & 0o777, 0o600);

    let second = cli(&directory, &[]).output().unwrap();
    assert!(!second.status.success());
    assert!(
        String::from_utf8(second.stderr)
            .unwrap()
            .contains(" attach")
    );
    assert_eq!(fs::symlink_metadata(&socket).unwrap().ino(), metadata.ino());

    let result = cli(&directory, &["status"]).output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(String::from_utf8(result.stdout).unwrap().contains(WAITING));

    let mut attach = TestProcess::spawn(cli(relative, &["attach"]).current_dir(root.path()));
    attach.send("status\nhelp\n");
    attach.close_input();
    let (exit, output, error) = attach.wait_for_exit(TIMEOUT);
    assert!(exit.success(), "{error}");
    assert!(output.find(WAITING).unwrap() < output.find("Available commands:").unwrap());

    bridge.stop("-TERM");
    assert!(!socket.exists());
}

#[test]
fn offline_long_paths_preserve_stale_sockets_and_allow_standalone_auth() {
    let root = tempfile::tempdir().unwrap();
    let directory = long_directory(root.path());
    let command = |args: &[&str]| cli(&directory, args).output().unwrap();
    let assert_offline = |args| {
        let result = command(args);
        assert!(!result.status.success());
        assert!(result.stdout.is_empty());
        let error = String::from_utf8(result.stderr).unwrap();
        assert!(error.contains("No MiGate bridge is running"), "{error}");
    };
    assert_offline(&["attach"]);
    assert!(!directory.exists());

    fs::create_dir(&directory).unwrap();
    assert_offline(&["status"]);
    assert!(fs::read_dir(&directory).unwrap().next().is_none());

    // Construct an ordinary Unix socket at the long path without using MiGate.
    let alias = tempfile::tempdir_in("/tmp").unwrap();
    let short = alias.path().join("data");
    symlink(&directory, &short).unwrap();
    drop(UnixListener::bind(short.join(SOCKET)).unwrap());
    drop(alias);
    let socket = directory.join(SOCKET);
    let original = fs::symlink_metadata(&socket).unwrap();
    assert!(original.file_type().is_socket());

    assert_offline(&["status"]);
    assert_offline(&["attach"]);
    assert!(!directory.join("state.db").exists());
    assert!(!directory.join("instance.lock").exists());

    let result = command(&["auth", "logout"]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(result.stdout, b"Xiaomi credentials removed.\n");
    assert!(directory.join("state.db").is_file());
    assert_eq!(fs::symlink_metadata(&socket).unwrap().ino(), original.ino());
}

#[test]
fn offline_commands_do_not_create_a_directory() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("not running");
    for args in [["attach"], ["status"]] {
        let result = cli(&directory, &args).output().unwrap();
        assert!(!result.status.success());
        assert!(result.stdout.is_empty());
        let error = String::from_utf8(result.stderr).unwrap();
        assert!(error.contains("No MiGate bridge is running"), "{error}");
        assert!(error.contains(directory.to_str().unwrap()), "{error}");
        assert!(error.contains("--data-dir"), "{error}");
        assert!(!directory.exists());
    }
}

#[test]
fn two_cli_clients_share_the_preparing_bridge_and_keep_sessions_independent() {
    let directory = tempfile::tempdir().unwrap();
    let (mut bridge, _proxy) = bridge(directory.path());
    let mut first = TestProcess::spawn(&mut cli(directory.path(), &["attach"]));
    let mut second = TestProcess::spawn(&mut cli(directory.path(), &["attach"]));

    first.send("this-is-invalid\nstatus\n");
    first.wait_for_output(Stream::Stdout, "Invalid command usage.");
    first.wait_for_output(Stream::Stdout, WAITING);
    second.send("help\n");
    second.wait_for_output(Stream::Stdout, "Available commands:");
    let (out, _) = second.output();
    assert!(!out.contains("Invalid command usage."), "{out}");

    let mut control = TestProcess::spawn(&mut cli(directory.path(), &["on", "outlet", "outlet:2"]));
    control.close_input();
    let (failed, out, err) = control.wait_for_exit(TIMEOUT);
    assert!(!failed.success(), "{out}\n{err}");
    assert_eq!(out, format!("{WAITING}\n"));

    let result = cli(directory.path(), &["status", "outlet"])
        .output()
        .unwrap();
    assert!(result.status.success(), "{:?}", result.stderr);
    assert!(String::from_utf8(result.stdout).unwrap().contains(WAITING));

    for words in [&["pairing"][..], &["pairing", "open"][..]] {
        let result = cli(directory.path(), words).output().unwrap();
        assert!(!result.status.success());
        assert_eq!(
            String::from_utf8(result.stdout).unwrap(),
            format!("{WAITING}\n")
        );
    }

    let result = cli(directory.path(), &[]).output().unwrap();
    assert!(!result.status.success());
    assert!(
        String::from_utf8(result.stderr)
            .unwrap()
            .contains(" attach")
    );
    first.interrupt();
    second.close_input();
    assert!(second.wait_for_exit(TIMEOUT).0.success());
    bridge.send("status\n");
    bridge.wait_for_output(Stream::Stdout, WAITING);
    bridge.close_input();
    bridge.wait_for_output(
        Stream::Stderr,
        "Terminal input ended; bridge is still running",
    );
    assert!(
        cli(directory.path(), &["status"])
            .output()
            .unwrap()
            .status
            .success()
    );
    bridge.stop("-TERM");
    assert!(!directory.path().join(SOCKET).exists());
}

#[test]
fn the_next_lock_owner_recovers_a_socket_left_by_a_crash() {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join(SOCKET);
    let (mut owner, _proxy) = bridge(directory.path());
    let metadata = fs::symlink_metadata(&socket).unwrap();
    assert!(metadata.file_type().is_socket());
    assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
    owner.child.kill().unwrap();
    assert!(!owner.wait_for_exit(TIMEOUT).0.success());
    assert!(socket.exists());

    let result = cli(directory.path(), &["status"]).output().unwrap();
    assert!(!result.status.success());
    assert!(
        String::from_utf8(result.stderr)
            .unwrap()
            .contains("No MiGate bridge is running")
    );
    assert!(socket.exists());

    let (owner, _proxy) = bridge(directory.path());
    assert!(
        cli(directory.path(), &["status"])
            .output()
            .unwrap()
            .status
            .success()
    );
    owner.interrupt();
    assert!(!socket.exists());
}

#[test]
fn a_conflicting_socket_path_is_reported_and_preserved() {
    let root = tempfile::tempdir().unwrap();
    let long = long_directory(root.path());
    fs::create_dir(&long).unwrap();
    for directory in [root.path(), &long] {
        let socket = directory.join(SOCKET);
        fs::write(&socket, b"belongs to the user").unwrap();
        for args in [&[][..], &["status"][..], &["auth", "logout"][..]] {
            let result = cli(directory, args).output().unwrap();
            assert!(!result.status.success());
            assert!(
                String::from_utf8(result.stderr)
                    .unwrap()
                    .contains("Management socket path conflict")
            );
            assert_eq!(fs::read(&socket).unwrap(), b"belongs to the user");
        }
    }
}

#[test]
fn inaccessible_socket_paths_report_permissions_errors() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join(SOCKET);
    let _socket = UnixListener::bind(&path).unwrap();
    let permissions = fs::metadata(directory.path()).unwrap().permissions();
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o000)).unwrap();
    if path.exists() {
        fs::set_permissions(directory.path(), permissions).unwrap();
        return;
    }
    let results =
        [&["status"][..], &["auth", "logout"]].map(|args| cli(directory.path(), args).output());
    fs::set_permissions(directory.path(), permissions).unwrap();
    for result in results {
        let result = result.unwrap();
        assert!(!result.status.success());
        let error = String::from_utf8(result.stderr).unwrap();
        assert!(
            error.contains("Failed to connect to the management socket")
                && error.contains("Permission denied"),
            "{error}"
        );
    }
    assert!(!directory.path().join("instance.lock").exists());
    assert!(!directory.path().join("state.db").exists());
}

#[test]
fn an_incompatible_peer_reports_a_protocol_error_without_opening_storage() {
    let directory = tempfile::tempdir().unwrap();
    let listener = UnixListener::bind(directory.path().join(SOCKET)).unwrap();
    let peer = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket.write_all(b"unrelated protocol\n").unwrap();
    });
    let process = TestProcess::spawn(&mut cli(directory.path(), &["status"]));
    let (status, output, error) = process.wait_for_exit(TIMEOUT);
    assert!(!status.success());
    assert!(output.is_empty(), "{output}");
    assert!(error.contains("Management protocol error"), "{error}");
    peer.join().unwrap();
    assert!(!directory.path().join("instance.lock").exists());
    assert!(!directory.path().join("state.db").exists());
}

#[test]
fn loss_of_the_connection_after_submission_reports_an_unknown_result() {
    let directory = tempfile::tempdir().unwrap();
    let listener = UnixListener::bind(directory.path().join(SOCKET)).unwrap();
    let peer = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket.write_all(b"migate/1\n").unwrap();
        socket.set_read_timeout(Some(TIMEOUT)).unwrap();
        let mut request = String::new();
        BufReader::new(socket).read_line(&mut request).unwrap();
        assert_eq!(request.trim_end(), r#"["on","outlet","outlet:2"]"#);
    });
    let process = TestProcess::spawn(&mut cli(directory.path(), &["on", "outlet", "outlet:2"]));
    let (status, output, error) = process.wait_for_exit(TIMEOUT);
    assert!(!status.success());
    assert!(output.is_empty(), "{output}");
    assert!(error.contains("command result is unknown"), "{error}");
    assert!(error.contains("status DID"), "{error}");
    peer.join().unwrap();
    assert!(!directory.path().join("instance.lock").exists());
}

fn read_auth_request(listener: &UnixListener, command: &str) -> UnixStream {
    let (mut socket, _) = listener.accept().unwrap();
    socket.set_read_timeout(Some(TIMEOUT)).unwrap();
    socket.write_all(b"migate/1\n").unwrap();
    let mut request = String::new();
    BufReader::new(&mut socket).read_line(&mut request).unwrap();
    assert_eq!(request.trim_end(), format!(r#"["auth","{command}"]"#));
    socket
}

#[test]
fn auth_commands_return_the_online_result_while_the_directory_is_locked() {
    let directory = tempfile::tempdir().unwrap();
    let _lock = migate::storage::InstanceLock::acquire(directory.path()).unwrap();
    let listener = UnixListener::bind(directory.path().join(SOCKET)).unwrap();
    let commands = [("login", true), ("check", false), ("logout", true)];
    let peer = thread::spawn(move || {
        for (command, success) in commands {
            let mut socket = read_auth_request(&listener, command);
            writeln!(socket, r#"{{"output":"online {command}\n"}}"#).unwrap();
            writeln!(socket, r#"{{"done":{success}}}"#).unwrap();
        }
    });
    for (command, success) in commands {
        let process = TestProcess::spawn(&mut cli(directory.path(), &["auth", command]));
        let (status, output, error) = process.wait_for_exit(TIMEOUT);
        assert_eq!(output, format!("online {command}\n"), "{error}");
        assert_eq!(status.success(), success, "{output}{error}");
        assert!(error.is_empty(), "{error}");
    }
    peer.join().unwrap();
    assert!(!directory.path().join("state.db").exists());
}

#[test]
fn online_login_streams_output_and_interrupt_closes_the_socket() {
    let directory = tempfile::tempdir().unwrap();
    let _lock = migate::storage::InstanceLock::acquire(directory.path()).unwrap();
    let listener = UnixListener::bind(directory.path().join(SOCKET)).unwrap();
    let peer = thread::spawn(move || {
        let mut socket = read_auth_request(&listener, "login");
        socket
            .write_all(b"{\"output\":\"QR output\\n\"}\n{\"output\":\"Waiting for scan confirmation...\\n\"}\n")
            .unwrap();
        assert_eq!(socket.read(&mut [0]).unwrap(), 0);
        listener.set_nonblocking(true).unwrap();
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    });
    let mut process = TestProcess::spawn(&mut cli(directory.path(), &["auth", "login"]));
    process.wait_for_output(Stream::Stdout, "Waiting for scan confirmation...");
    let (output, error) = process.interrupt_failure();
    assert_eq!(output, "QR output\nWaiting for scan confirmation...\n");
    assert!(error.contains("Ctrl-C"), "{error}");
    peer.join().unwrap();
    assert!(!directory.path().join("state.db").exists());
}

#[test]
fn auth_reports_a_locked_directory_with_no_management_socket() {
    let directory = tempfile::tempdir().unwrap();
    let _lock = migate::storage::InstanceLock::acquire(directory.path()).unwrap();
    let process = TestProcess::spawn(&mut cli(directory.path(), &["auth", "logout"]));
    let (status, output, error) = process.wait_for_exit(TIMEOUT);
    assert!(!status.success());
    assert!(output.is_empty(), "{output}");
    assert!(
        error
            .contains("Another MiGate process is using this data directory. Please retry shortly."),
        "{error}"
    );
    assert!(!directory.path().join("state.db").exists());
}

#[test]
fn auth_rejects_protocol_errors_without_opening_storage() {
    for bad_greeting in [true, false] {
        let directory = tempfile::tempdir().unwrap();
        let listener = UnixListener::bind(directory.path().join(SOCKET)).unwrap();
        let peer = thread::spawn(move || {
            if bad_greeting {
                let (mut socket, _) = listener.accept().unwrap();
                socket.write_all(b"unrelated protocol\n").unwrap();
            } else {
                let mut socket = read_auth_request(&listener, "logout");
                socket.write_all(b"invalid frame\n").unwrap();
            }
        });
        let process = TestProcess::spawn(&mut cli(directory.path(), &["auth", "logout"]));
        let (status, output, error) = process.wait_for_exit(TIMEOUT);
        assert!(!status.success(), "{output}{error}");
        assert!(output.is_empty(), "{output}");
        assert!(error.contains("Management protocol error"), "{error}");
        peer.join().unwrap();
        assert!(!directory.path().join("instance.lock").exists());
        assert!(!directory.path().join("state.db").exists());
    }
}

#[test]
fn auth_connection_loss_after_submission_does_not_fall_back_or_retry() {
    let directory = tempfile::tempdir().unwrap();
    let listener = UnixListener::bind(directory.path().join(SOCKET)).unwrap();
    let peer = thread::spawn(move || {
        drop(read_auth_request(&listener, "logout"));
        listener
    });
    let process = TestProcess::spawn(&mut cli(directory.path(), &["auth", "logout"]));
    let (status, output, error) = process.wait_for_exit(TIMEOUT);
    assert!(!status.success(), "{output}{error}");
    assert!(output.is_empty(), "{output}");
    assert!(error.contains("command result is unknown"), "{error}");
    assert!(error.contains("auth check"), "{error}");
    let listener = peer.join().unwrap();
    listener.set_nonblocking(true).unwrap();
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    assert!(!directory.path().join("instance.lock").exists());
    assert!(!directory.path().join("state.db").exists());
}
