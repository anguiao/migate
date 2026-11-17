use crate::device::{
    Availability, Command, CommandError, DeviceKey, FunctionDescription, FunctionKind,
    PhysicalDevice, PhysicalDeviceDescription, Power,
};
use futures_lite::future::{block_on, poll_once};

fn description(id: &str) -> PhysicalDeviceDescription {
    PhysicalDeviceDescription::new(
        DeviceKey::new("xiaomi", id),
        "三路插座",
        ["outlet_1", "outlet_2", "usb"]
            .into_iter()
            .map(|id| FunctionDescription {
                id: id.into(),
                label: id.into(),
                kind: FunctionKind::Outlet,
            })
            .collect(),
    )
    .unwrap()
}

#[test]
fn physical_availability_is_shared_but_power_revisions_and_queues_are_independent() {
    block_on(async {
        let (physical, mut receivers) = PhysicalDevice::new(description("multi"));
        let first = physical.function("outlet_1").unwrap();
        let second = physical.function("outlet_2").unwrap();
        physical.set_availability(Availability::Available);
        first.observe(false);
        assert_eq!(second.snapshot().power(), Some(Power::Unknown));
        assert_eq!(first.function_key().id, "outlet_1");
        assert_eq!(first.function_key().device, *physical.key());
        let revision = second.snapshot().revision;
        let mut first_command = Box::pin(first.execute(Command::On));
        let mut second_command = Box::pin(second.execute(Command::Off));
        assert!(poll_once(first_command.as_mut()).await.is_none());
        assert!(poll_once(second_command.as_mut()).await.is_none());
        let pending = receivers[0].next_power().await.unwrap();
        second.observe(false);
        receivers[1].next_power().await.unwrap().accept().unwrap();
        assert_eq!(second_command.await.unwrap().power(), Some(Power::Off));
        assert_eq!(second.snapshot().revision, revision + 1);
        assert_eq!(first.snapshot().power(), Some(Power::Off));
        first.observe(true);
        pending.accept().unwrap();
        first_command.await.unwrap();
        assert_eq!(second.snapshot().power(), Some(Power::Off));
        let mut changes = second.subscribe();
        physical.set_availability(Availability::Unavailable("offline".into()));
        assert_eq!(
            changes.changed().await.availability,
            Availability::Unavailable("offline".into())
        );
        assert_eq!(
            first.snapshot().availability,
            second.snapshot().availability
        );
    });
}

#[test]
fn closing_one_device_cancels_only_its_commands() {
    block_on(async {
        let (first, mut first_receivers) = PhysicalDevice::new(description("first"));
        let (second, mut second_receivers) = PhysicalDevice::new(description("second"));
        first.set_availability(Availability::Available);
        second.set_availability(Availability::Available);
        assert_eq!(first.functions().len(), 3);
        let function = first.function("usb").unwrap();
        let other = second.function("usb").unwrap();
        let mut command = Box::pin(function.execute(Command::On));
        let mut other_command = Box::pin(other.execute(Command::Off));
        poll_once(command.as_mut()).await;
        poll_once(other_command.as_mut()).await;
        let stale = first_receivers[2].next_power().await.unwrap();
        first.close();
        assert_eq!(command.await, Err(CommandError::Cancelled));
        assert_eq!(stale.accept(), Err(CommandError::Cancelled));
        assert_eq!(
            function.execute(Command::On).await,
            Err(CommandError::Cancelled)
        );
        second.function("usb").unwrap().observe(false);
        second_receivers[2]
            .next_power()
            .await
            .unwrap()
            .accept()
            .unwrap();
        assert_eq!(other_command.await.unwrap().power(), Some(Power::Off));
    });
}

#[test]
fn cancelling_one_channel_does_not_withdraw_another_channel() {
    block_on(async {
        let (physical, mut receivers) = PhysicalDevice::new(description("multi"));
        physical.set_availability(Availability::Available);
        let first = physical.function("outlet_1").unwrap();
        let second = physical.function("outlet_2").unwrap();
        let mut cancelled = Box::pin(first.execute(Command::On));
        let mut surviving = Box::pin(second.execute(Command::Off));
        poll_once(cancelled.as_mut()).await;
        poll_once(surviving.as_mut()).await;
        drop(cancelled);
        assert!(poll_once(receivers[0].next_power()).await.is_none());
        second.observe(false);
        receivers[1].next_power().await.unwrap().accept().unwrap();
        assert_eq!(surviving.await.unwrap().power(), Some(Power::Off));
    });
}

#[test]
fn closing_a_device_wakes_and_ends_its_waiting_receivers() {
    block_on(async {
        let (device, mut receivers) = PhysicalDevice::new(description("removed"));
        let (_other, mut other_receivers) = PhysicalDevice::new(description("remaining"));
        let mut waiting = Box::pin(receivers[0].next_power());
        assert!(poll_once(waiting.as_mut()).await.is_none());
        device.close();
        assert!(waiting.await.is_none());
        assert!(poll_once(other_receivers[0].next_power()).await.is_none());
    });
}
