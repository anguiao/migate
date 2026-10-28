use super::*;

#[test]
fn curtain_unsent_targets_are_replaced_and_stop_is_a_barrier() {
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
