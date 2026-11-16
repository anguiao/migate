use super::*;

#[test]
fn stopping_a_sent_startup_waits_for_its_receipt_and_does_not_restore_pending_steps() {
    block_on(async {
        let (physical, mut receiver) = ready_light(true);
        let function = &physical.functions()[0];
        let action = begin(function, 20.0, false);
        let sent = frame(&mut receiver).await;
        sent.mark_sent().unwrap();
        function
            .submit_light_frame(action.token(), brightness(70.0), true)
            .unwrap();
        function.stop_light(LightDimension::Brightness).unwrap();
        assert_eq!(action.ended().await, Err(CommandError::Stopped));
        assert!(poll_once(action.accepted()).await.is_none());
        sent.accept().unwrap();
        assert!(action.accepted().await.is_ok());
        assert!(!function.light_action_current(action.token()));
        assert!(
            function
                .submit_light_frame(action.token(), brightness(70.0), true)
                .is_err()
        );
        assert!(poll_once(receiver.next_operation()).await.is_none());
    });
}

#[test]
fn partial_acceptance_updates_one_snapshot_and_failure_stops_only_the_original_action() {
    block_on(async {
        let (physical, mut receiver) = ready_light(true);
        let function = &physical.functions()[0];
        let old = function
            .begin_light_action(temperature(3100.0), false)
            .unwrap();
        let request = frame(&mut receiver).await;
        request.mark_sent().unwrap();
        let new = function
            .begin_light_action(temperature(5000.0), true)
            .unwrap();
        let revision = function.snapshot().revision;
        function
            .observe_light(LightPatch {
                power: Some(true),
                temperature_kelvin: Some(3100),
                ..LightPatch::default()
            })
            .unwrap();
        assert_eq!(function.snapshot().revision, revision + 1);
        let failure = CommandError::Rejected("mode write rejected".into());
        request.reject(failure.clone());
        assert_eq!(old.accepted().await, Err(failure));
        assert_eq!(old.ended().await, Err(CommandError::Superseded));
        assert!(function.light_action_current(new.token()));
        let request = frame(&mut receiver).await;
        request.mark_sent().unwrap();
        request.accept().unwrap();
        assert!(new.accepted().await.is_ok());
        assert_eq!(
            function.snapshot().lighting().unwrap().temperature_kelvin,
            Some(3100)
        );
    });
}

#[test]
fn later_frame_failure_ends_an_already_accepted_action_and_clears_its_final_frame() {
    block_on(async {
        let (physical, mut receiver) = ready_light(true);
        let function = &physical.functions()[0];
        let action = begin(function, 20.0, false);
        let first = frame(&mut receiver).await;
        first.mark_sent().unwrap();
        first.accept().unwrap();
        assert!(action.accepted().await.is_ok());
        function
            .submit_light_frame(action.token(), brightness(40.0), false)
            .unwrap();
        let next = frame(&mut receiver).await;
        next.mark_sent().unwrap();
        function
            .submit_light_frame(action.token(), brightness(70.0), true)
            .unwrap();
        next.reject(CommandError::Timeout);
        assert_eq!(action.ended().await, Err(CommandError::Timeout));
        assert!(!function.light_action_current(action.token()));
        assert!(poll_once(receiver.next_operation()).await.is_none());
    });
}

#[test]
fn unchanged_final_frames_accept_but_transitions_wait_for_a_write() {
    for final_frame in [false, true] {
        block_on(async {
            let (physical, mut receiver) = ready_light(true);
            let function = &physical.functions()[0];
            function
                .observe_light(LightPatch {
                    brightness_percent: Some(20.0),
                    ..LightPatch::default()
                })
                .unwrap();
            let before = function.snapshot();
            let action = begin(function, 20.0, final_frame);
            frame(&mut receiver).await.accept_unchanged().unwrap();
            assert_eq!(function.snapshot(), before);
            if final_frame {
                assert_eq!(action.accepted().await.unwrap(), before);
                assert!(action.accepted_while_current());
                assert_eq!(action.ended().await.unwrap(), before);
            } else {
                assert!(poll_once(action.accepted()).await.is_none());
                assert!(poll_once(action.ended()).await.is_none());
                function
                    .submit_light_frame(action.token(), brightness(30.0), false)
                    .unwrap();
                let changed = frame(&mut receiver).await;
                changed.mark_sent().unwrap();
                changed.reject(CommandError::Rejected(
                    "first changed frame rejected".into(),
                ));
                assert!(matches!(
                    action.accepted().await,
                    Err(CommandError::Rejected(_))
                ));
                assert!(matches!(
                    action.ended().await,
                    Err(CommandError::Rejected(_))
                ));
                assert!(!action.accepted_while_current());
                assert_eq!(function.snapshot(), before);
            }
        });
    }
}

#[test]
fn external_off_withdraws_dequeued_unsent_power_but_owns_sent_power_receipt() {
    block_on(async {
        let (physical, mut receiver) = ready_light(true);
        let function = &physical.functions()[0];
        let mut on = Box::pin(function.execute(Command::On));
        assert!(poll_once(on.as_mut()).await.is_none());
        let request = match receiver.next_operation().await.unwrap() {
            DeviceRequest::Power(request) => request,
            _ => panic!("expected power"),
        };
        function.take_over_light_all().unwrap();
        assert!(request.current_result().is_err());
        assert_eq!(request.mark_sent(), Err(CommandError::Superseded));
        assert_eq!(on.await, Err(CommandError::Superseded));
        request.reject(CommandError::Cancelled);

        let mut on = Box::pin(function.execute(Command::On));
        assert!(poll_once(on.as_mut()).await.is_none());
        let request = match receiver.next_operation().await.unwrap() {
            DeviceRequest::Power(request) => request,
            _ => panic!("expected power"),
        };
        request.mark_sent().unwrap();
        function.take_over_light_all().unwrap();
        assert!(poll_once(on.as_mut()).await.is_none());
        request.accept().unwrap();
        assert!(on.await.is_ok());
    });
}

#[test]
fn restoring_availability_does_not_release_the_original_sent_gate() {
    block_on(async {
        let (physical, mut receiver) = ready_light(true);
        let function = &physical.functions()[0];
        let old = begin(function, 20.0, false);
        let sent = frame(&mut receiver).await;
        sent.mark_sent().unwrap();
        physical.set_availability(Availability::Unavailable("no control path".into()));
        physical.set_availability(Availability::Available);
        let new = begin(function, 70.0, true);
        assert!(poll_once(receiver.next_operation()).await.is_none());
        assert!(poll_once(old.accepted()).await.is_none());
        sent.accept().unwrap();
        assert!(old.accepted().await.is_ok());
        assert!(function.light_action_current(new.token()));
        assert_eq!(frame(&mut receiver).await.token, new.token());
    });
}

#[test]
fn diagnostics_distinguish_cancelled_action_from_its_pending_original_receipt() {
    block_on(async {
        let (physical, mut receiver) = ready_light(true);
        let function = &physical.functions()[0];
        let action = begin(function, 20.0, false);
        let sent = frame(&mut receiver).await;
        assert_eq!(
            function
                .light_action_diagnostics()
                .unwrap()
                .awaiting_startup,
            None
        );
        sent.mark_sent().unwrap();
        assert_eq!(
            function
                .light_action_diagnostics()
                .unwrap()
                .awaiting_startup,
            Some(action.token())
        );
        function.stop_light(LightDimension::Brightness).unwrap();
        let stopped = function.light_action_diagnostics().unwrap();
        assert_eq!(stopped.brightness, None);
        assert_eq!(stopped.awaiting_startup, Some(action.token()));
        assert_eq!(stopped.last.unwrap().result, Err(CommandError::Stopped));
        sent.accept().unwrap();
        let ended = function.light_action_diagnostics().unwrap();
        assert_eq!(ended.awaiting_startup, None);
        assert_eq!(ended.last.unwrap().result, Err(CommandError::Stopped));
    });
}

#[test]
fn accepted_startup_is_reported_while_a_nonfinal_action_remains_current() {
    block_on(async {
        let (physical, mut receiver) = ready_light(true);
        let function = &physical.functions()[0];
        let action = begin(function, 20.0, false);
        let sent = frame(&mut receiver).await;
        sent.mark_sent().unwrap();
        sent.accept().unwrap();
        let diagnostics = function.light_action_diagnostics().unwrap();
        assert_eq!(diagnostics.brightness, Some(action.token()));
        assert_eq!(diagnostics.awaiting_startup, None);
        let result = diagnostics.last.unwrap();
        assert_eq!(result.token, action.token());
        assert_eq!(result.result, Ok(()));
    });
}

#[test]
fn startup_acceptance_ownership_survives_a_later_frame_failure() {
    block_on(async {
        let (physical, mut receiver) = ready_light(true);
        let function = &physical.functions()[0];
        let action = begin(function, 20.0, false);
        assert!(!action.accepted_while_current());
        let first = frame(&mut receiver).await;
        first.mark_sent().unwrap();
        first.accept().unwrap();
        action.accepted().await.unwrap();
        assert!(action.accepted_while_current());
        function
            .submit_light_frame(action.token(), brightness(30.0), false)
            .unwrap();
        let later = frame(&mut receiver).await;
        later.mark_sent().unwrap();
        later.reject(CommandError::Timeout);
        assert_eq!(action.ended().await, Err(CommandError::Timeout));
        assert!(action.accepted_while_current());
        assert!(!function.light_action_current(action.token()));
    });
}

#[test]
fn a_startup_receipt_after_cancellation_does_not_acquire_acceptance_ownership() {
    for external in [false, true] {
        block_on(async {
            let (physical, mut receiver) = ready_light(true);
            let function = &physical.functions()[0];
            let action = begin(function, 20.0, false);
            let first = frame(&mut receiver).await;
            first.mark_sent().unwrap();
            if external {
                function
                    .take_over_light(LightDimension::Brightness)
                    .unwrap();
            } else {
                function.stop_light(LightDimension::Brightness).unwrap();
            }
            assert!(action.ended().await.is_err());
            assert!(poll_once(action.accepted()).await.is_none());
            assert!(!action.accepted_while_current());
            first.accept().unwrap();
            action.accepted().await.unwrap();
            assert!(!action.accepted_while_current());
        });
    }
}
