mod process;

use super::{BridgeOptions, StartupGate, run_xiaomi_bridge};
use crate::app::tests::support::{outlet, status};
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

struct ObservedEof<'a>(&'a event_listener::Event);

impl futures_lite::io::AsyncRead for ObservedEof<'_> {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
        _: &mut [u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        self.0.notify(usize::MAX);
        std::task::Poll::Ready(Ok(0))
    }
}

#[test]
fn empty_configuration_starts_the_bridge_and_eof_keeps_the_source_running() {
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
                let eof = event_listener::Event::new();
                let eof_seen = eof.listen();
                let source = AppSource::new(&store, None);
                let handle = source.handle.clone();
                let error = run_xiaomi_bridge(
                    BridgeOptions {
                        store: &store,
                        identity: &identity,
                        status: &status,
                        port: 0,
                    },
                    async { Ok(handle) },
                    async {
                        listener.await;
                        eof_seen.await;
                        let rebuilt = started.listen();
                        source.describe(outlet());
                        source.stage();
                        rebuilt.await;
                        Err("source update served after EOF".into())
                    },
                    futures_lite::io::BufReader::new(ObservedEof(&eof)),
                    Vec::new(),
                    async {
                        async_io::Timer::after(std::time::Duration::from_secs(10)).await;
                        Err("timed out waiting for the bridge to serve the update".into())
                    },
                    |_| {
                        started.notify(usize::MAX);
                        Ok(())
                    },
                )
                .await
                .unwrap_err();
                assert_eq!(error.to_string(), "source update served after EOF");
                assert!(
                    store
                        .devices()
                        .load_configuration()
                        .unwrap()
                        .device(outlet().key())
                        .is_some()
                );
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
                let app = Rc::new(crate::app::App::open(source.handle.clone(), store.clone()));
                let committed = app.clone();
                let updates = Rc::new(Cell::new(0));
                let source_updates = updates.clone();
                let (expire, deadline) = flume::bounded(0);
                let gate = StartupGate::until(deadline);
                let source = async move {
                    let mut expire = Some(expire);
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
                    loop {
                        source.stage();
                        source_updates.set(source_updates.get() + 1);
                        if source_updates.get() == 4 {
                            drop(expire.take());
                        }
                        future::yield_now().await;
                    }
                };
                let started = event_listener::Event::new();
                let listening = started.listen();
                let published_at_start = Cell::new(0);
                let bridge = super::run_lifecycle(
                    BridgeOptions {
                        store: &store,
                        identity: &identity,
                        status: &status,
                        port: 0,
                    },
                    async { Ok((app, gate)) },
                    source,
                    Cursor::new([]),
                    Vec::new(),
                    async {
                        listening.await;
                        Ok(())
                    },
                    |_| {
                        assert!(updates.get() >= 4);
                        assert!(
                            !committed
                                .source
                                .initial_reads_ready(&committed.current().configuration)
                        );
                        published_at_start.set(committed.matter_configuration().entries.len());
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
                assert_eq!(published_at_start.get(), 2);
            })
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn terminal_mapping_restarts_the_running_matter_instance_once() {
    std::thread::Builder::new().stack_size(16 * 1024 * 1024).spawn(|| {
        block_on(async {
            use crate::{device::RelayMode, matter::tests::support::PublishedRelayModel, storage::tests::fixtures, xiaomi::capability::tests::source::SourceFixture};
            use futures_lite::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
            use std::os::unix::net::UnixStream;
            let directory = tempfile::tempdir().unwrap();
            let store = Store::open(directory.path()).unwrap();
            let fixture = SourceFixture::new("panel", "zimi.switch.dhkg01", serde_json::from_str(include_str!("../../../../tests/fixtures/miot/zimi-dhkg01.json")).unwrap());
            let mut configuration = fixtures::planned(&store, vec![fixture]);
            configuration.relay_channels[0].mode = Some(RelayMode::Wired);
            fixtures::save(&store, &configuration);
            let identity = store.load_identity().unwrap();
            let status = status(&store);
            let source = AppSource::new(&store, None);
            source.confirm_all();
            source.read_power(false);
            let app = Rc::new(crate::app::App::open(source.handle.clone(), store.clone()));
            let endpoint = configuration.relay_channels[0].endpoint;
            let (input, mut sender) = {
                let (a, b) = UnixStream::pair().unwrap();
                (BufReader::new(async_io::Async::new(a).unwrap()), async_io::Async::new(b).unwrap())
            };
            let (output, mut replies) = {
                let (a, b) = UnixStream::pair().unwrap();
                (async_io::Async::new(a).unwrap(), BufReader::new(async_io::Async::new(b).unwrap()))
            };
            let starts = Cell::new(0);
            let started = event_listener::Event::new();
            let done = event_listener::Event::new();
            let stopping = done.listen();
            let bridge = super::run_lifecycle(
                BridgeOptions { store: &store, identity: &identity, status: &status, port: 0 },
                async { Ok((app.clone(), StartupGate::new(std::time::Duration::ZERO))) },
                async move { let _source = source; future::pending().await },
                input, output,
                async { stopping.await; Ok(()) },
                |_| { starts.set(starts.get() + 1); started.notify(usize::MAX); Ok(()) },
            );
            let controller = async {
                loop { let listener = started.listen(); if starts.get() >= 1 { break; } listener.await; }
                let before = PublishedRelayModel::new(&app, &store);
                before.check_relay_type(endpoint, false);
                let original_id = before.unique_id(endpoint).await;
                let database = rusqlite::Connection::open(store.path()).unwrap();
                database.execute_batch("CREATE TRIGGER fail_mapping BEFORE UPDATE ON bridge_configuration BEGIN SELECT RAISE(ABORT, 'injected'); END;").unwrap();
                sender.write_all(b"map-type panel relay:2 light\n").await.unwrap();
                let mut response = String::new();
                replies.read_line(&mut response).await.unwrap();
                assert!(response.contains("Mapping failed"), "{response}");
                database.execute_batch("DROP TRIGGER fail_mapping").unwrap();
                async_io::Timer::after(std::time::Duration::from_millis(1100)).await;
                assert_eq!(starts.get(), 1);
                PublishedRelayModel::new(&app, &store).check_relay_type(endpoint, false);
                response.clear();
                sender.write_all(b"map-type panel relay:2 light\n").await.unwrap();
                replies.read_line(&mut response).await.unwrap();
                assert!(response.contains("mapped to light"), "{response}");
                loop { let listener = started.listen(); if starts.get() >= 2 { break; } listener.await; }
                let after = PublishedRelayModel::new(&app, &store);
                after.check_relay_type(endpoint, true);
                assert_eq!(after.unique_id(endpoint).await, original_id);
                assert_eq!(crate::matter::tests::support::persisted_root_version(&store), configuration.root_version + 1);
                assert!(!app.pending());
                response.clear();
                sender.write_all(b"map-type panel relay:2 light\n").await.unwrap();
                replies.read_line(&mut response).await.unwrap();
                assert!(response.contains("already mapped"), "{response}");
                async_io::Timer::after(std::time::Duration::from_millis(100)).await;
                assert_eq!(starts.get(), 2);
                done.notify(usize::MAX);
            };
            let (result, ()) = future::zip(bridge, controller).await;
            result.unwrap();
        })
    }).unwrap().join().unwrap();
}

struct WaitingDeadline(event_listener::Event);
impl super::StartupDeadline for WaitingDeadline {
    fn expired(&self) -> bool {
        false
    }
    async fn wait(&self) {
        self.0.notify(usize::MAX);
        future::pending::<()>().await;
    }
}

#[test]
fn withdrawal_while_startup_waits_is_excluded_when_configuration_save_fails() {
    std::thread::Builder::new()
        .stack_size(32 * 1024 * 1024)
        .spawn(|| {
            block_on(async {
                let directory = tempfile::tempdir().unwrap();
                let store = Store::open(directory.path()).unwrap();
                let identity = store.load_identity().unwrap();
                let status = status(&store);
                let source = AppSource::new(&store, Some(outlet()));
                source.confirm_all();
                let app = Rc::new(crate::app::App::open(source.handle.clone(), store.clone()));
                rusqlite::Connection::open(store.path())
                    .unwrap()
                    .execute_batch(
                        "CREATE TRIGGER fail_withdrawal BEFORE UPDATE ON bridge_configuration
                         BEGIN SELECT RAISE(ABORT, 'injected'); END;",
                    )
                    .unwrap();
                let waiting = event_listener::Event::new();
                let waiting_seen = waiting.listen();
                let gate = StartupGate::until(WaitingDeadline(waiting));
                let started = event_listener::Event::new();
                let started_seen = started.listen();
                let stop = event_listener::Event::new();
                let stopping = stop.listen();
                let socket = std::net::UdpSocket::bind("[::1]:0").unwrap();
                let port = socket.local_addr().unwrap().port();
                drop(socket);
                let bridge = super::run_lifecycle(
                    BridgeOptions {
                        store: &store,
                        identity: &identity,
                        status: &status,
                        port,
                    },
                    async { Ok((app.clone(), gate)) },
                    async {
                        waiting_seen.await;
                        source.remove("test-outlet");
                        // Keep the waiting runner intact to expose a topology captured before withdrawal.
                        let plan = app.plan().unwrap().unwrap();
                        assert!(app.apply(app.prepare(plan)).is_err());
                        future::pending().await
                    },
                    Cursor::new([]),
                    Vec::new(),
                    async {
                        stopping.await;
                        Ok(())
                    },
                    |_| {
                        started.notify(usize::MAX);
                        Ok(())
                    },
                );
                let controller = async {
                    started_seen.await;
                    let parts = crate::matter::tests::support::running_parts(port).await;
                    assert!(
                        parts.is_empty(),
                        "withdrawn source was assembled: {parts:?}"
                    );
                    assert!(app.failure().is_some());
                    assert!(
                        store
                            .devices()
                            .load_configuration()
                            .unwrap()
                            .device(outlet().key())
                            .unwrap()
                            .native
                            .is_some()
                    );
                    assert_eq!(
                        crate::matter::tests::support::persisted_root_version(&store),
                        app.current().configuration.root_version,
                    );
                    stop.notify(usize::MAX);
                };
                future::or(
                    async {
                        let (result, ()) = future::zip(bridge, controller).await;
                        result.unwrap();
                    },
                    async {
                        async_io::Timer::after(std::time::Duration::from_secs(10)).await;
                        panic!("timed out reading the startup topology")
                    },
                )
                .await;
            });
        })
        .unwrap()
        .join()
        .unwrap();
}
