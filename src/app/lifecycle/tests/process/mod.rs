//! Isolated processes exercise the ordinary application lifecycle and real Matter sockets.
mod advertisement;
#[cfg(target_os = "linux")]
mod avahi;

// This helper file is shared by the two test crates.
#[path = "../../../../../tests/support/process.rs"]
mod process_support;
use crate::app::tests::support::outlet;
use crate::xiaomi::adapter::tests::app_source::AppSource;
use advertisement::{
    COMMISSIONABLE_SERVICE, OPERATIONAL_SERVICE, assert_resolves, commissionable_name,
    store_test_fabric,
};
use process_support::{Stream, TestProcess};
use std::{
    fs,
    net::{SocketAddr, UdpSocket},
    path::Path,
    process::Command,
    time::Duration,
};

const MATTER_LISTENING: &str = "Matter UDP listening on port ";
const MDNS_UPDATED: &str = "mDNS services updated";

impl TestProcess {
    fn bridge(directory: &Path, initial: bool) -> Self {
        let gate = directory.join("source-ready");
        if gate.exists() {
            fs::remove_file(&gate).unwrap();
        }
        let reserved = UdpSocket::bind(matter_address(0)).unwrap();
        let port = reserved.local_addr().unwrap().port();
        let mut process = Self::spawn(&mut helper_command(directory, port, initial));

        // The source and terminal must make progress while Matter is gated.
        process.wait_for_output(Stream::Stderr, "Fake source awaiting initial read");
        process.send("status\n");
        process.wait_for_output(Stream::Stdout, "test-outlet (Test Outlet): initializing\n");
        process.wait_for_output(Stream::Stdout, "  outlet:2 (主电源): unknown");
        let (_, stderr) = process.output();
        assert!(!stderr.contains(MATTER_LISTENING), "{stderr}");
        // Retaining the port prevents any unrelated process from acquiring it;
        // an early Matter bind would cause the child to exit with an error.
        assert!(process.child.try_wait().unwrap().is_none(), "{stderr}");

        drop(reserved);
        fs::write(&gate, b"ready").unwrap();
        process.wait_for_output(
            Stream::Stderr,
            &format!(
                "Fake source initial read: {}",
                if initial { "on" } else { "off" }
            ),
        );
        let (listening_port, logs) = process.wait_for_advertisement(1);
        let (_, stderr) = process.output();
        assert!(
            stderr.find("Fake source initial read:").unwrap()
                < stderr.find(MATTER_LISTENING).unwrap(),
            "{stderr}"
        );
        assert_eq!(listening_port, port);
        assert_resolves(&commissionable_name(&logs), COMMISSIONABLE_SERVICE, port);
        process
    }

    fn wait_for_advertisement(&mut self, run: usize) -> (u16, String) {
        let stderr = self.wait_for_output_matching(Stream::Stderr, MDNS_UPDATED, |stderr| {
            stderr
                .split(MATTER_LISTENING)
                .nth(run)
                .is_some_and(|run| run.contains(MDNS_UPDATED))
        });
        let started = stderr.split(MATTER_LISTENING).nth(run).unwrap();
        let port = started.lines().next().unwrap().parse().unwrap();
        assert_ne!(port, 0);
        assert_eq!(
            UdpSocket::bind(matter_address(port)).unwrap_err().kind(),
            std::io::ErrorKind::AddrInUse,
        );
        (port, started.to_owned())
    }
}

fn matter_address(port: u16) -> SocketAddr {
    let mut address = rs_matter::transport::MATTER_SOCKET_BIND_ADDR;
    address.set_port(port);
    address
}

#[test]
fn runtime_rebuild_keeps_the_dynamic_port_and_updates_the_advertisement() {
    let directory = tempfile::tempdir().unwrap();
    let mut process = TestProcess::spawn(&mut ready_bridge_command(directory.path(), 0));
    let (port, initial) = process.wait_for_advertisement(1);
    let initial_name = commissionable_name(&initial);
    assert_resolves(&initial_name, COMMISSIONABLE_SERVICE, port);

    fs::write(directory.path().join("source-remove"), b"remove").unwrap();
    let (rebuilt_port, rebuilt) = process.wait_for_advertisement(2);
    assert_eq!(rebuilt_port, port);
    let rebuilt_name = commissionable_name(&rebuilt);
    assert_ne!(rebuilt_name, initial_name);
    let initial_id = u64::from_str_radix(&initial_name, 16).unwrap();
    assert!(rebuilt.contains(&format!(
        "Deregistering mDNS service: Commissionable {{ id: {initial_id},"
    )));
    assert_resolves(&rebuilt_name, COMMISSIONABLE_SERVICE, port);
    process.stop("-TERM");
}

#[test]
fn runtime_rebuild_keeps_the_restored_operational_registration() {
    let directory = tempfile::tempdir().unwrap();
    let store = crate::storage::Store::open(directory.path()).unwrap();
    let name = store_test_fabric(&store);
    drop(store);
    let mut process = TestProcess::spawn(&mut ready_bridge_command(directory.path(), 0));
    let (port, logs) = process.wait_for_advertisement(1);
    assert!(
        logs.contains("Registering mDNS service: Commissioned"),
        "{logs}"
    );
    assert_resolves(&name, OPERATIONAL_SERVICE, port);

    process.close_input();
    process.wait_for_output(
        Stream::Stderr,
        "Terminal input ended; bridge is still running",
    );
    fs::write(directory.path().join("source-remove"), b"remove").unwrap();
    let (rebuilt_port, logs) = process.wait_for_advertisement(2);
    assert_eq!(rebuilt_port, port);
    assert!(!logs.contains("Registering mDNS service:"), "{logs}");
    assert!(!logs.contains("Deregistering mDNS service:"), "{logs}");
    assert_resolves(&name, OPERATIONAL_SERVICE, port);
    process.stop("-TERM");
}

#[test]
fn occupied_matter_port_causes_a_diagnostic_exit() {
    let directory = tempfile::tempdir().unwrap();
    let reserved = UdpSocket::bind(matter_address(0)).unwrap();
    let port = reserved.local_addr().unwrap().port();
    let process = TestProcess::spawn(&mut ready_bridge_command(directory.path(), port));
    let (status, _, stderr) = process.wait_for_exit();
    assert!(!status.success());
    assert!(
        stderr.contains(&format!("Failed to bind Matter UDP port {port}")),
        "{stderr}"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn unavailable_system_bus_causes_a_diagnostic_exit() {
    let directory = tempfile::tempdir().unwrap();
    let mut command = ready_bridge_command(directory.path(), 0);
    command.env(
        "DBUS_SYSTEM_BUS_ADDRESS",
        format!("unix:path={}/missing-bus", directory.path().display()),
    );
    let (status, _, stderr) = TestProcess::spawn(&mut command).wait_for_exit();
    assert!(!status.success());
    assert!(
        stderr.contains("Failed to connect to the system D-Bus for Matter mDNS"),
        "{stderr}"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn unavailable_avahi_causes_a_diagnostic_exit() {
    let directory = tempfile::tempdir().unwrap();
    let mut command = ready_bridge_command(directory.path(), 0);
    let (_bus, address) = avahi::private_bus(directory.path());
    command.env("DBUS_SYSTEM_BUS_ADDRESS", &address);
    let (status, _, stderr) = TestProcess::spawn(&mut command).wait_for_exit();
    assert!(!status.success());
    assert!(
        stderr.contains("Failed to advertise Matter services with Avahi"),
        "{stderr}"
    );
}

#[test]
fn sigterm_stops_the_shared_runtime_and_releases_its_directory() {
    let directory = tempfile::tempdir().unwrap();
    let process = TestProcess::bridge(directory.path(), false);
    let (_, errors) = process.stop("-TERM");
    assert!(errors.contains("Fake source stopped"), "{errors}");
    let _lock = crate::storage::InstanceLock::acquire(directory.path()).unwrap();
}

#[test]
fn shared_runtime_eof_interrupt_and_restart_keep_identity_without_persisting_power() {
    let directory = tempfile::tempdir().unwrap();
    let mut identity = None;
    for (close_input, initial) in [(false, false), (true, true)] {
        let mut process = TestProcess::bridge(directory.path(), initial);
        process.send("status\non test-outlet outlet:2\nstatus\n");
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
                "outlet:2 (主电源): {}",
                if initial { "on" } else { "off" }
            )),
            "{output}"
        );
        assert!(output.contains("outlet:2 (主电源): on"), "{output}");
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
    let mut process = TestProcess::bridge(directory.path(), false);
    process.send("on test-outlet outlet:2\n");
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

fn ready_bridge_command(directory: &Path, port: u16) -> Command {
    crate::storage::Store::open(directory).unwrap();
    fs::write(directory.join("source-ready"), b"ready").unwrap();
    helper_command(directory, port, false)
}

fn helper_command(directory: &Path, port: u16, initial: bool) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "--ignored",
            "app::lifecycle::tests::process::bridge_helper_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("MIGATE_TEST_HELPER_DIRECTORY", directory)
        .env(
            "MIGATE_TEST_INITIAL_POWER",
            if initial { "on" } else { "off" },
        )
        .env("MIGATE_TEST_MATTER_PORT", port.to_string());
    command
}

#[test]
#[ignore = "subprocess entry point used by the bridge lifecycle tests"]
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
            log::info!("Fake source process starting");
            let _instance = crate::storage::InstanceLock::acquire(&directory).unwrap();
            log::info!("Fake source directory locked");
            let store = crate::storage::Store::open(&directory).unwrap();
            let identity = store.load_identity().unwrap();
            log::info!("Data directory: {}", directory.display());
            let report = AuthService::new(store.xiaomi(), CloudClient::new().unwrap())
                .unwrap()
                .local_status()
                .unwrap();
            let status = AuthStatus::new(report, "/bin/migate".into(), directory.clone());
            let mut source = AppSource::new(&store, Some(outlet()));
            let handle = source.handle.clone();
            let source_device = source.physical.as_ref().unwrap().clone();
            let mut receiver = source.receivers.remove(0);
            let gate = directory.join("source-ready");
            let remove = directory.join("source-remove");
            let initial =
                std::env::var("MIGATE_TEST_INITIAL_POWER").is_ok_and(|value| value == "on");
            let port = std::env::var("MIGATE_TEST_MATTER_PORT")
                .unwrap()
                .parse()
                .unwrap();
            struct Guard;
            impl Drop for Guard {
                fn drop(&mut self) {
                    log::info!("Fake source stopped");
                }
            }
            let source = async move {
                let _guard = Guard;
                log::info!("Fake source awaiting initial read");
                while !gate.try_exists()? {
                    async_io::Timer::after(Duration::from_millis(10)).await;
                }
                source.read_power(initial);
                source_device.set_availability(Availability::Available);
                log::info!(
                    "Fake source initial read: {}",
                    if initial { "on" } else { "off" }
                );
                let commands = async {
                    while let Some(operation) = receiver.next_operation().await {
                        let DeviceRequest::Power(request) = operation else {
                            panic!("unexpected light operation")
                        };
                        source_device.functions()[0].observe(request.target);
                        request.accept()?;
                    }
                    future::pending().await
                };
                let topology = async {
                    let mut change = 0;
                    loop {
                        while !remove.try_exists()? {
                            async_io::Timer::after(Duration::from_millis(10)).await;
                        }
                        fs::remove_file(&remove)?;
                        change += 1;
                        if change == 1 {
                            source.remove("test-outlet");
                        } else {
                            let mut scope = crate::storage::tests::fixtures::scope();
                            scope.home_id = format!("home-{change}");
                            source.scope(scope);
                        }
                        source.stage();
                        log::info!("Fake source staged configuration {change}");
                        future::yield_now().await;
                    }
                };
                future::or(commands, topology).await
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
                    port,
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
