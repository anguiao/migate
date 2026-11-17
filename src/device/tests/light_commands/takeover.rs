use super::*;

#[test]
fn light_toggle_requires_known_state_before_entering_the_send_queue() {
    block_on(async {
        let (physical, mut receiver) = ready_light(true);
        let function = &physical.functions()[0];
        assert_eq!(
            poll_once(function.execute(Command::Toggle)).await,
            Some(Err(CommandError::UnknownState))
        );
        assert!(poll_once(receiver.next_power()).await.is_none());
    });
}

#[test]
fn invalid_new_targets_leave_the_existing_action_and_pending_frame_intact() {
    block_on(async {
        let (physical, mut receiver) = ready_light(false);
        let function = &physical.functions()[0];
        let valid = begin(function, 20.0, false);
        // Invalid numeric targets cannot be constructed at all.
        assert!(LightTarget::brightness(None, f64::NAN).is_err());
        assert!(LightTarget::brightness(None, -1.0).is_err());
        assert!(LightTarget::brightness(None, 101.0).is_err());
        assert!(LightTarget::brightness(None, 0.0).is_err());
        assert!(LightTarget::temperature(None, 0.0).is_err());
        // A valid target unsupported by this profile is rejected before takeover.
        let rgb = LightTarget::rgb(
            None,
            Rgb {
                red: 0,
                green: 255,
                blue: 0,
            },
        );
        assert!(matches!(
            function.begin_light_action(rgb, true),
            Err(CommandError::Unsupported)
        ));
        assert!(function.light_action_current(valid.token()));
        assert_eq!(frame(&mut receiver).await.token, valid.token());
    });
}

#[test]
fn explicit_unsent_startup_is_superseded_but_same_action_ticks_keep_its_waiter() {
    block_on(async {
        let (physical, mut receiver) = ready_light(true);
        let function = &physical.functions()[0];
        let old = begin(function, 20.0, true);
        let new = begin(function, 40.0, false);
        assert_eq!(old.accepted().await, Err(CommandError::Superseded));
        assert_eq!(old.ended().await, Err(CommandError::Superseded));
        function
            .submit_light_frame(new.token(), brightness(60.0), true)
            .unwrap();
        assert!(poll_once(new.accepted()).await.is_none());
        let latest = frame(&mut receiver).await;
        assert_eq!(latest.target, brightness(60.0));
        latest.mark_sent().unwrap();
        latest.accept().unwrap();
        assert!(new.accepted().await.is_ok());
        assert!(new.ended().await.is_ok());
        assert!(poll_once(receiver.next_operation()).await.is_none());
    });
}

#[test]
fn stopping_a_dequeued_unsent_frame_prevents_transmission_and_preserves_the_stopped_result() {
    block_on(async {
        let (physical, mut receiver) = ready_light(true);
        let function = &physical.functions()[0];
        let action = begin(function, 20.0, false);
        let dequeued = frame(&mut receiver).await;
        assert!(dequeued.current_result().is_ok());
        function.stop_light(LightDimension::Brightness).unwrap();
        assert!(dequeued.current_result().is_err());
        assert_eq!(dequeued.mark_sent(), Err(CommandError::Stopped));
        assert_eq!(action.accepted().await, Err(CommandError::Stopped));
        dequeued.reject(CommandError::Cancelled);
        assert_eq!(action.ended().await, Err(CommandError::Stopped));
        assert!(poll_once(receiver.next_operation()).await.is_none());
    });
}

#[test]
fn ordinary_observation_does_not_take_over_and_explicit_external_takeover_is_dimension_scoped() {
    block_on(async {
        let (physical, mut receiver) = ready_light(true);
        let function = &physical.functions()[0];
        let level = begin(function, 20.0, false);
        let color = function
            .begin_light_action(temperature(3100.0), false)
            .unwrap();
        function
            .observe_light(LightPatch {
                brightness_percent: Some(30.0),
                ..LightPatch::default()
            })
            .unwrap();
        assert!(function.light_action_current(level.token()));
        function
            .take_over_light(LightDimension::Brightness)
            .unwrap();
        function
            .observe_light(LightPatch {
                brightness_percent: Some(40.0),
                ..LightPatch::default()
            })
            .unwrap();
        assert_eq!(level.accepted().await, Err(CommandError::Superseded));
        assert!(function.light_action_current(color.token()));
        let next = frame(&mut receiver).await;
        assert_eq!(next.token, color.token());
        next.mark_sent().unwrap();
        next.accept().unwrap();
        assert_eq!(
            function.snapshot().lighting().unwrap().brightness_percent,
            Some(40.0)
        );
    });
}

#[test]
fn lamp_power_and_lighting_share_one_gate_and_valid_off_cancels_both_dimensions() {
    block_on(async {
        let (physical, mut receiver) = ready_light(true);
        let function = &physical.functions()[0];
        let level = begin(function, 20.0, false);
        let sent = frame(&mut receiver).await;
        sent.mark_sent().unwrap();
        let color = function
            .begin_light_action(temperature(3100.0), false)
            .unwrap();
        let mut off = Box::pin(function.execute(Command::Off));
        assert!(poll_once(off.as_mut()).await.is_none());
        assert!(!function.light_action_current(level.token()));
        assert!(!function.light_action_current(color.token()));
        assert_eq!(color.accepted().await, Err(CommandError::Superseded));
        assert!(poll_once(receiver.next_operation()).await.is_none());
        sent.accept().unwrap();
        let power = match receiver.next_operation().await.unwrap() {
            DeviceRequest::Power(request) => request,
            _ => panic!("expected off"),
        };
        assert!(!power.target);
        let newer = begin(function, 50.0, true);
        assert!(poll_once(receiver.next_operation()).await.is_none());
        power.accept().unwrap();
        assert!(off.await.is_ok());
        let latest = frame(&mut receiver).await;
        assert_eq!(latest.token, newer.token());
    });
}

#[test]
fn unavailable_paths_clear_pending_actions_without_finishing_sent_startup_early() {
    block_on(async {
        let (physical, mut receiver) = ready_light(true);
        let function = &physical.functions()[0];
        let sent_action = begin(function, 20.0, false);
        let sent = frame(&mut receiver).await;
        sent.mark_sent().unwrap();
        let pending = function
            .begin_light_action(temperature(3100.0), false)
            .unwrap();
        let unavailable = CommandError::Unavailable("no control path".into());
        physical.set_availability(Availability::Unavailable("no control path".into()));
        assert_eq!(pending.accepted().await, Err(unavailable.clone()));
        assert_eq!(sent_action.ended().await, Err(unavailable));
        assert!(poll_once(sent_action.accepted()).await.is_none());
        sent.accept().unwrap();
        assert!(sent_action.accepted().await.is_ok());
        physical.set_availability(Availability::Available);
        assert!(poll_once(receiver.next_operation()).await.is_none());
        assert!(!function.light_action_current(sent_action.token()));
    });
}

#[test]
fn tokens_cannot_cross_function_instances_and_close_invalidates_pending_without_revival() {
    block_on(async {
        let (old_physical, mut old_receiver) = ready_light(true);
        let old_function = &old_physical.functions()[0];
        let old_action = begin(old_function, 20.0, false);
        let old_request = frame(&mut old_receiver).await;
        old_request.mark_sent().unwrap();
        let (new_physical, mut new_receiver) = ready_light(true);
        let new_function = &new_physical.functions()[0];
        let new_action = begin(new_function, 70.0, true);
        assert!(!new_function.light_action_current(old_action.token()));
        assert!(
            new_function
                .submit_light_frame(old_action.token(), brightness(10.0), true)
                .is_err()
        );
        drop(old_receiver);
        assert_eq!(old_action.ended().await, Err(CommandError::Cancelled));
        assert!(poll_once(old_action.accepted()).await.is_none());
        old_request.accept().unwrap();
        assert!(old_action.accepted().await.is_ok());
        assert!(new_function.light_action_current(new_action.token()));
        assert_eq!(frame(&mut new_receiver).await.token, new_action.token());
    });
}

#[test]
fn dropping_a_frame_cancels_its_action_without_blocking_later_work() {
    block_on(async {
        let (physical, mut receiver) = ready_light(true);
        let function = &physical.functions()[0];
        let action = begin(function, 20.0, false);
        drop(frame(&mut receiver).await);
        assert_eq!(action.accepted().await, Err(CommandError::Cancelled));
        assert_eq!(action.ended().await, Err(CommandError::Cancelled));
        let next = begin(function, 70.0, true);
        assert_eq!(frame(&mut receiver).await.token, next.token());
    });
}

#[test]
fn pending_superseded_and_stopped_actions_wake_diagnostics_before_any_dequeue() {
    block_on(async {
        let (physical, _receiver) = ready_light(true);
        let function = &physical.functions()[0];
        let mut changes = function.subscribe_light_actions().unwrap();
        let old = begin(function, 20.0, false);
        let first = changes.changed().await;
        assert_eq!(first.brightness, Some(old.token()));
        let new = begin(function, 40.0, false);
        let superseded = changes.changed().await;
        assert_eq!(superseded.brightness, Some(new.token()));
        assert_eq!(superseded.last.as_ref().unwrap().token, old.token());
        assert_eq!(
            superseded.last.unwrap().result,
            Err(CommandError::Superseded)
        );
        function.stop_light(LightDimension::Brightness).unwrap();
        let stopped = changes.changed().await;
        assert_eq!(stopped.brightness, None);
        assert_eq!(stopped.last.as_ref().unwrap().token, new.token());
        assert_eq!(stopped.last.unwrap().result, Err(CommandError::Stopped));
        assert!(poll_once(changes.changed()).await.is_none());
    });
}

#[test]
fn zero_level_target_is_rejected_before_taking_over_an_existing_action() {
    let (physical, _receiver) = ready_light(true);
    let function = &physical.functions()[0];
    let old = begin(function, 20.0, false);
    // Zero brightness can no longer be a target at all.
    assert!(LightTarget::brightness(None, 0.0).is_err());
    assert!(LightTarget::brightness(Some(false), 0.0).is_err());
    assert!(function.light_action_current(old.token()));
}

#[test]
fn linked_off_invalidates_both_old_dimensions_and_retains_its_own_action() {
    block_on(async {
        let (physical, mut receiver) = ready_light(true);
        let function = &physical.functions()[0];
        let old = begin(function, 20.0, false);
        let color = function
            .begin_light_action(temperature(3100.0).with_power(Some(true)), false)
            .unwrap();
        let off = function
            .begin_light_action(brightness(1.0).with_power(Some(false)), true)
            .unwrap();
        assert_eq!(old.accepted().await, Err(CommandError::Superseded));
        assert!(!function.light_action_current(color.token()));
        assert_eq!(color.accepted().await, Err(CommandError::Superseded));
        assert!(function.light_action_current(off.token()));
        assert_eq!(frame(&mut receiver).await.token, off.token());
    });
}

#[test]
fn linked_off_tick_cancels_the_other_dimension_and_preserves_its_token() {
    block_on(async {
        let (physical, mut receiver) = ready_light(true);
        let function = &physical.functions()[0];
        let level = begin(function, 20.0, false);
        let color = function
            .begin_light_action(temperature(3100.0), false)
            .unwrap();
        function
            .submit_light_frame(level.token(), brightness(1.0).with_power(Some(false)), true)
            .unwrap();
        assert!(!function.light_action_current(color.token()));
        assert_eq!(color.accepted().await, Err(CommandError::Superseded));
        assert!(function.light_action_current(level.token()));
        assert_eq!(frame(&mut receiver).await.token, level.token());
    });
}

#[test]
fn a_new_valid_off_withdraws_older_pending_and_dequeued_unsent_light_power_on() {
    block_on(async {
        let (physical, mut receiver) = ready_light(true);
        let function = &physical.functions()[0];
        let mut on = Box::pin(function.execute(Command::On));
        assert!(poll_once(on.as_mut()).await.is_none());
        let mut off = Box::pin(function.execute(Command::Off));
        assert!(poll_once(off.as_mut()).await.is_none());
        assert_eq!(
            poll_once(on.as_mut()).await,
            Some(Err(CommandError::Superseded))
        );
        let request = match receiver.next_operation().await.unwrap() {
            DeviceRequest::Power(request) => request,
            _ => panic!("expected power"),
        };
        assert!(!request.target);
        request.accept().unwrap();
        off.await.unwrap();

        let mut on = Box::pin(function.execute(Command::On));
        assert!(poll_once(on.as_mut()).await.is_none());
        let request = match receiver.next_operation().await.unwrap() {
            DeviceRequest::Power(request) => request,
            _ => panic!("expected power"),
        };
        let mut off = Box::pin(function.execute(Command::Off));
        assert!(poll_once(off.as_mut()).await.is_none());
        assert!(request.current_result().is_err());
        assert_eq!(request.mark_sent(), Err(CommandError::Superseded));
        assert_eq!(on.await, Err(CommandError::Superseded));
        request.reject(CommandError::Cancelled);
        let request = match receiver.next_operation().await.unwrap() {
            DeviceRequest::Power(request) => request,
            _ => panic!("expected power"),
        };
        assert!(!request.target);
        request.accept().unwrap();
        off.await.unwrap();
    });
}

#[test]
fn unknown_toggle_does_not_cancel_either_existing_light_dimension() {
    block_on(async {
        let (physical, _receiver) = ready_light(true);
        let function = &physical.functions()[0];
        let level = begin(function, 20.0, false);
        let color = function
            .begin_light_action(temperature(3100.0), false)
            .unwrap();
        assert_eq!(
            function.execute(Command::Toggle).await,
            Err(CommandError::UnknownState)
        );
        assert!(function.light_action_current(level.token()));
        assert!(function.light_action_current(color.token()));
    });
}

#[test]
fn action_tokens_do_not_cross_different_device_keys() {
    let (old_physical, _old_receiver) = ready_light(true);
    let old = begin(&old_physical.functions()[0], 20.0, false);
    let old_description = old_physical.description();
    let description = PhysicalDeviceDescription::new(
        DeviceKey::new("xiaomi", "different-light"),
        old_description.label(),
        old_description.functions().to_vec(),
    )
    .unwrap();
    let (new_physical, _new_receiver) = new_device(description);
    new_physical.set_availability(Availability::Available);
    let new_function = &new_physical.functions()[0];
    let new = begin(new_function, 70.0, true);
    assert!(!new_function.light_action_current(old.token()));
    assert!(
        new_function
            .submit_light_frame(old.token(), brightness(50.0), true)
            .is_err()
    );
    assert!(new_function.light_action_current(new.token()));
}
