use super::*;

fn target(function: &Function) -> Option<u32> {
    function.matter_curtain().1
}

#[test]
fn stop_failures_preserve_physical_feedback_and_later_targets_take_priority() {
    let (device, mut receiver) = ready_curtain("curtain");
    let function = &device.functions()[0];
    block_on(async {
        let observe = |value| function.observe_curtain(value, None, 1);
        observe(CurtainValue::CurrentPosition(30));
        observe(CurtainValue::TargetPosition(80));
        observe(CurtainValue::Status(CurtainMotion::Opening));
        // A failed receipt cannot contradict a subsequent physical stop.
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
            assert_eq!(target(function), Some(30));
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
fn rejecting_first_stop_preserves_confirmed_feedback_and_the_later_queued_stop() {
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
        assert_eq!(target(function), Some(25));
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
fn fresh_stops_after_retarget_or_resumed_motion_follow_physical_feedback() {
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
        // Retargeting during motion still allows its final stopped transition.
        observe(CurtainValue::TargetPosition(80));
        observe(CurtainValue::Status(CurtainMotion::Stopped));
        assert_eq!(target(function), Some(25));
        request.accept().unwrap();
        first.accepted().await.unwrap();
        let request = next_curtain(&mut receiver).await;
        request.mark_sent().unwrap();
        observe(CurtainValue::Status(CurtainMotion::Stopped));
        assert_eq!(target(function), Some(25));
        request.accept().unwrap();
        second.accepted().await.unwrap();

        // Another physical stop after resuming also settles, independently of
        // which command originally started or stopped the movement.
        let fresh = function.begin_curtain(CurtainCommand::Stop).unwrap();
        let request = next_curtain(&mut receiver).await;
        request.mark_sent().unwrap();
        observe(CurtainValue::Status(CurtainMotion::Stopped));
        assert_eq!(target(function), Some(25));
        observe(CurtainValue::Status(CurtainMotion::Closing));
        observe(CurtainValue::Status(CurtainMotion::Stopped));
        assert_eq!(target(function), Some(25));
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
        assert_eq!(target(function), Some(25));
        let request = next_curtain(&mut receiver).await;
        request.mark_sent().unwrap();
        function.observe_curtain(CurtainValue::Status(CurtainMotion::Stopped), None, 1);
        assert_eq!(target(function), Some(25));
        request.accept().unwrap();
        second.accepted().await.unwrap();
    });
}

#[test]
fn external_stop_converges_in_either_position_report_order_without_writes() {
    for position_first in [false, true] {
        let (device, mut receiver) = ready_curtain("curtain");
        let function = &device.functions()[0];
        let observe = |value| function.observe_curtain(value, None, 1);
        observe(CurtainValue::CurrentPosition(50));
        observe(CurtainValue::TargetPosition(91));
        observe(CurtainValue::Status(CurtainMotion::Opening));
        if position_first {
            observe(CurtainValue::CurrentPosition(57));
            assert_eq!(target(function), Some(91));
        }
        observe(CurtainValue::Status(CurtainMotion::Stopped));
        assert_eq!(target(function), Some(if position_first { 57 } else { 50 }));
        observe(CurtainValue::CurrentPosition(57));
        assert_eq!(target(function), Some(57));
        assert_eq!(
            function.snapshot().curtain().unwrap().target_position.value,
            Some(91)
        );
        block_on(async { assert!(poll_once(receiver.next_operation()).await.is_none()) });
    }
}

#[test]
fn stopped_baselines_converge_in_any_order_and_unknown_position_stays_unknown() {
    for order in [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ] {
        let (device, _) = curtain("curtain");
        let function = &device.functions()[0];
        for field in order {
            let value = match field {
                0 => CurtainValue::CurrentPosition(57),
                1 => CurtainValue::TargetPosition(91),
                2 => CurtainValue::Status(CurtainMotion::Stopped),
                _ => unreachable!(),
            };
            function.read_curtain(value, None, 1);
            let native = function.snapshot().curtain().unwrap();
            if native.status.value == Some(CurtainMotion::Stopped) {
                assert_eq!(target(function), native.current_position.value);
            }
        }
        assert_eq!(target(function), Some(57));
        function.read_curtain(CurtainValue::TargetPosition(91), None, 2);
        assert_eq!(target(function), Some(57));
    }
}

#[test]
fn external_new_target_releases_stop_and_duplicate_stopped_reports_do_not_restore_it() {
    for next in [80, 91] {
        let (device, _) = curtain("curtain");
        let function = &device.functions()[0];
        let observe = |value| function.observe_curtain(value, None, 1);
        observe(CurtainValue::TargetPosition(91));
        observe(CurtainValue::Status(CurtainMotion::Stopped));
        assert_eq!(target(function), None);
        observe(CurtainValue::CurrentPosition(57));
        assert_eq!(target(function), Some(57));
        observe(CurtainValue::TargetPosition(next));
        observe(CurtainValue::Status(CurtainMotion::Stopped));
        observe(CurtainValue::CurrentPosition(58));
        assert_eq!(target(function), Some(next));
        observe(CurtainValue::Status(CurtainMotion::Opening));
        observe(CurtainValue::Status(CurtainMotion::Stopped));
        observe(CurtainValue::CurrentPosition(63));
        assert_eq!(target(function), Some(63));
    }
}
