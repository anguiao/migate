use super::*;
use futures_lite::future::{block_on, poll_once};

fn outlet() -> DeviceDescription {
    DeviceDescription {
        key: DeviceKey::new("xiaomi", "test-outlet"),
        category: Category::Outlet,
        capabilities: vec![Capability::OnOff],
    }
}

fn ready_device() -> (Device, CommandReceiver) {
    let (device, receiver) = Device::new(outlet());
    device.observe(false);
    device.set_availability(Availability::Available);
    (device, receiver)
}

#[test]
fn new_devices_have_stable_identity_unknown_state_and_initializing_availability() {
    let (device, _) = Device::new(outlet());
    assert_eq!(
        device.description().key,
        DeviceKey::new("xiaomi", "test-outlet")
    );
    assert_eq!(device.snapshot().power, Power::Unknown);
    assert_eq!(device.snapshot().availability, Availability::Initializing);
    assert_eq!(device.snapshot().revision, 0);
}

#[test]
fn notifications_coalesce_and_same_values_do_not_increment_revision() {
    block_on(async {
        let (device, _receiver) = Device::new(outlet());
        let mut changes = device.subscribe();
        device.observe(false);
        device.observe(false);
        device.set_availability(Availability::Available);
        assert_eq!(device.snapshot().revision, 2);
        let latest = changes.changed().await;
        assert_eq!(latest, device.snapshot());
        device.set_availability(Availability::Available);
        assert!(poll_once(changes.changed()).await.is_none());
        device.set_availability(Availability::Unavailable("connection lost".into()));
        let latest = changes.changed().await;
        assert_eq!(latest.power, Power::Off);
        assert_eq!(latest.revision, 3);
    });
}

#[test]
fn commands_change_state_only_after_acceptance_and_repeat_targets_are_allowed() {
    block_on(async {
        let (device, mut receiver) = ready_device();
        let mut changes = device.subscribe();
        let command = device.execute(Command::On);
        futures_lite::pin!(command);
        assert!(poll_once(command.as_mut()).await.is_none());
        let request = receiver.next().await.unwrap();
        assert!(request.target);
        assert_eq!(device.snapshot().power, Power::Off);
        request.accept().unwrap();
        assert_eq!(command.await.unwrap().power, Power::On);
        assert_eq!(changes.changed().await.power, Power::On);

        let command = device.execute(Command::On);
        futures_lite::pin!(command);
        assert!(poll_once(command.as_mut()).await.is_none());
        receiver.next().await.unwrap().accept().unwrap();
        command.await.unwrap();
        assert!(poll_once(changes.changed()).await.is_none());
    });
}

#[test]
fn rejected_commands_preserve_state_and_later_observations_still_apply() {
    block_on(async {
        let (device, mut receiver) = ready_device();
        let command = device.execute(Command::On);
        futures_lite::pin!(command);
        poll_once(command.as_mut()).await;
        receiver
            .next()
            .await
            .unwrap()
            .reject(CommandError::Rejected("access denied".into()));
        assert_eq!(
            command.await,
            Err(CommandError::Rejected("access denied".into()))
        );
        assert_eq!(device.snapshot().power, Power::Off);
        device.observe(true);
        assert_eq!(device.snapshot().power, Power::On);
    });
}

#[test]
fn commands_are_fifo_and_toggle_uses_snapshot_at_execution_time() {
    block_on(async {
        let (device, mut receiver) = ready_device();
        let first = device.execute(Command::On);
        let second = device.execute(Command::Toggle);
        futures_lite::pin!(first, second);
        poll_once(first.as_mut()).await;
        poll_once(second.as_mut()).await;
        let first_request = receiver.next().await.unwrap();
        assert!(poll_once(receiver.next()).await.is_none());
        first_request.accept().unwrap();
        first.await.unwrap();
        // A current observation affects Toggle when its turn arrives.
        device.observe(false);
        let second_request = receiver.next().await.unwrap();
        assert!(second_request.target);
        second_request.accept().unwrap();
        assert_eq!(second.await.unwrap().power, Power::On);
    });
}

#[test]
fn toggle_unknown_and_unavailable_commands_fail_without_sending() {
    block_on(async {
        let (device, mut receiver) = Device::new(outlet());
        assert_eq!(
            device.execute(Command::On).await,
            Err(CommandError::Unavailable("initializing".into()))
        );
        device.set_availability(Availability::Available);
        let command = device.execute(Command::Toggle);
        futures_lite::pin!(command);
        poll_once(command.as_mut()).await;
        assert!(poll_once(receiver.next()).await.is_none());
        assert_eq!(command.await, Err(CommandError::UnknownState));
        device.set_availability(Availability::Unavailable("offline".into()));
        assert_eq!(
            device.execute(Command::On).await,
            Err(CommandError::Unavailable("offline".into()))
        );
    });
}

#[test]
fn unsupported_capabilities_fail_without_sending() {
    block_on(async {
        let mut description = outlet();
        description.capabilities.clear();
        let (device, mut receiver) = Device::new(description);
        device.set_availability(Availability::Available);
        assert_eq!(
            device.execute(Command::On).await,
            Err(CommandError::Unsupported)
        );
        assert!(poll_once(receiver.next()).await.is_none());
    });
}

#[derive(Default)]
struct TestClock {
    elapsed: Mutex<Duration>,
    changed: Event,
}

impl TestClock {
    fn advance(&self, duration: Duration) {
        *self.elapsed.lock().unwrap() += duration;
        self.changed.notify(usize::MAX);
    }
}

impl Clock for Arc<TestClock> {
    fn now(&self) -> Duration {
        *self.elapsed.lock().unwrap()
    }

    fn wait_until(&self, deadline: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            loop {
                let listener = self.changed.listen();
                if self.now() >= deadline {
                    return;
                }
                listener.await;
            }
        })
    }
}

#[test]
fn five_second_deadline_includes_queue_wait_and_expired_commands_are_never_sent() {
    block_on(async {
        let clock = Arc::new(TestClock::default());
        let (device, mut receiver) = Device::with_clock(outlet(), Box::new(clock.clone()));
        device.observe(false);
        device.set_availability(Availability::Available);
        let first = device.execute(Command::On);
        let second = device.execute(Command::Off);
        futures_lite::pin!(first, second);
        poll_once(first.as_mut()).await;
        let request = receiver.next().await.unwrap();
        assert_eq!(request.remaining(), Duration::from_secs(5));
        poll_once(second.as_mut()).await;
        clock.advance(Duration::from_secs(5));
        assert_eq!(request.accept(), Err(CommandError::Timeout));
        assert_eq!(first.await, Err(CommandError::Timeout));
        assert!(poll_once(receiver.next()).await.is_none());
        assert_eq!(second.await, Err(CommandError::Timeout));
        assert_eq!(device.snapshot().power, Power::Off);
    });
}

#[test]
fn receiver_shutdown_cancels_pending_commands_and_future_submissions() {
    block_on(async {
        let (device, receiver) = ready_device();
        let command = device.execute(Command::On);
        futures_lite::pin!(command);
        poll_once(command.as_mut()).await;
        drop(receiver);
        assert_eq!(command.await, Err(CommandError::Cancelled));
        assert_eq!(
            device.execute(Command::Off).await,
            Err(CommandError::Cancelled)
        );
    });
}

#[test]
fn becoming_unavailable_ends_active_and_queued_requests_and_stale_acceptance() {
    block_on(async {
        let (device, mut receiver) = ready_device();
        let first = device.execute(Command::On);
        let second = device.execute(Command::On);
        futures_lite::pin!(first, second);
        poll_once(first.as_mut()).await;
        poll_once(second.as_mut()).await;
        let stale = receiver.next().await.unwrap();
        device.set_availability(Availability::Unavailable("connection lost".into()));
        let error = CommandError::Unavailable("connection lost".into());
        assert_eq!(poll_once(first.as_mut()).await, Some(Err(error.clone())));
        assert_eq!(poll_once(second.as_mut()).await, Some(Err(error.clone())));
        device.observe(false);
        device.set_availability(Availability::Available);
        assert_eq!(stale.accept(), Err(error));
        assert!(poll_once(receiver.next()).await.is_none());
        assert_eq!(device.snapshot().power, Power::Off);
    });
}
