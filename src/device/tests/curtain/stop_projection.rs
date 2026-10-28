use super::*;

fn target(function: &Function) -> Option<u32> {
    function.matter_curtain().1
}

#[test]
fn stop_failure_and_unsent_barriers_do_not_overwrite_later_target() {
    let (device, mut receiver) = ready_curtain("curtain");
    let function = &device.functions()[0];
    block_on(async {
        let observe = |value| function.observe_curtain(value, None, 1);
        observe(CurtainValue::CurrentPosition(30));
        observe(CurtainValue::TargetPosition(80));
        observe(CurtainValue::Status(CurtainMotion::Opening));
        // A sent stop can report failure, after which an unrelated stop does not
        // replace the native target. A timeout is treated the same way.
        for error in [
            CommandError::Rejected("denied".into()),
            CommandError::Timeout,
        ] {
            let stop = function.begin_curtain(CurtainCommand::Stop).unwrap();
            let request = next_curtain(&mut receiver).await;
            request.mark_sent().unwrap();
            request.reject(error.clone());
            assert_eq!(stop.accepted().await.unwrap_err(), error);
            observe(CurtainValue::Status(CurtainMotion::Stopped));
            assert_eq!(target(function), Some(80));
        }
        // A stop behind an in-flight target must still be transmitted, but it
        // cannot reactivate stale stop handling after a later target is queued.
        let old = function
            .begin_curtain(CurtainCommand::Target(60.0))
            .unwrap();
        let inflight = next_curtain(&mut receiver).await;
        inflight.mark_sent().unwrap();
        let stop = function.begin_curtain(CurtainCommand::Stop).unwrap();
        let newer = function
            .begin_curtain(CurtainCommand::Target(50.0))
            .unwrap();
        inflight.accept().unwrap();
        old.accepted().await.unwrap();
        let request = next_curtain(&mut receiver).await;
        assert_eq!(request.command, CurtainCommand::Stop);
        request.mark_sent().unwrap();
        observe(CurtainValue::Status(CurtainMotion::Stopped));
        observe(CurtainValue::CurrentPosition(31));
        assert_eq!(target(function), Some(80));
        request.accept().unwrap();
        stop.accepted().await.unwrap();
        let request = next_curtain(&mut receiver).await;
        request.mark_sent().unwrap();
        request.accept().unwrap();
        newer.accepted().await.unwrap();
    });
}

#[test]
fn later_rejected_target_still_retires_previous_stop_projection() {
    let (device, mut receiver) = ready_curtain("curtain");
    let function = &device.functions()[0];
    block_on(async {
        function.observe_curtain(CurtainValue::CurrentPosition(25), None, 1);
        function.observe_curtain(CurtainValue::TargetPosition(80), None, 1);
        let stop = function.begin_curtain(CurtainCommand::Stop).unwrap();
        let request = next_curtain(&mut receiver).await;
        request.mark_sent().unwrap();
        request.accept().unwrap();
        stop.accepted().await.unwrap();
        function.observe_curtain(CurtainValue::Status(CurtainMotion::Stopped), None, 1);
        assert_eq!(target(function), Some(25));
        device.set_availability(Availability::Unavailable("offline".into()));
        assert!(
            function
                .begin_curtain(CurtainCommand::Target(10.0))
                .is_err()
        );
        assert_eq!(target(function), Some(80));
        function.observe_curtain(CurtainValue::Status(CurtainMotion::Stopped), None, 1);
        assert_eq!(target(function), Some(80));
    });
}

#[test]
fn stop_feedback_accepts_either_report_order_and_unknown_current() {
    let (device, mut receiver) = ready_curtain("curtain");
    let function = &device.functions()[0];
    block_on(async {
        function.observe_curtain(CurtainValue::TargetPosition(80), None, 1);
        let stop = function.begin_curtain(CurtainCommand::Stop).unwrap();
        let request = next_curtain(&mut receiver).await;
        request.mark_sent().unwrap();
        request.accept().unwrap();
        stop.accepted().await.unwrap();
        // Receipt alone is not a physical stop.
        assert_eq!(target(function), Some(80));
        function.observe_curtain(CurtainValue::Status(CurtainMotion::Stopped), None, 1);
        assert!(target(function).is_none());
        function.observe_curtain(CurtainValue::CurrentPosition(44), None, 1);
        assert_eq!(target(function), Some(44));

        function.observe_curtain(CurtainValue::Status(CurtainMotion::Opening), None, 1);
        let stop = function.begin_curtain(CurtainCommand::Stop).unwrap();
        let request = next_curtain(&mut receiver).await;
        request.mark_sent().unwrap();
        request.accept().unwrap();
        stop.accepted().await.unwrap();
        function.observe_curtain(CurtainValue::CurrentPosition(50), None, 1);
        assert_eq!(target(function), Some(80));
        function.observe_curtain(CurtainValue::Status(CurtainMotion::Stopped), None, 1);
        assert_eq!(target(function), Some(50));
    });
}

#[test]
fn repeating_stop_keeps_confirmed_position_until_a_new_target_or_motion() {
    let (device, mut receiver) = ready_curtain("curtain");
    let function = &device.functions()[0];
    block_on(async {
        function.observe_curtain(CurtainValue::CurrentPosition(25), None, 1);
        function.observe_curtain(CurtainValue::TargetPosition(80), None, 1);
        let first = function.begin_curtain(CurtainCommand::Stop).unwrap();
        let request = next_curtain(&mut receiver).await;
        request.mark_sent().unwrap();
        function.observe_curtain(CurtainValue::Status(CurtainMotion::Stopped), None, 1);
        assert_eq!(target(function), Some(25));
        let second = function.begin_curtain(CurtainCommand::Stop).unwrap();
        assert_eq!(target(function), Some(25));
        request.accept().unwrap();
        first.accepted().await.unwrap();
        let request = next_curtain(&mut receiver).await;
        request.mark_sent().unwrap();
        request.reject(CommandError::Timeout);
        assert_eq!(second.accepted().await.unwrap_err(), CommandError::Timeout);
        assert_eq!(target(function), Some(25));
        function.observe_curtain(CurtainValue::Status(CurtainMotion::Opening), None, 1);
        assert_eq!(target(function), Some(80));
    });
}

#[test]
fn rejecting_first_stop_preserves_the_later_queued_stop() {
    let (device, mut receiver) = ready_curtain("curtain");
    let function = &device.functions()[0];
    block_on(async {
        function.observe_curtain(CurtainValue::CurrentPosition(25), None, 1);
        function.observe_curtain(CurtainValue::TargetPosition(80), None, 1);
        let first = function.begin_curtain(CurtainCommand::Stop).unwrap();
        let second = function.begin_curtain(CurtainCommand::Stop).unwrap();
        let request = next_curtain(&mut receiver).await;
        request.mark_sent().unwrap();
        function.observe_curtain(CurtainValue::Status(CurtainMotion::Stopped), None, 1);
        assert_eq!(target(function), Some(25));
        request.reject(CommandError::Timeout);
        assert_eq!(first.accepted().await.unwrap_err(), CommandError::Timeout);
        assert_eq!(target(function), Some(80));
        let request = next_curtain(&mut receiver).await;
        assert_eq!(request.command, CurtainCommand::Stop);
        request.mark_sent().unwrap();
        function.observe_curtain(CurtainValue::Status(CurtainMotion::Stopped), None, 1);
        assert_eq!(target(function), Some(25));
        request.accept().unwrap();
        second.accepted().await.unwrap();
    });
}

#[test]
fn later_native_target_and_resumed_motion_retire_older_stop_work() {
    let (device, mut receiver) = ready_curtain("curtain");
    let function = &device.functions()[0];
    block_on(async {
        let observe = |value| function.observe_curtain(value, Some(1), 1);
        observe(CurtainValue::CurrentPosition(25));
        observe(CurtainValue::TargetPosition(80));
        observe(CurtainValue::Status(CurtainMotion::Opening));
        let first = function.begin_curtain(CurtainCommand::Stop).unwrap();
        let second = function.begin_curtain(CurtainCommand::Stop).unwrap();
        let request = next_curtain(&mut receiver).await;
        request.mark_sent().unwrap();
        // Even an identical native target report retires both older Stops.
        observe(CurtainValue::TargetPosition(80));
        observe(CurtainValue::Status(CurtainMotion::Stopped));
        assert_eq!(target(function), Some(80));
        request.accept().unwrap();
        first.accepted().await.unwrap();
        let request = next_curtain(&mut receiver).await;
        request.mark_sent().unwrap();
        observe(CurtainValue::Status(CurtainMotion::Stopped));
        assert_eq!(target(function), Some(80));
        request.accept().unwrap();
        second.accepted().await.unwrap();

        // A fresh Stop can still settle, and resumed motion prevents an old
        // duplicate stopped report from taking over again.
        let fresh = function.begin_curtain(CurtainCommand::Stop).unwrap();
        let request = next_curtain(&mut receiver).await;
        request.mark_sent().unwrap();
        observe(CurtainValue::Status(CurtainMotion::Stopped));
        assert_eq!(target(function), Some(25));
        observe(CurtainValue::Status(CurtainMotion::Closing));
        observe(CurtainValue::Status(CurtainMotion::Stopped));
        assert_eq!(target(function), Some(80));
        request.accept().unwrap();
        fresh.accepted().await.unwrap();
    });
}

#[test]
fn unsent_stop_failure_leaves_the_next_stop_usable() {
    let (device, mut receiver) = ready_curtain("curtain");
    let function = &device.functions()[0];
    block_on(async {
        function.observe_curtain(CurtainValue::CurrentPosition(25), None, 1);
        function.observe_curtain(CurtainValue::TargetPosition(80), None, 1);
        let first = function.begin_curtain(CurtainCommand::Stop).unwrap();
        let second = function.begin_curtain(CurtainCommand::Stop).unwrap();
        let request = next_curtain(&mut receiver).await;
        assert!(matches!(request.accept(), Err(CommandError::SendFailed(_))));
        assert!(matches!(
            first.accepted().await,
            Err(CommandError::SendFailed(_))
        ));
        function.observe_curtain(CurtainValue::Status(CurtainMotion::Stopped), None, 1);
        assert_eq!(target(function), Some(80));
        let request = next_curtain(&mut receiver).await;
        request.mark_sent().unwrap();
        function.observe_curtain(CurtainValue::Status(CurtainMotion::Stopped), None, 1);
        assert_eq!(target(function), Some(25));
        request.accept().unwrap();
        second.accepted().await.unwrap();
    });
}
