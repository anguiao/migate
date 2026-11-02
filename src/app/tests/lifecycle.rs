use super::super::{BridgeOptions, run_xiaomi_bridge};
use super::{outlet, status};
use crate::{storage::Store, xiaomi::adapter::tests::app_source::AppSource};
use futures_lite::{
    future::{self, block_on, poll_once},
    io::Cursor,
};
use std::{cell::Cell, rc::Rc};

struct Dropped(Rc<Cell<bool>>);
impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.set(true);
    }
}
#[test]
fn preparation_interrupt_drops_source_and_cancels_queued_commands() {
    block_on(async {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let identity = store.load_identity().unwrap();
        let status = status(&store);
        let AppSource {
            adapter,
            handle,
            physical,
            mut receivers,
        } = AppSource::new(&store, Some(outlet()));
        let device = physical.unwrap();
        let receiver = receivers.remove(0);
        drop(handle);
        device.functions()[0].observe(false);
        device.set_availability(crate::device::Availability::Available);
        let command = device.functions()[0].execute(crate::device::Command::On);
        let mut command = Box::pin(command);
        assert!(poll_once(command.as_mut()).await.is_none());
        let dropped = Rc::new(Cell::new(false));
        let guard = Dropped(dropped.clone());
        let resources = (receiver, adapter);
        let source = async move {
            let _guard = guard;
            let _resources = resources;
            future::pending().await
        };
        run_xiaomi_bridge(
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
        let error = block_on(run_xiaomi_bridge(
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
                    let source = AppSource::new(&store, Some(outlet()));
                    source.read_power(false);
                    let AppSource {
                        adapter,
                        handle,
                        physical,
                        mut receivers,
                    } = source;
                    let device = physical.unwrap();
                    let receiver = receivers.remove(0);
                    device.set_availability(crate::device::Availability::Available);
                    let mut command =
                        Box::pin(device.functions()[0].execute(crate::device::Command::On));
                    assert!(poll_once(command.as_mut()).await.is_none());
                    let started = event_listener::Event::new();
                    let listening = started.listen();
                    let paired = Cell::new(false);
                    let dropped = Rc::new(Cell::new(false));
                    let guard = Dropped(dropped.clone());
                    let source = async move {
                        let _guard = guard;
                        let _adapter = adapter;
                        let _receiver = receiver;
                        listening.await;
                        if failure {
                            Err("source failed after startup".into())
                        } else {
                            Ok(())
                        }
                    };
                    let error = run_xiaomi_bridge(
                        BridgeOptions {
                            store: &store,
                            identity: &identity,
                            status: &status,
                            port: 0,
                        },
                        async { Ok(handle) },
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
                    assert_eq!(
                        device.functions()[0].snapshot().power(),
                        Some(crate::device::Power::Off)
                    );
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
                let source = AppSource::new(&store, Some(outlet()));
                source.read_power(false);
                let AppSource {
                    adapter,
                    handle,
                    physical,
                    mut receivers,
                } = source;
                let device = physical.unwrap();
                let mut receiver = receivers.remove(0);
                device.set_availability(crate::device::Availability::Available);
                let before = device.functions()[0].snapshot();
                let mut active =
                    Box::pin(device.functions()[0].execute(crate::device::Command::On));
                let mut queued =
                    Box::pin(device.functions()[0].execute(crate::device::Command::Toggle));
                assert!(poll_once(active.as_mut()).await.is_none());
                assert!(poll_once(queued.as_mut()).await.is_none());
                let stop = event_listener::Event::new();
                let listening = stop.listen();
                let ready = event_listener::Event::new();
                let ready_listening = ready.listen();
                let ready_seen = Cell::new(false);
                let ready_ref = &ready_seen;
                let stop_ref = &stop;
                let dropped = Rc::new(Cell::new(false));
                let guard = Dropped(dropped.clone());
                let source = async move {
                    let _guard = guard;
                    let _adapter = adapter;
                    ready_listening.await;
                    assert!(ready_ref.get());
                    let _active = receiver.next_operation().await.unwrap();
                    stop_ref.notify(usize::MAX);
                    let _receiver = receiver;
                    future::pending().await
                };
                run_xiaomi_bridge(
                    BridgeOptions {
                        store: &store,
                        identity: &identity,
                        status: &status,
                        port: 0,
                    },
                    async {
                        ready_seen.set(true);
                        ready.notify(usize::MAX);
                        Ok(handle)
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
                let after = device.functions()[0].snapshot();
                assert_eq!(after.power(), before.power());
                assert_eq!(after.lighting(), before.lighting());
                assert_eq!(
                    after.availability,
                    crate::device::Availability::Unavailable("Xiaomi adapter stopped".into())
                );
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
                    let AppSource {
                        adapter,
                        handle,
                        physical,
                        mut receivers,
                    } = AppSource::new(&store, Some(outlet()));
                    assert!(physical.is_some());
                    let receiver = receivers.remove(0);
                    let dropped = Rc::new(Cell::new(false));
                    let guard = Dropped(dropped.clone());
                    let source = async move {
                        let _guard = guard;
                        let _adapter = adapter;
                        let _receiver = receiver;
                        future::pending().await
                    };
                    let input: &[u8] = if output_failure {
                        b"status\n"
                    } else {
                        b"\xff\n"
                    };
                    let error = run_xiaomi_bridge(
                        BridgeOptions {
                            store: &store,
                            identity: &identity,
                            status: &status,
                            port: 0,
                        },
                        async { Ok(handle) },
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

#[test]
fn empty_registry_starts_the_bridge_and_eof_keeps_the_source_running() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            block_on(async {
                let directory = tempfile::tempdir().unwrap();
                let store = Store::open(directory.path()).unwrap();
                let identity = store.load_identity().unwrap();
                let status = status(&store);
                let started = event_listener::Event::new();
                let listener = started.listen();
                let paired = Cell::new(false);
                let AppSource {
                    adapter, handle, ..
                } = AppSource::new(&store, None);
                run_xiaomi_bridge(
                    BridgeOptions {
                        store: &store,
                        identity: &identity,
                        status: &status,
                        port: 0,
                    },
                    async { Ok(handle) },
                    async move {
                        let _adapter = adapter;
                        future::pending().await
                    },
                    Cursor::new(b"list\nstatus\n"),
                    Vec::new(),
                    async {
                        listener.await;
                        Ok(())
                    },
                    |_| {
                        paired.set(true);
                        started.notify(usize::MAX);
                        Ok(())
                    },
                )
                .await
                .unwrap();
                assert!(paired.get());
            })
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn frequent_configuration_changes_cannot_postpone_the_startup_deadline() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            block_on(async {
                let directory = tempfile::tempdir().unwrap();
                let store = Store::open(directory.path()).unwrap();
                let identity = store.load_identity().unwrap();
                let status = status(&store);
                let source = AppSource::new(&store, Some(outlet()));
                source.confirm_all();
                let app =
                    Rc::new(crate::app::App::open(source.handle.clone(), store.clone()).unwrap());
                let committed = app.clone();
                let updates = Rc::new(Cell::new(0));
                let source_updates = updates.clone();
                let flood_started = std::time::Instant::now();
                let source = async move {
                    future::yield_now().await;
                    source.describe(
                        crate::device::PhysicalDeviceDescription::new(
                            crate::device::DeviceKey::new("xiaomi", "new-outlet"),
                            "New outlet",
                            outlet().functions().to_vec(),
                        )
                        .unwrap(),
                    );
                    source.read_power(false);
                    while flood_started.elapsed() < std::time::Duration::from_secs(5) {
                        source.stage();
                        source_updates.set(source_updates.get() + 1);
                        future::yield_now().await;
                    }
                    future::pending::<Result<(), crate::RuntimeError>>().await
                };
                let started = event_listener::Event::new();
                let listening = started.listen();
                let began = std::time::Instant::now();
                let started_at = Cell::new(None);
                let published_at_start = Cell::new(0);
                let bridge = super::super::run_lifecycle(
                    BridgeOptions {
                        store: &store,
                        identity: &identity,
                        status: &status,
                        port: 0,
                    },
                    async { Ok((app, std::time::Duration::from_millis(50))) },
                    source,
                    Cursor::new([]),
                    Vec::new(),
                    async {
                        listening.await;
                        Ok(())
                    },
                    |_| {
                        started_at.set(Some(std::time::Instant::now()));
                        published_at_start.set(committed.frozen().len());
                        started.notify(usize::MAX);
                        Ok(())
                    },
                );
                future::or(bridge, async {
                    async_io::Timer::after(std::time::Duration::from_secs(5)).await;
                    panic!("configuration changes postponed Matter startup")
                })
                .await
                .unwrap();
                assert!(updates.get() > 1);
                assert_eq!(published_at_start.get(), 2);
                let elapsed = started_at.get().unwrap().duration_since(began);
                assert!(elapsed >= std::time::Duration::from_millis(50));
                assert!(elapsed < std::time::Duration::from_secs(5));
            })
        })
        .unwrap()
        .join()
        .unwrap();
}
