//! Isolated processes exercise the ordinary application lifecycle and real Matter sockets.
#[path = "../../../tests/support/process.rs"]
mod process_support;
use super::outlet;
use crate::xiaomi::adapter::tests::app_source::AppSource;
use process_support::{Stream, TestProcess};
use std::{fs, net::UdpSocket, path::Path, process::Command, time::Duration};

impl TestProcess {
    fn bridge(directory: &Path, initial: bool) -> (Self, u16) {
        let gate = directory.join("source-ready");
        if gate.exists() {
            fs::remove_file(&gate).unwrap();
        }
        let mut process = Self::spawn(helper_command(directory).env(
            "MIGATE_TEST_INITIAL_POWER",
            if initial { "on" } else { "off" },
        ));
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
        process.send("status\n");
        process.wait_for_output(Stream::Stdout, "test-outlet (Test Outlet): initializing\n");
        process.wait_for_output(Stream::Stdout, "  main (Main power): unknown");
        fs::write(&gate, b"ready").unwrap();
        process.wait_for_output(
            Stream::Stderr,
            &format!(
                "Fake source initial read: {}",
                if initial { "on" } else { "off" }
            ),
        );
        (process, port)
    }
}

#[test]
fn sigterm_stops_the_shared_runtime_and_releases_its_directory() {
    let directory = tempfile::tempdir().unwrap();
    let (process, _) = TestProcess::bridge(directory.path(), false);
    let (_, errors) = process.stop("-TERM");
    assert!(errors.contains("Fake source stopped"), "{errors}");
    let _lock = crate::storage::InstanceLock::acquire(directory.path()).unwrap();
}

#[test]
fn shared_runtime_eof_interrupt_and_restart_keep_identity_without_persisting_power() {
    let peer_directory = tempfile::tempdir().unwrap();
    let (mut peer, peer_port) = TestProcess::bridge(peer_directory.path(), false);
    let directory = tempfile::tempdir().unwrap();
    let mut identity = None;
    for (close_input, initial) in [(false, false), (true, true)] {
        let (mut process, port) = TestProcess::bridge(directory.path(), initial);
        assert_ne!(port, peer_port);
        process.send("status\non test-outlet\nstatus\n");
        if close_input {
            process.close_input();
        }
        process.wait_for_output(Stream::Stdout, "test-outlet: command accepted\n");
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
        assert!(
            output.contains(&format!(
                "main (Main power): {}",
                if initial { "on" } else { "off" }
            )),
            "{output}"
        );
        assert!(output.contains("main (Main power): on"), "{output}");
        assert!(errors.contains("Fake source stopped"), "{errors}");
        assert!(errors.contains(directory.path().to_str().unwrap()));
        let reopened = crate::storage::Store::open(directory.path()).unwrap();
        let restored = (
            reopened.load_identity().unwrap(),
            reopened
                .devices()
                .load_configuration()
                .unwrap()
                .device(&crate::device::DeviceKey::new("xiaomi", "test-outlet"))
                .unwrap()
                .clone(),
        );
        if let Some(identity) = &identity {
            assert_eq!(identity, &restored);
        } else {
            identity = Some(restored);
        }
    }
    peer.send("status\n");
    peer.wait_for_output(Stream::Stdout, "main (Main power): off\n");
    peer.interrupt();
}

#[test]
fn shared_runtime_recovers_corrupt_matter_settings_and_keeps_identity() {
    let directory = tempfile::tempdir().unwrap();
    let store = crate::storage::Store::open(directory.path()).unwrap();
    let identity = store.load_identity().unwrap();
    store
        .matter()
        .put(rs_matter::persist::BASIC_INFO_KEY, &[0xff, 0x11])
        .unwrap();
    let (mut process, _) = TestProcess::bridge(directory.path(), false);
    process.send("on test-outlet\n");
    process.wait_for_output(Stream::Stdout, "test-outlet: command accepted\n");
    let (_, stderr) = process.interrupt();
    assert!(
        stderr.contains("Ignoring the persisted value under key 0x100"),
        "{stderr}"
    );
    assert!(stderr.contains("Fake source stopped"), "{stderr}");
    assert_eq!(store.load_identity().unwrap(), identity);
    assert_ne!(
        store
            .matter()
            .get(rs_matter::persist::BASIC_INFO_KEY)
            .unwrap(),
        Some(vec![0xff, 0x11])
    );
}

fn helper_command(directory: &Path) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "app::lifecycle::tests::process::bridge_helper_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("MIGATE_TEST_HELPER_DIRECTORY", directory);
    command
}

// The ordinary test returns immediately; only the isolated test subprocess enables it.
#[test]
fn bridge_helper_child() {
    let Some(directory) = std::env::var_os("MIGATE_TEST_HELPER_DIRECTORY") else {
        return;
    };
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            use crate::{
                app::{BridgeOptions, run_xiaomi_bridge, shutdown_signal},
                device::{Availability, DeviceRequest},
                terminal::{self, AuthStatus},
                xiaomi::{auth::AuthService, cloud::CloudClient},
            };
            use futures_lite::{future, io::BufReader};
            env_logger::Builder::new()
                .filter_level(log::LevelFilter::Info)
                .target(env_logger::Target::Stderr)
                .init();
            let directory = std::path::PathBuf::from(directory);
            let _instance = crate::storage::InstanceLock::acquire(&directory).unwrap();
            let store = crate::storage::Store::open(&directory).unwrap();
            let identity = store.load_identity().unwrap();
            log::info!("Data directory: {}", directory.display());
            let report = AuthService::new(store.xiaomi(), CloudClient::new().unwrap())
                .local_status()
                .unwrap();
            let status = AuthStatus::new(report, "/bin/migate".into(), directory.clone());
            let AppSource {
                adapter,
                handle,
                physical,
                mut receivers,
            } = AppSource::new(&store, Some(outlet()));
            let source_device = physical.unwrap();
            let mut receiver = receivers.remove(0);
            let gate = directory.join("source-ready");
            let initial =
                std::env::var("MIGATE_TEST_INITIAL_POWER").is_ok_and(|value| value == "on");
            struct Guard;
            impl Drop for Guard {
                fn drop(&mut self) {
                    log::info!("Fake source stopped");
                }
            }
            let source = async move {
                let _guard = Guard;
                let _adapter = adapter;
                while !gate.try_exists()? {
                    async_io::Timer::after(Duration::from_millis(10)).await;
                }
                source_device.functions()[0].observe(initial);
                source_device.set_availability(Availability::Available);
                log::info!(
                    "Fake source initial read: {}",
                    if initial { "on" } else { "off" }
                );
                while let Some(operation) = receiver.next_operation().await {
                    let DeviceRequest::Power(request) = operation else {
                        panic!("unexpected light operation")
                    };
                    source_device.functions()[0].observe(request.target);
                    request.accept()?;
                }
                Ok::<(), crate::RuntimeError>(())
            };
            let interrupt = async {
                shutdown_signal().await?;
                Ok(())
            };
            let result = future::block_on(run_xiaomi_bridge(
                BridgeOptions {
                    store: &store,
                    identity: &identity,
                    status: &status,
                    port: 0,
                },
                async { Ok(handle) },
                source,
                BufReader::new(blocking::Unblock::new(std::io::stdin())),
                blocking::Unblock::new(std::io::stdout()),
                interrupt,
                |event| terminal::pairing::write_event(std::io::stdout().lock(), event),
            ));
            if let Err(error) = result {
                panic!("isolated bridge failed: {error}");
            }
        })
        .unwrap()
        .join()
        .unwrap();
}
