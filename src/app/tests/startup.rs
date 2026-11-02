use super::*;
use crate::{
    device::{DeviceKey, FunctionDescription, FunctionKind, PhysicalDeviceDescription},
    storage::Store,
    xiaomi::adapter::tests::app_source::AppSource,
};
use futures_lite::future::{block_on, poll_once};

fn outlet(id: &str) -> PhysicalDeviceDescription {
    PhysicalDeviceDescription::new(
        DeviceKey::new("xiaomi", id),
        "Test Outlet",
        vec![FunctionDescription {
            id: "outlet:2".into(),
            label: "Main power".into(),
            kind: FunctionKind::Outlet,
        }],
    )
    .unwrap()
}

fn fixture() -> (tempfile::TempDir, AppSource, App) {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let source = AppSource::new(&store, Some(outlet("test-outlet")));
    let app = App::open(source.handle.clone(), store).unwrap();
    (directory, source, app)
}

#[test]
fn first_observation_or_read_failure_releases_the_gate_once() {
    block_on(async {
        for observed in [true, false] {
            let (_directory, source, app) = fixture();
            let gate = StartupGate {
                deadline: Instant::now() + Duration::from_secs(60),
                complete: Cell::new(false),
            };
            let mut wait = Box::pin(gate.wait(&app));
            assert!(poll_once(wait.as_mut()).await.is_none());
            if observed {
                source.read_power(true);
            } else {
                source.fail_initial_read();
            }
            assert!(poll_once(wait.as_mut()).await.is_some());

            let (_other_directory, _other_source, other_app) = fixture();
            assert!(
                poll_once(Box::pin(gate.wait(&other_app)).as_mut())
                    .await
                    .is_some()
            );
        }
    });
}

#[test]
fn deadline_releases_pending_reads_without_discarding_later_observations() {
    block_on(async {
        let (_directory, source, app) = fixture();
        assert!(!app.source.initial_reads_ready(&app.current().configuration));
        let gate = StartupGate {
            deadline: Instant::now(),
            complete: Cell::new(false),
        };
        gate.wait(&app).await;
        assert!(!app.source.initial_reads_ready(&app.current().configuration));
        source.read_power(true);
        assert!(app.source.initial_reads_ready(&app.current().configuration));
        assert_eq!(
            source.physical.as_ref().unwrap().functions()[0]
                .snapshot()
                .power(),
            Some(crate::device::Power::On)
        );
    });
}

#[test]
fn restarting_the_wait_keeps_the_original_deadline() {
    block_on(async {
        let (_directory, _source, app) = fixture();
        let gate = StartupGate {
            deadline: Instant::now() + Duration::from_millis(20),
            complete: Cell::new(false),
        };
        let mut first_wait = Box::pin(gate.wait(&app));
        assert!(poll_once(first_wait.as_mut()).await.is_none());
        drop(first_wait);

        async_io::Timer::at(gate.deadline).await;
        assert!(
            poll_once(Box::pin(gate.wait(&app)).as_mut())
                .await
                .is_some()
        );
        assert!(!app.source.initial_reads_ready(&app.current().configuration));
    });
}

#[test]
fn removing_a_waiting_device_uses_the_committed_configuration() {
    block_on(async {
        let (_directory, source, app) = fixture();
        let gate = StartupGate {
            deadline: Instant::now() + Duration::from_secs(60),
            complete: Cell::new(false),
        };
        let mut wait = Box::pin(gate.wait(&app));
        assert!(poll_once(wait.as_mut()).await.is_none());

        source.remove("test-outlet");
        let plan = app
            .plan()
            .unwrap()
            .expect("removed device changes the graph");
        app.apply(app.prepare(plan)).unwrap();
        assert!(app.frozen().is_empty());
        assert!(poll_once(wait.as_mut()).await.is_some());
    });
}

#[test]
fn adding_a_device_during_the_wait_requires_its_initial_result() {
    block_on(async {
        let (_directory, mut source, app) = fixture();
        source.confirm_all();
        let gate = StartupGate::new(Duration::from_secs(60));
        let mut wait = Box::pin(gate.wait(&app));
        assert!(poll_once(wait.as_mut()).await.is_none());

        source.describe(outlet("new-outlet"));
        let plan = app.plan().unwrap().expect("new device changes the graph");
        app.apply(app.prepare(plan)).unwrap();
        assert_eq!(app.frozen().len(), 2);

        source.read_power(true);
        assert!(poll_once(wait.as_mut()).await.is_none());
        source.physical = app
            .current()
            .devices()
            .into_iter()
            .find(|device| device.key().id == "new-outlet");
        source.read_power(true);
        assert!(poll_once(wait.as_mut()).await.is_some());
    });
}
