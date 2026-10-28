use crate::device::{
    CurtainObservation, DeviceKey, FunctionDescription, FunctionKind, PhysicalDevice,
    PhysicalDeviceDescription,
};

impl crate::device::FunctionSnapshot {
    pub(crate) fn curtain(&self) -> Option<crate::device::CurtainSnapshot> {
        match self.state {
            crate::device::FunctionState::Curtain { curtain } => Some(curtain),
            _ => None,
        }
    }
}

#[test]
fn curtain_ignores_power_observations_without_publishing_a_change() {
    let description = PhysicalDeviceDescription::new(
        DeviceKey::new("xiaomi", "curtain"),
        "Curtain",
        vec![FunctionDescription {
            id: "main".into(),
            label: "Curtain".into(),
            kind: FunctionKind::Curtain,
        }],
    )
    .unwrap();
    let (device, _) = PhysicalDevice::new(description);
    let function = &device.functions()[0];
    let original = function.snapshot();
    let curtain = original.curtain().unwrap();
    let unknown = CurtainObservation {
        value: None,
        source_time: None,
        received_at: None,
    };
    assert_eq!(curtain.current_position, unknown);
    assert_eq!(curtain.target_position, unknown);
    assert_eq!(curtain.status.value, None);
    assert_eq!(curtain.status.source_time, None);
    assert_eq!(curtain.status.received_at, None);
    function.observe(true);
    assert_eq!(function.snapshot(), original);
}

#[test]
fn curtain_observations_update_each_value_and_only_value_changes_advance_revision() {
    use crate::device::{CurtainMotion, CurtainValue};

    let description = PhysicalDeviceDescription::new(
        DeviceKey::new("xiaomi", "curtain"),
        "Curtain",
        vec![FunctionDescription {
            id: "main".into(),
            label: "Curtain".into(),
            kind: FunctionKind::Curtain,
        }],
    )
    .unwrap();
    let (device, _) = PhysicalDevice::new(description);
    let function = &device.functions()[0];
    let initial = function.snapshot();
    function.observe_curtain(CurtainValue::CurrentPosition(37), Some(11), 12);
    let current = function.snapshot();
    assert_eq!(current.revision, initial.revision + 1);
    assert_eq!(current.curtain().unwrap().current_position.value, Some(37));
    assert_eq!(current.curtain().unwrap().target_position.value, None);
    function.observe_curtain(CurtainValue::CurrentPosition(37), None, 13);
    let repeated = function.snapshot();
    assert_eq!(repeated.revision, current.revision);
    assert_eq!(
        repeated.curtain().unwrap().current_position.source_time,
        None
    );
    assert_eq!(
        repeated.curtain().unwrap().current_position.received_at,
        Some(13)
    );
    function.observe_curtain(CurtainValue::TargetPosition(80), Some(14), 15);
    function.observe_curtain(CurtainValue::Status(CurtainMotion::Opening), Some(16), 17);
    let values = function.snapshot().curtain().unwrap();
    assert_eq!(values.current_position.value, Some(37));
    assert_eq!(values.target_position.value, Some(80));
    assert_eq!(values.status.value, Some(CurtainMotion::Opening));
}

fn ready_curtain(id: &str) -> (PhysicalDevice, crate::device::CommandReceiver) {
    let description = PhysicalDeviceDescription::new(
        DeviceKey::new("xiaomi", id),
        "Curtain",
        vec![FunctionDescription {
            id: "main".into(),
            label: "Curtain".into(),
            kind: FunctionKind::Curtain,
        }],
    )
    .unwrap();
    let (device, mut receivers) = PhysicalDevice::new(description);
    device.set_availability(crate::device::Availability::Available);
    (device, receivers.remove(0))
}

async fn next_curtain(
    receiver: &mut crate::device::CommandReceiver,
) -> crate::device::CurtainRequest {
    match receiver.next_operation().await.unwrap() {
        crate::device::DeviceRequest::Curtain(request) => request,
        _ => panic!("expected curtain request"),
    }
}

#[test]
fn curtain_unsent_targets_are_replaced_and_stop_is_a_barrier() {
    use crate::device::{CommandError, CurtainCommand};
    use futures_lite::future::{block_on, poll_once};

    block_on(async {
        let (device, mut receiver) = ready_curtain("one");
        let function = &device.functions()[0];
        let first = function
            .begin_curtain(CurtainCommand::Target(10.0))
            .unwrap();
        let dequeued = next_curtain(&mut receiver).await;
        let second = function
            .begin_curtain(CurtainCommand::Target(20.0))
            .unwrap();
        assert_eq!(dequeued.current_result(), Err(CommandError::Superseded));
        assert_eq!(dequeued.mark_sent(), Err(CommandError::Superseded));
        assert_eq!(first.accepted().await, Err(CommandError::Superseded));
        let third = function
            .begin_curtain(CurtainCommand::Target(30.0))
            .unwrap();
        assert_eq!(second.accepted().await, Err(CommandError::Superseded));
        let stop = function.begin_curtain(CurtainCommand::Stop).unwrap();
        assert_eq!(third.accepted().await, Err(CommandError::Superseded));
        let after = function
            .begin_curtain(CurtainCommand::Target(40.0))
            .unwrap();
        drop(dequeued);

        let request = next_curtain(&mut receiver).await;
        assert_eq!(request.command, CurtainCommand::Stop);
        request.mark_sent().unwrap();
        assert!(poll_once(receiver.next_operation()).await.is_none());
        request.accept().unwrap();
        stop.accepted().await.unwrap();
        let request = next_curtain(&mut receiver).await;
        assert_eq!(request.command, CurtainCommand::Target(40.0));
        request.mark_sent().unwrap();
        request.accept().unwrap();
        after.accepted().await.unwrap();
    });
}

#[test]
fn curtain_sent_receipt_keeps_gate_and_never_changes_observations() {
    use crate::device::{CurtainCommand, CurtainMotion, CurtainValue};
    use futures_lite::future::{block_on, poll_once};

    block_on(async {
        let (device, mut receiver) = ready_curtain("one");
        let function = &device.functions()[0];
        function.observe_curtain(CurtainValue::CurrentPosition(20), Some(1), 2);
        function.observe_curtain(CurtainValue::TargetPosition(80), Some(1), 2);
        function.observe_curtain(CurtainValue::Status(CurtainMotion::Opening), Some(1), 2);
        let before = function.snapshot();
        let first = function
            .begin_curtain(CurtainCommand::Target(60.0))
            .unwrap();
        let sent = next_curtain(&mut receiver).await;
        sent.mark_sent().unwrap();
        let replaced = function
            .begin_curtain(CurtainCommand::Target(70.0))
            .unwrap();
        let stop = function.begin_curtain(CurtainCommand::Stop).unwrap();
        let after = function
            .begin_curtain(CurtainCommand::Target(90.0))
            .unwrap();
        assert_eq!(
            replaced.accepted().await,
            Err(crate::device::CommandError::Superseded)
        );
        assert!(poll_once(receiver.next_operation()).await.is_none());
        sent.accept().unwrap();
        first.accepted().await.unwrap();
        assert_eq!(function.snapshot(), before);
        let stopping = next_curtain(&mut receiver).await;
        assert_eq!(stopping.command, CurtainCommand::Stop);
        stopping.mark_sent().unwrap();
        assert!(poll_once(receiver.next_operation()).await.is_none());
        stopping.accept().unwrap();
        stop.accepted().await.unwrap();
        assert_eq!(function.snapshot(), before);
        let target = next_curtain(&mut receiver).await;
        assert_eq!(target.command, CurtainCommand::Target(90.0));
        target.mark_sent().unwrap();
        target.accept().unwrap();
        after.accepted().await.unwrap();
        assert_eq!(function.snapshot(), before);
    });
}

#[test]
fn curtain_lifecycle_cancels_unsent_work_and_devices_are_independent() {
    use crate::device::{Availability, CommandError, CurtainCommand};
    use futures_lite::future::{block_on, poll_once};

    block_on(async {
        let (one, mut first_receiver) = ready_curtain("one");
        let (two, mut second_receiver) = ready_curtain("two");
        let first = one.functions()[0]
            .begin_curtain(CurtainCommand::Target(20.0))
            .unwrap();
        let sent = next_curtain(&mut first_receiver).await;
        sent.mark_sent().unwrap();
        let pending = one.functions()[0]
            .begin_curtain(CurtainCommand::Stop)
            .unwrap();
        let other = two.functions()[0]
            .begin_curtain(CurtainCommand::Target(40.0))
            .unwrap();
        let independent = next_curtain(&mut second_receiver).await;
        independent.mark_sent().unwrap();
        independent.accept().unwrap();
        other.accepted().await.unwrap();
        one.set_availability(Availability::Unavailable("offline".into()));
        assert!(matches!(
            pending.accepted().await,
            Err(CommandError::Unavailable(_))
        ));
        assert!(poll_once(first.accepted()).await.is_none());
        sent.accept().unwrap();
        first.accepted().await.unwrap();
        one.set_availability(Availability::Available);
        assert!(poll_once(first_receiver.next_operation()).await.is_none());
        let queued = one.functions()[0]
            .begin_curtain(CurtainCommand::Target(50.0))
            .unwrap();
        let unsent = next_curtain(&mut first_receiver).await;
        one.set_availability(Availability::Unavailable("offline".into()));
        assert!(matches!(
            queued.accepted().await,
            Err(CommandError::Unavailable(_))
        ));
        assert!(matches!(
            unsent.mark_sent(),
            Err(CommandError::Unavailable(_))
        ));
        one.set_availability(Availability::Available);
        drop(unsent);
        assert!(poll_once(first_receiver.next_operation()).await.is_none());
    });
}

#[test]
fn curtain_rejects_power_and_invalid_target_commands() {
    use crate::device::{Command, CommandError, CurtainCommand};
    use futures_lite::future::block_on;

    block_on(async {
        let (device, _receiver) = ready_curtain("one");
        let function = &device.functions()[0];
        for command in [Command::On, Command::Off, Command::Toggle] {
            assert_eq!(
                function.execute(command).await,
                Err(CommandError::Unsupported)
            );
        }
        for target in [-1.0, 101.0, f64::NAN, f64::INFINITY] {
            assert!(matches!(
                function.begin_curtain(CurtainCommand::Target(target)),
                Err(CommandError::Rejected(_))
            ));
        }
    });
}
