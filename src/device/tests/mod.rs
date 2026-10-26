async fn next_power(
    receiver: &mut crate::device::CommandReceiver,
) -> Option<crate::device::CommandRequest> {
    match receiver.next_operation().await? {
        crate::device::DeviceRequest::Power(request) => Some(request),
        crate::device::DeviceRequest::Light(_) => panic!("unexpected lighting request"),
    }
}

use futures_lite::future::{block_on, poll_once};

use super::{
    Availability, Command, CommandError, CommandReceiver, DeviceKey, FunctionDescription,
    FunctionKind, PhysicalDevice, PhysicalDeviceDescription, Power,
};

fn outlet() -> PhysicalDeviceDescription {
    PhysicalDeviceDescription::new(
        DeviceKey::new("xiaomi", "test-outlet"),
        "Test Outlet",
        vec![FunctionDescription {
            id: "main".into(),
            label: "Main power".into(),
            kind: FunctionKind::Outlet,
        }],
    )
    .unwrap()
}

fn new_device(description: PhysicalDeviceDescription) -> (PhysicalDevice, CommandReceiver) {
    let (physical, mut receivers) = PhysicalDevice::new(description);
    (physical, receivers.remove(0))
}

fn ready_device() -> (PhysicalDevice, CommandReceiver) {
    let (physical, receiver) = new_device(outlet());
    physical.functions()[0].observe(false);
    physical.set_availability(Availability::Available);
    (physical, receiver)
}

#[test]
fn new_devices_have_stable_identity_unknown_state_and_initializing_availability() {
    let (physical, _) = new_device(outlet());
    let device = &physical.functions()[0];
    assert_eq!(
        device.function_key().device,
        DeviceKey::new("xiaomi", "test-outlet")
    );
    assert_eq!(device.snapshot().power(), Some(Power::Unknown));
    assert_eq!(device.snapshot().availability, Availability::Initializing);
    assert_eq!(device.snapshot().revision, 0);
}

#[test]
fn notifications_coalesce_and_same_values_do_not_increment_revision() {
    block_on(async {
        let (physical, _receiver) = new_device(outlet());
        let device = &physical.functions()[0];
        let mut changes = device.subscribe();
        device.observe(false);
        device.observe(false);
        physical.set_availability(Availability::Available);
        assert_eq!(device.snapshot().revision, 2);
        let latest = changes.changed().await;
        assert_eq!(latest, device.snapshot());
        physical.set_availability(Availability::Available);
        assert!(poll_once(changes.changed()).await.is_none());
        physical.set_availability(Availability::Unavailable("connection lost".into()));
        let latest = changes.changed().await;
        assert_eq!(latest.power(), Some(Power::Off));
        assert_eq!(latest.revision, 3);
    });
}

#[test]
fn commands_change_state_only_after_acceptance_and_repeat_targets_are_allowed() {
    block_on(async {
        let (physical, mut receiver) = ready_device();
        let device = &physical.functions()[0];
        let mut changes = device.subscribe();
        let command = device.execute(Command::On);
        futures_lite::pin!(command);
        assert!(poll_once(command.as_mut()).await.is_none());
        let request = next_power(&mut receiver).await.unwrap();
        assert!(request.target);
        assert_eq!(device.snapshot().power(), Some(Power::Off));
        device.observe(true);
        request.accept().unwrap();
        assert_eq!(command.await.unwrap().power(), Some(Power::On));
        assert_eq!(changes.changed().await.power(), Some(Power::On));

        let command = device.execute(Command::On);
        futures_lite::pin!(command);
        assert!(poll_once(command.as_mut()).await.is_none());
        device.observe(true);
        next_power(&mut receiver).await.unwrap().accept().unwrap();
        command.await.unwrap();
        assert!(poll_once(changes.changed()).await.is_none());
    });
}

#[test]
fn request_acceptance_does_not_infer_or_publish_power_state() {
    block_on(async {
        let (physical, mut receiver) = ready_device();
        let device = &physical.functions()[0];
        let mut changes = device.subscribe();
        let mut command = Box::pin(device.execute(Command::On));
        assert!(poll_once(command.as_mut()).await.is_none());
        let request = next_power(&mut receiver).await.unwrap();
        assert!(request.target);
        assert_eq!(device.snapshot().power(), Some(Power::Off));
        request.accept().unwrap();
        assert_eq!(command.await.unwrap().power(), Some(Power::Off));
        assert_eq!(device.snapshot().power(), Some(Power::Off));
        assert!(poll_once(changes.changed()).await.is_none());
        device.observe(true);
        assert_eq!(device.snapshot().power(), Some(Power::On));
        assert_eq!(changes.changed().await.power(), Some(Power::On));
    });
}

#[test]
fn rejected_commands_preserve_state_and_later_observations_still_apply() {
    block_on(async {
        let (physical, mut receiver) = ready_device();
        let device = &physical.functions()[0];
        let command = device.execute(Command::On);
        futures_lite::pin!(command);
        poll_once(command.as_mut()).await;
        next_power(&mut receiver)
            .await
            .unwrap()
            .reject(CommandError::Rejected("access denied".into()));
        assert_eq!(
            command.await,
            Err(CommandError::Rejected("access denied".into()))
        );
        assert_eq!(device.snapshot().power(), Some(Power::Off));
        device.observe(true);
        assert_eq!(device.snapshot().power(), Some(Power::On));
    });
}

#[test]
fn commands_are_fifo_and_toggle_uses_snapshot_at_execution_time() {
    block_on(async {
        let (physical, mut receiver) = ready_device();
        let device = &physical.functions()[0];
        let first = device.execute(Command::On);
        let second = device.execute(Command::Toggle);
        futures_lite::pin!(first, second);
        let before_queue = std::time::Instant::now();
        poll_once(first.as_mut()).await;
        poll_once(second.as_mut()).await;
        let after_queue = std::time::Instant::now();
        let first_request = next_power(&mut receiver).await.unwrap();
        assert!((before_queue..=after_queue).contains(&first_request.queued_at()));
        assert!(poll_once(next_power(&mut receiver)).await.is_none());
        device.observe(true);
        first_request.accept().unwrap();
        first.await.unwrap();
        // A current observation affects Toggle when its turn arrives.
        device.observe(false);
        let second_request = next_power(&mut receiver).await.unwrap();
        assert!((before_queue..=after_queue).contains(&second_request.queued_at()));
        assert!(second_request.target);
        device.observe(true);
        second_request.accept().unwrap();
        assert_eq!(second.await.unwrap().power(), Some(Power::On));
    });
}

#[test]
fn toggle_unknown_and_unavailable_commands_fail_without_sending() {
    block_on(async {
        let (physical, mut receiver) = new_device(outlet());
        let device = &physical.functions()[0];
        assert_eq!(
            device.execute(Command::On).await,
            Err(CommandError::Unavailable("initializing".into()))
        );
        physical.set_availability(Availability::Available);
        let command = device.execute(Command::Toggle);
        futures_lite::pin!(command);
        poll_once(command.as_mut()).await;
        assert!(poll_once(next_power(&mut receiver)).await.is_none());
        assert_eq!(command.await, Err(CommandError::UnknownState));
        physical.set_availability(Availability::Unavailable("offline".into()));
        assert_eq!(
            device.execute(Command::On).await,
            Err(CommandError::Unavailable("offline".into()))
        );
    });
}

#[test]
fn queued_commands_wait_for_the_active_adapter_request() {
    block_on(async {
        let (physical, mut receiver) = new_device(outlet());
        let device = &physical.functions()[0];
        device.observe(false);
        physical.set_availability(Availability::Available);
        let first = device.execute(Command::On);
        let second = device.execute(Command::Off);
        futures_lite::pin!(first, second);
        poll_once(first.as_mut()).await;
        let request = next_power(&mut receiver).await.unwrap();
        poll_once(second.as_mut()).await;
        assert!(poll_once(first.as_mut()).await.is_none());
        assert!(poll_once(second.as_mut()).await.is_none());
        device.observe(true);
        request.accept().unwrap();
        assert_eq!(first.await.unwrap().power(), Some(Power::On));
        let request = next_power(&mut receiver).await.unwrap();
        assert!(!request.target);
        device.observe(false);
        request.accept().unwrap();
        assert_eq!(second.await.unwrap().power(), Some(Power::Off));
        assert_eq!(device.snapshot().power(), Some(Power::Off));
    });
}

#[test]
fn receiver_shutdown_cancels_pending_commands_and_future_submissions() {
    block_on(async {
        let (physical, receiver) = ready_device();
        let device = &physical.functions()[0];
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
fn receiver_shutdown_cancels_active_commands_without_accepting_stale_results() {
    block_on(async {
        let (physical, mut receiver) = ready_device();
        let device = &physical.functions()[0];
        let command = device.execute(Command::On);
        futures_lite::pin!(command);
        poll_once(command.as_mut()).await;
        let request = next_power(&mut receiver).await.unwrap();
        drop(receiver);
        assert_eq!(request.mark_sent(), Err(CommandError::Cancelled));
        assert_eq!(request.accept(), Err(CommandError::Cancelled));
        assert_eq!(command.await, Err(CommandError::Cancelled));
        assert_eq!(device.snapshot().power(), Some(Power::Off));
    });
}

#[test]
fn physical_availability_notifications_do_not_follow_channel_power_changes() {
    block_on(async {
        let (physical, _receiver) = ready_device();
        let function = &physical.functions()[0];
        let mut physical_changes = physical.subscribe();
        let mut function_changes = function.subscribe();

        function.observe(true);
        assert!(poll_once(physical_changes.changed()).await.is_none());
        assert_eq!(function_changes.changed().await.power(), Some(Power::On));

        let offline = Availability::Unavailable("offline".into());
        physical.set_availability(offline.clone());
        assert_eq!(physical_changes.changed().await, offline);
        assert_eq!(function_changes.changed().await.availability, offline);
        assert!(poll_once(physical_changes.changed()).await.is_none());
    });
}

#[test]
fn active_acceptance_survives_an_offline_observation() {
    block_on(async {
        let (physical, mut receiver) = ready_device();
        let device = &physical.functions()[0];
        let mut command = Box::pin(device.execute(Command::On));
        assert!(poll_once(command.as_mut()).await.is_none());
        let request = next_power(&mut receiver).await.unwrap();
        physical.set_availability(Availability::Unavailable("offline".into()));
        assert!(poll_once(command.as_mut()).await.is_none());
        device.observe(true);
        let accepted = request.accept().unwrap();
        assert_eq!(accepted.power(), Some(Power::On));
        assert_eq!(
            accepted.availability,
            Availability::Unavailable("offline".into())
        );
        assert_eq!(command.await.unwrap(), accepted);
        assert!(matches!(
            device.execute(Command::Off).await,
            Err(CommandError::Unavailable(_))
        ));
    });
}

#[test]
fn becoming_unavailable_ends_queued_requests_but_preserves_active_acceptance() {
    block_on(async {
        let (physical, mut receiver) = ready_device();
        let device = &physical.functions()[0];
        let first = device.execute(Command::On);
        let second = device.execute(Command::On);
        futures_lite::pin!(first, second);
        poll_once(first.as_mut()).await;
        poll_once(second.as_mut()).await;
        let stale = next_power(&mut receiver).await.unwrap();
        physical.set_availability(Availability::Unavailable("connection lost".into()));
        let error = CommandError::Unavailable("connection lost".into());
        assert!(poll_once(first.as_mut()).await.is_none());
        assert_eq!(poll_once(second.as_mut()).await, Some(Err(error.clone())));
        device.observe(false);
        physical.set_availability(Availability::Available);
        device.observe(true);
        assert_eq!(stale.accept().unwrap().power(), Some(Power::On));
        assert_eq!(first.await.unwrap().power(), Some(Power::On));
        assert!(poll_once(next_power(&mut receiver)).await.is_none());
        assert_eq!(device.snapshot().power(), Some(Power::On));
    });
}

#[test]
fn dropping_a_queued_command_withdraws_it_before_the_adapter_can_send() {
    block_on(async {
        let (physical, mut receiver) = ready_device();
        let device = &physical.functions()[0];
        let mut cancelled = Box::pin(device.execute(Command::On));
        assert!(poll_once(cancelled.as_mut()).await.is_none());
        drop(cancelled);

        let mut next = Box::pin(device.execute(Command::Off));
        assert!(poll_once(next.as_mut()).await.is_none());
        let request = next_power(&mut receiver).await.unwrap();
        assert!(!request.target);
        device.observe(false);
        request.accept().unwrap();
        assert_eq!(next.await.unwrap().power(), Some(Power::Off));
        assert!(poll_once(next_power(&mut receiver)).await.is_none());
    });
}

#[test]
fn dropping_a_caller_does_not_discard_an_inflight_adapter_result() {
    block_on(async {
        let (physical, mut receiver) = ready_device();
        let device = &physical.functions()[0];
        let mut command = Box::pin(device.execute(Command::On));
        assert!(poll_once(command.as_mut()).await.is_none());
        let request = next_power(&mut receiver).await.unwrap();
        drop(command);

        device.observe(true);
        assert_eq!(request.accept().unwrap().power(), Some(Power::On));
        assert_eq!(device.snapshot().power(), Some(Power::On));
        assert!(poll_once(next_power(&mut receiver)).await.is_none());
    });
}

#[test]
fn acceptance_settles_the_current_snapshot_and_never_infers_a_target() {
    block_on(async {
        let (physical, mut receiver) = ready_device();
        let device = &physical.functions()[0];
        let mut first = Box::pin(device.execute(Command::On));
        let mut next = Box::pin(device.execute(Command::Toggle));
        assert!(poll_once(first.as_mut()).await.is_none());
        assert!(poll_once(next.as_mut()).await.is_none());
        let request = next_power(&mut receiver).await.unwrap();
        assert!(request.target);
        device.observe(false);
        let observed = device.snapshot();
        assert_eq!(request.accept().unwrap(), observed);
        assert_eq!(first.await.unwrap(), observed);
        let following = next_power(&mut receiver).await.unwrap();
        assert!(following.target);
        device.observe(true);
        following.accept().unwrap();
        assert_eq!(next.await.unwrap().power(), Some(Power::On));
    });
}

#[test]
fn acceptance_does_not_restore_a_closed_function() {
    block_on(async {
        let (physical, mut receiver) = ready_device();
        let device = &physical.functions()[0];
        let mut command = Box::pin(device.execute(Command::On));
        assert!(poll_once(command.as_mut()).await.is_none());
        let request = next_power(&mut receiver).await.unwrap();
        device.observe(false);
        let observed = device.snapshot();
        drop(receiver);
        assert_eq!(request.accept(), Err(CommandError::Cancelled));
        assert_eq!(command.await, Err(CommandError::Cancelled));
        assert_eq!(device.snapshot(), observed);
    });
}

mod lighting;

mod light_commands;

mod validity;

mod multi_device;

mod sensors;

mod states;
