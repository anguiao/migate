use super::*;
use crate::{
    matter::tests::test_device,
    xiaomi::{auth::AuthService, cloud::CloudClient},
};
use futures_lite::{
    future::{block_on, poll_once},
    io::Cursor,
};
use std::{cell::Cell, rc::Rc};

struct Dropped(Rc<Cell<bool>>);
impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.set(true);
    }
}
fn status(store: &Store) -> AuthStatus {
    AuthStatus::new(
        AuthService::new(store.xiaomi(), CloudClient::new().unwrap())
            .local_status()
            .unwrap(),
        "/bin/migate".into(),
        "/data".into(),
    )
}
#[test]
fn default_labels_are_bounded_utf8_and_include_model_and_device_id() {
    assert_eq!(
        default_device_label("cuco.plug.v3", "571329355"),
        "cuco.plug.v3 (571329355)"
    );
    for (model, id) in [
        ("插座设备".repeat(20), "设备标识".repeat(20)),
        ("x".repeat(100), "1".repeat(128)),
    ] {
        let label = default_device_label(&model, &id);
        assert!(label.len() <= 32);
        assert!(label.ends_with(')'));
        assert!(label.contains('('));
    }
}
#[test]
fn preparation_interrupt_drops_source_and_cancels_queued_commands() {
    block_on(async {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let identity = store.load_identity().unwrap();
        let status = status(&store);
        let (device, receiver) = test_device();
        device.observe(false);
        device.set_availability(crate::device::Availability::Available);
        let command = device.execute(crate::device::Command::On);
        let mut command = Box::pin(command);
        assert!(poll_once(command.as_mut()).await.is_none());
        let dropped = Rc::new(Cell::new(false));
        let guard = Dropped(dropped.clone());
        let source = async move {
            let _guard = guard;
            let _receiver = receiver;
            future::pending().await
        };
        run_prepared_bridge(
            BridgeOptions {
                store: &store,
                identity: &identity,
                status: &status,
                port: 0,
            },
            future::pending(),
            source,
            Cursor::new([]),
            Vec::new(),
            async { Ok(()) },
            |_| panic!("must not pair before ready"),
        )
        .await
        .unwrap();
        assert!(dropped.get());
        assert_eq!(command.await, Err(crate::device::CommandError::Cancelled));
    });
}
#[test]
fn source_errors_and_unexpected_completion_stop_preparation() {
    for failure in [true, false] {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let identity = store.load_identity().unwrap();
        let status = status(&store);
        let error = block_on(run_prepared_bridge(
            BridgeOptions {
                store: &store,
                identity: &identity,
                status: &status,
                port: 0,
            },
            future::pending(),
            async {
                if failure {
                    Err("source failed".into())
                } else {
                    Ok(())
                }
            },
            Cursor::new([]),
            Vec::new(),
            future::pending(),
            |_| Ok(()),
        ))
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            if failure {
                "source failed"
            } else {
                "Device source ended unexpectedly"
            }
        );
    }
}

#[test]
fn source_completion_after_matter_startup_stops_serving_and_drops_commands() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            block_on(async {
                for failure in [true, false] {
                    let directory = tempfile::tempdir().unwrap();
                    let store = Store::open(directory.path()).unwrap();
                    let identity = store.load_identity().unwrap();
                    let status = status(&store);
                    let (device, receiver) = test_device();
                    device.observe(false);
                    device.set_availability(crate::device::Availability::Available);
                    let mut command = Box::pin(device.execute(crate::device::Command::On));
                    assert!(poll_once(command.as_mut()).await.is_none());
                    let started = event_listener::Event::new();
                    let listening = started.listen();
                    let paired = Cell::new(false);
                    let dropped = Rc::new(Cell::new(false));
                    let guard = Dropped(dropped.clone());
                    let source = async move {
                        let _guard = guard;
                        let _receiver = receiver;
                        listening.await;
                        if failure {
                            Err("source failed after startup".into())
                        } else {
                            Ok(())
                        }
                    };
                    let error = run_prepared_bridge(
                        BridgeOptions {
                            store: &store,
                            identity: &identity,
                            status: &status,
                            port: 0,
                        },
                        async {
                            Ok(DeviceSetup {
                                device: device.clone(),
                                default_label: "Test Outlet".into(),
                            })
                        },
                        source,
                        Cursor::new([]),
                        Vec::new(),
                        future::pending(),
                        |_| {
                            paired.set(true);
                            started.notify(usize::MAX);
                            Ok(())
                        },
                    )
                    .await
                    .unwrap_err();
                    assert!(paired.get());
                    assert_eq!(
                        error.to_string(),
                        if failure {
                            "source failed after startup"
                        } else {
                            "Device source ended unexpectedly"
                        }
                    );
                    assert!(dropped.get());
                    assert_eq!(command.await, Err(crate::device::CommandError::Cancelled));
                    assert_eq!(device.snapshot().power, crate::device::Power::Off);
                }
            })
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn serving_interrupt_cancels_active_and_queued_commands_without_changing_power() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            block_on(async {
                let directory = tempfile::tempdir().unwrap();
                let store = Store::open(directory.path()).unwrap();
                let identity = store.load_identity().unwrap();
                let status = status(&store);
                let (device, mut receiver) = test_device();
                device.observe(false);
                device.set_availability(crate::device::Availability::Available);
                let before = device.snapshot();
                let mut active = Box::pin(device.execute(crate::device::Command::On));
                let mut queued = Box::pin(device.execute(crate::device::Command::Toggle));
                assert!(poll_once(active.as_mut()).await.is_none());
                assert!(poll_once(queued.as_mut()).await.is_none());
                let stop = event_listener::Event::new();
                let listening = stop.listen();
                let ready_seen = Cell::new(false);
                let ready_ref = &ready_seen;
                let stop_ref = &stop;
                let dropped = Rc::new(Cell::new(false));
                let guard = Dropped(dropped.clone());
                let source = async move {
                    let _guard = guard;
                    assert!(ready_ref.get());
                    let _active = receiver.next().await.unwrap();
                    stop_ref.notify(usize::MAX);
                    let _receiver = receiver;
                    future::pending().await
                };
                run_prepared_bridge(
                    BridgeOptions {
                        store: &store,
                        identity: &identity,
                        status: &status,
                        port: 0,
                    },
                    async {
                        ready_seen.set(true);
                        Ok(DeviceSetup {
                            device: device.clone(),
                            default_label: "Test Outlet".into(),
                        })
                    },
                    source,
                    Cursor::new([]),
                    Vec::new(),
                    async {
                        listening.await;
                        Ok(())
                    },
                    |_| Ok(()),
                )
                .await
                .unwrap();
                assert!(dropped.get());
                assert_eq!(active.await, Err(crate::device::CommandError::Cancelled));
                assert_eq!(queued.await, Err(crate::device::CommandError::Cancelled));
                assert_eq!(device.snapshot(), before);
            })
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn serving_terminal_io_failure_drops_the_source() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            block_on(async {
                for output_failure in [false, true] {
                    let directory = tempfile::tempdir().unwrap();
                    let store = Store::open(directory.path()).unwrap();
                    let identity = store.load_identity().unwrap();
                    let status = status(&store);
                    let (device, receiver) = test_device();
                    let dropped = Rc::new(Cell::new(false));
                    let guard = Dropped(dropped.clone());
                    let source = async move {
                        let _guard = guard;
                        let _receiver = receiver;
                        future::pending().await
                    };
                    let input: &[u8] = if output_failure {
                        b"status\n"
                    } else {
                        b"\xff\n"
                    };
                    let error = run_prepared_bridge(
                        BridgeOptions {
                            store: &store,
                            identity: &identity,
                            status: &status,
                            port: 0,
                        },
                        async {
                            Ok(DeviceSetup {
                                device,
                                default_label: "Test Outlet".into(),
                            })
                        },
                        source,
                        Cursor::new(input),
                        Cursor::new(&mut [0u8; 0][..]),
                        future::pending(),
                        |_| Ok(()),
                    )
                    .await
                    .unwrap_err();
                    assert!(error.to_string().starts_with("Terminal I/O failed:"));
                    assert!(dropped.get());
                }
            })
        })
        .unwrap()
        .join()
        .unwrap();
}
