use futures_lite::future::{block_on, poll_once};
use migate::device::{
    Availability, Capability, Category, Command, CommandError, DeviceKey, DeviceRegistry,
    FunctionDescription, PhysicalDevice, PhysicalDeviceDescription, Power,
};

fn description(id: &str) -> PhysicalDeviceDescription {
    PhysicalDeviceDescription {
        key: DeviceKey::new("xiaomi", id),
        label: "三路插座".into(),
        functions: ["outlet_1", "outlet_2", "usb"]
            .into_iter()
            .map(|id| FunctionDescription {
                id: id.into(),
                label: id.into(),
                category: Category::Outlet,
                capabilities: vec![Capability::OnOff],
            })
            .collect(),
    }
}

#[test]
fn physical_availability_is_shared_but_power_revisions_and_queues_are_independent() {
    block_on(async {
        let (physical, mut receivers) = PhysicalDevice::new(description("multi"));
        let first = physical.function("outlet_1").unwrap();
        let second = physical.function("outlet_2").unwrap();
        physical.set_availability(Availability::Available);
        first.observe(false);
        assert_eq!(second.snapshot().power, Power::Unknown);
        assert_eq!(first.function_key().id, "outlet_1");
        assert_eq!(first.function_key().device, *physical.key());
        let revision = second.snapshot().revision;
        let mut first_command = Box::pin(first.execute(Command::On));
        let mut second_command = Box::pin(second.execute(Command::Off));
        assert!(poll_once(first_command.as_mut()).await.is_none());
        assert!(poll_once(second_command.as_mut()).await.is_none());
        let pending = receivers[0].next().await.unwrap();
        receivers[1].next().await.unwrap().accept().unwrap();
        assert_eq!(second_command.await.unwrap().power, Power::Off);
        assert_eq!(second.snapshot().revision, revision + 1);
        assert_eq!(first.snapshot().power, Power::Off);
        pending.accept().unwrap();
        first_command.await.unwrap();
        assert_eq!(second.snapshot().power, Power::Off);
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
fn registry_publishes_whole_devices_and_removal_cancels_only_their_commands() {
    block_on(async {
        let registry = DeviceRegistry::new();
        let mut changes = registry.subscribe();
        let (first, mut first_receivers) = PhysicalDevice::new(description("first"));
        let (second, mut second_receivers) = PhysicalDevice::new(description("second"));
        first.set_availability(Availability::Available);
        second.set_availability(Availability::Available);
        registry.insert(first.clone());
        let snapshot = changes.changed().await;
        assert_eq!(snapshot.devices.len(), 1);
        assert_eq!(snapshot.devices[0].functions().len(), 3);
        registry.insert(second.clone());
        let function = first.function("usb").unwrap();
        let other = second.function("usb").unwrap();
        let mut command = Box::pin(function.execute(Command::On));
        let mut other_command = Box::pin(other.execute(Command::Off));
        poll_once(command.as_mut()).await;
        poll_once(other_command.as_mut()).await;
        let stale = first_receivers[2].next().await.unwrap();
        registry.remove(first.key()).unwrap();
        assert_eq!(command.await, Err(CommandError::Cancelled));
        assert_eq!(stale.accept(), Err(CommandError::Cancelled));
        assert_eq!(
            function.execute(Command::On).await,
            Err(CommandError::Cancelled)
        );
        assert!(registry.get(first.key()).is_none());
        assert_eq!(changes.changed().await.devices.len(), 1);
        second_receivers[2].next().await.unwrap().accept().unwrap();
        assert_eq!(other_command.await.unwrap().power, Power::Off);
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
        assert!(poll_once(receivers[0].next()).await.is_none());
        receivers[1].next().await.unwrap().accept().unwrap();
        assert_eq!(surviving.await.unwrap().power, Power::Off);
    });
}

#[test]
fn registry_removal_wakes_and_ends_waiting_receivers() {
    block_on(async {
        let registry = DeviceRegistry::new();
        let (device, mut receivers) = PhysicalDevice::new(description("removed"));
        let (other, mut other_receivers) = PhysicalDevice::new(description("remaining"));
        registry.insert(device.clone());
        registry.insert(other);
        let mut waiting = Box::pin(receivers[0].next());
        assert!(poll_once(waiting.as_mut()).await.is_none());
        registry.remove(device.key()).unwrap();
        assert!(waiting.await.is_none());
        assert!(poll_once(other_receivers[0].next()).await.is_none());
    });
}
