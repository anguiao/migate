use super::*;

#[test]
fn one_inflight_frame_coalesces_ticks_and_keeps_the_final_target_and_original_waiter() {
    block_on(async {
        let (physical, mut receiver) = ready_light(true);
        let function = &physical.functions()[0];
        let action = begin(function, 20.0, false);
        let first = frame(&mut receiver).await;
        first.mark_sent().unwrap();
        for value in [30.0, 40.0] {
            function
                .submit_light_frame(action.token(), brightness(value), false)
                .unwrap();
        }
        let before_final = std::time::Instant::now();
        function
            .submit_light_frame(action.token(), brightness(70.0), true)
            .unwrap();
        let after_final = std::time::Instant::now();
        assert!(poll_once(receiver.next_operation()).await.is_none());
        assert!(poll_once(action.accepted()).await.is_none());
        first.accept().unwrap();
        assert!(action.accepted().await.is_ok());
        assert!(function.light_action_current(action.token()));
        let last = receiver.next_operation().await.unwrap();
        assert!((before_final..=after_final).contains(&last.queued_at()));
        let DeviceRequest::Light(last) = last else {
            panic!("expected the final lighting frame");
        };
        assert_eq!(last.token, action.token());
        assert_eq!(last.target, brightness(70.0));
        assert!(last.final_frame);
        last.mark_sent().unwrap();
        last.accept().unwrap();
        assert!(action.ended().await.is_ok());
        assert!(!function.light_action_current(action.token()));
        assert_eq!(
            function.snapshot().lighting().unwrap().brightness_percent,
            None
        );
        assert!(poll_once(receiver.next_operation()).await.is_none());
    });
}

#[test]
fn independent_dimensions_progress_fairly_and_temperature_rgb_share_a_slot() {
    block_on(async {
        let (physical, mut receiver) = ready_light(true);
        let function = &physical.functions()[0];
        let level = begin(function, 20.0, false);
        let first = frame(&mut receiver).await;
        first.mark_sent().unwrap();
        let white = function
            .begin_light_action(temperature(3100.0), false)
            .unwrap();
        function
            .submit_light_frame(level.token(), brightness(40.0), false)
            .unwrap();
        let red = LightTarget::rgb(
            None,
            Rgb {
                red: 255,
                green: 0,
                blue: 0,
            },
        );
        let color = function.begin_light_action(red, true).unwrap();
        assert_eq!(white.accepted().await, Err(CommandError::Superseded));
        first.accept().unwrap();
        let next = frame(&mut receiver).await;
        // The earlier retained frame progresses even while the other dimension keeps ticking.
        assert_eq!(next.token, level.token());
        next.mark_sent().unwrap();
        next.accept().unwrap();
        let next = frame(&mut receiver).await;
        assert_eq!(next.token, color.token());
        assert_eq!(next.target, red);
        next.mark_sent().unwrap();
        next.accept().unwrap();
        assert!(color.accepted().await.is_ok());
        assert!(function.light_action_current(level.token()));
    });
}

#[test]
fn final_submission_cannot_be_overwritten_by_late_ticks_and_ends_only_after_acceptance() {
    block_on(async {
        let (physical, mut receiver) = ready_light(true);
        let function = &physical.functions()[0];
        let action = begin(function, 20.0, false);
        function
            .submit_light_frame(action.token(), brightness(70.0), true)
            .unwrap();
        assert!(
            function
                .submit_light_frame(action.token(), brightness(40.0), false)
                .is_err()
        );
        assert!(function.light_action_current(action.token()));
        assert!(poll_once(action.ended()).await.is_none());
        let final_request = frame(&mut receiver).await;
        assert_eq!(final_request.target, brightness(70.0));
        final_request.mark_sent().unwrap();
        assert!(poll_once(action.ended()).await.is_none());
        final_request.accept().unwrap();
        assert!(action.ended().await.is_ok());
    });
}

#[test]
fn merging_unsent_ticks_retains_startup_power_until_a_frame_is_submitted() {
    block_on(async {
        let (physical, mut receiver) = ready_light(true);
        let function = &physical.functions()[0];
        let action = function
            .begin_light_action(brightness(20.0).with_power(Some(true)), false)
            .unwrap();
        function
            .submit_light_frame(action.token(), brightness(30.0), false)
            .unwrap();
        function
            .submit_light_frame(action.token(), brightness(70.0), true)
            .unwrap();
        let request = frame(&mut receiver).await;
        assert_eq!(request.target, brightness(70.0).with_power(Some(true)));
        request.mark_sent().unwrap();
        request.accept().unwrap();
        assert!(action.accepted().await.is_ok());
    });
}

#[test]
fn a_new_explicit_linked_power_value_overrides_the_pending_startup_value() {
    block_on(async {
        let (physical, mut receiver) = ready_light(true);
        let function = &physical.functions()[0];
        let action = function
            .begin_light_action(brightness(20.0).with_power(Some(true)), false)
            .unwrap();
        function
            .submit_light_frame(
                action.token(),
                brightness(1.0).with_power(Some(false)),
                true,
            )
            .unwrap();
        assert_eq!(frame(&mut receiver).await.target.power(), Some(false));
    });
}

#[test]
fn dequeued_startup_power_is_not_repeated_in_later_controller_ticks() {
    block_on(async {
        let (physical, mut receiver) = ready_light(true);
        let function = &physical.functions()[0];
        let action = function
            .begin_light_action(brightness(20.0).with_power(Some(true)), false)
            .unwrap();
        let first = frame(&mut receiver).await;
        assert_eq!(first.target.power(), Some(true));
        first.mark_sent().unwrap();
        function
            .submit_light_frame(action.token(), brightness(70.0), true)
            .unwrap();
        first.accept().unwrap();
        assert_eq!(frame(&mut receiver).await.target.power(), None);
    });
}

#[test]
fn newer_ticks_withdraw_dequeued_unsent_frames_without_ending_the_action() {
    for power in [None, Some(false)] {
        block_on(async {
            let (physical, mut receiver) = ready_light(true);
            let function = &physical.functions()[0];
            let action = function
                .begin_light_action(brightness(20.0).with_power(Some(true)), false)
                .unwrap();
            let old = frame(&mut receiver).await;
            function
                .submit_light_frame(
                    action.token(),
                    LightTarget::brightness(power, 70.0).expect("valid test target"),
                    true,
                )
                .unwrap();
            assert!(old.current_result().is_err());
            assert_eq!(old.mark_sent(), Err(CommandError::Superseded));
            assert!(function.light_action_current(action.token()));
            assert!(poll_once(action.accepted()).await.is_none());
            assert!(poll_once(action.ended()).await.is_none());
            old.reject(CommandError::Superseded);
            let latest = frame(&mut receiver).await;
            assert_eq!(latest.target.power(), power.or(Some(true)));
            assert_eq!(latest.target.brightness_percent(), Some(70.0));
            assert_eq!(latest.token, action.token());
            assert!(latest.final_frame);
            latest.mark_sent().unwrap();
            latest.accept().unwrap();
            action.accepted().await.unwrap();
            action.ended().await.unwrap();
        });
    }
}
