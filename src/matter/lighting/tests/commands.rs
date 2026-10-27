use super::*;

#[test]
fn ct_wire_zero_clips_and_step_keeps_requested_rate_at_boundary() {
    use c::ClusterAsyncHandler as _;
    setup!(false, function, receiver, light, clock, ctx);
    function
        .observe_light(LightPatch {
            power: Some(true),
            temperature_kelvin: Some(5000),
            mode: Some(LightMode::Temperature),
            ..Default::default()
        })
        .unwrap();
    block_on(async {
        let handler = ColorHandler::<false>(&light);
        let data = TLVElement::new(&[
            0x15, 0x25, 0, 0, 0, 0x25, 1, 0, 0, 0x24, 2, 0, 0x24, 3, 0, 0x18,
        ]);
        let mut invoke =
            pin!(handler.handle_move_to_color_temperature(
                &ctx,
                c::MoveToColorTemperatureRequest::new(data)
            ));
        assert!(poll_once(invoke.as_mut()).await.is_none());
        let request = frame(&mut receiver).await;
        assert_eq!(
            request.target.color_target(),
            Some(LightColorTarget::Temperature(1_000_000.0 / 154.0))
        );
        accept(&function, request);
        invoke.await.unwrap();
        // Starting at 200 mireds, 100 mireds over 10 seconds reaches 250 in five seconds.
        function
            .observe_light(LightPatch {
                temperature_kelvin: Some(5000),
                ..Default::default()
            })
            .unwrap();
        let data = TLVElement::new(&[
            0x15, 0x24, 0, 1, 0x25, 1, 100, 0, 0x25, 2, 100, 0, 0x25, 3, 0, 0, 0x25, 4, 250, 0,
            0x24, 5, 0, 0x24, 6, 0, 0x18,
        ]);
        let mut invoke = pin!(
            handler.handle_step_color_temperature(&ctx, c::StepColorTemperatureRequest::new(data))
        );
        assert!(poll_once(invoke.as_mut()).await.is_none());
        let request = frame(&mut receiver).await;
        accept(&function, request);
        invoke.await.unwrap();
        clock.set(5000);
        light.advance();
        let request = frame(&mut receiver).await;
        assert!(request.final_frame);
        assert_eq!(
            request.target.color_target(),
            Some(LightColorTarget::Temperature(4000.0))
        );
    });
}

#[test]
fn malformed_stop_with_onoff_does_not_cancel_active_action() {
    use l::ClusterAsyncHandler as _;
    setup!(false, function, receiver, light, clock, ctx);
    function
        .observe_light(LightPatch {
            power: Some(true),
            brightness_percent: Some(50.0),
            ..Default::default()
        })
        .unwrap();
    let action = light
        .begin(
            LightingTask::Level(Task::Move {
                with_on_off: false,
                start: 100,
                move_mode: Direction::Up,
                rate: 1,
            }),
            None,
        )
        .unwrap();
    block_on(async {
        let result = LevelHandler(&light)
            .handle_stop_with_on_off(
                &ctx,
                l::StopWithOnOffRequest::new(TLVElement::new(&[0x15, 0x18])),
            )
            .await;
        assert!(result.is_err());
        assert!(function.light_action_current(action.token()));
        assert!(frame(&mut receiver).await.is_current());
    });
    assert_eq!(clock.now(), 0);
}

#[test]
fn ordinary_onoff_honors_on_level_and_repeated_off_cancels_without_reapplying_preset() {
    use crate::device::Command;
    let (light, function, mut receiver, clock) = light_fixture(false);
    function
        .observe_light(LightPatch {
            power: Some(false),
            brightness_percent: Some(60.0),
            ..Default::default()
        })
        .unwrap();
    light.on_level.set(Some(1));
    block_on(async {
        let mut on = pin!(light.power(Command::On));
        assert!(poll_once(on.as_mut()).await.is_none());
        let request = frame(&mut receiver).await;
        assert_eq!(request.target.power(), Some(true));
        assert_eq!(request.target.brightness_percent(), Some(1.0));
        accept(&function, request);
        on.await.unwrap();
        function
            .observe_light(LightPatch {
                brightness_percent: Some(60.0),
                ..Default::default()
            })
            .unwrap();
        let mut repeated = pin!(light.power(Command::On));
        assert!(poll_once(repeated.as_mut()).await.is_none());
        match receiver.next_operation().await.unwrap() {
            DeviceRequest::Power(request) => {
                request.accept().unwrap();
            }
            _ => panic!("repeated on reapplied preset"),
        };
        repeated.await.unwrap();
        assert_eq!(
            function.snapshot().lighting().unwrap().brightness_percent,
            Some(60.0)
        );
        let mut off = pin!(light.power(Command::Off));
        assert!(poll_once(off.as_mut()).await.is_none());
        let request = frame(&mut receiver).await;
        assert_eq!(request.target.power(), Some(false));
        assert_eq!(request.target.brightness_percent(), Some(1.0));
        accept(&function, request);
        off.await.unwrap();
        function
            .observe_light(LightPatch {
                brightness_percent: Some(30.0),
                ..Default::default()
            })
            .unwrap();
        let old = light
            .begin(
                LightingTask::Level(Task::Move {
                    with_on_off: false,
                    start: 75,
                    move_mode: Direction::Up,
                    rate: 1,
                }),
                None,
            )
            .unwrap();
        let mut repeated = pin!(light.power(Command::Off));
        assert!(poll_once(repeated.as_mut()).await.is_none());
        assert!(!function.light_action_current(old.token()));
        match receiver.next_operation().await.unwrap() {
            DeviceRequest::Power(request) => {
                request.accept().unwrap();
            }
            _ => panic!("repeated off reapplied preset"),
        };
        repeated.await.unwrap();
        assert_eq!(
            function.snapshot().lighting().unwrap().brightness_percent,
            Some(30.0)
        );
        light.on_level.set(None);
        function
            .observe_light(LightPatch {
                power: Some(true),
                ..Default::default()
            })
            .unwrap();
        let mut off = pin!(light.power(Command::Off));
        assert!(poll_once(off.as_mut()).await.is_none());
        match receiver.next_operation().await.unwrap() {
            DeviceRequest::Power(request) => {
                request.accept().unwrap();
            }
            _ => panic!("null on level changed brightness"),
        };
        off.await.unwrap();
    });
    assert_eq!(clock.now(), 0);
}

#[test]
fn ct_stop_parses_all_fields_before_cancellation_and_ignores_motion_only_bounds() {
    use c::ClusterAsyncHandler as _;
    setup!(false, function, receiver, light, clock, ctx);
    function
        .observe_light(LightPatch {
            power: Some(true),
            temperature_kelvin: Some(5000),
            ..Default::default()
        })
        .unwrap();
    let action = light
        .begin(
            LightingTask::Color(ColorTask::ColorTemperatureMove {
                start: 200,
                rate_mireds_per_sec: 1,
                min_mireds: 154,
                max_mireds: 370,
            }),
            None,
        )
        .unwrap();
    block_on(async {
        for bytes in [
            &[
                0x15, 0x24, 0, 0, 0x25, 1, 0, 0, 0x24, 4, 0, 0x24, 5, 0, 0x18,
            ][..],
            &[
                0x15, 0x24, 0, 0, 0x25, 1, 0, 0, 0x29, 2, 0x25, 3, 0, 0, 0x24, 4, 0, 0x24, 5, 0,
                0x18,
            ][..],
        ] {
            let result = ColorHandler::<false>(&light)
                .handle_move_color_temperature(
                    &ctx,
                    c::MoveColorTemperatureRequest::new(TLVElement::new(bytes)),
                )
                .await;
            assert!(result.is_err());
            assert!(function.light_action_current(action.token()));
        }
        let bytes = [
            0x15, 0x24, 0, 0, 0x25, 1, 0, 0, 0x25, 2, 250, 0, 0x25, 3, 200, 0, 0x24, 4, 0, 0x24, 5,
            0, 0x18,
        ];
        ColorHandler::<false>(&light)
            .handle_move_color_temperature(
                &ctx,
                c::MoveColorTemperatureRequest::new(TLVElement::new(&bytes)),
            )
            .await
            .unwrap();
        assert!(!function.light_action_current(action.token()));
        assert!(poll_once(receiver.next_operation()).await.is_none());
    });
    assert_eq!(clock.now(), 0);
}

#[test]
fn clipped_ct_step_reaches_two_mired_boundary_in_two_milliseconds_and_equal_target_is_final() {
    use c::ClusterAsyncHandler as _;
    setup!(true, function, receiver, light, clock, ctx);
    function
        .observe_light(LightPatch {
            power: Some(true),
            temperature_kelvin: Some(5000),
            mode: Some(LightMode::Rgb),
            ..Default::default()
        })
        .unwrap();
    block_on(async {
        let handler = ColorHandler::<true>(&light);
        let bytes = [
            0x15, 0x24, 0, 1, 0x25, 1, 100, 0, 0x25, 2, 1, 0, 0x25, 3, 0, 0, 0x25, 4, 202, 0, 0x24,
            5, 0, 0x24, 6, 0, 0x18,
        ];
        let mut invoke = pin!(handler.handle_step_color_temperature(
            &ctx,
            c::StepColorTemperatureRequest::new(TLVElement::new(&bytes))
        ));
        assert!(poll_once(invoke.as_mut()).await.is_none());
        let first = frame(&mut receiver).await;
        assert!(!first.final_frame);
        accept(&function, first);
        invoke.await.unwrap();
        let mut run = pin!(light.run(&ctx));
        assert!(poll_once(run.as_mut()).await.is_none());
        clock.set(2);
        assert!(poll_once(run.as_mut()).await.is_none());
        let final_request = frame(&mut receiver).await;
        assert!(final_request.final_frame);
        assert_eq!(
            final_request.target.color_target(),
            Some(LightColorTarget::Temperature(1_000_000.0 / 202.0))
        );
        accept(&function, final_request);
        function
            .observe_light(LightPatch {
                mode: Some(LightMode::Rgb),
                ..Default::default()
            })
            .unwrap();
        let bytes = [
            0x15, 0x25, 0, 202, 0, 0x25, 1, 100, 0, 0x24, 2, 0, 0x24, 3, 0, 0x18,
        ];
        let mut invoke = pin!(handler.handle_move_to_color_temperature(
            &ctx,
            c::MoveToColorTemperatureRequest::new(TLVElement::new(&bytes))
        ));
        assert!(poll_once(invoke.as_mut()).await.is_none());
        let request = frame(&mut receiver).await;
        assert!(request.final_frame);
        accept(&function, request);
        invoke.await.unwrap();
        assert_eq!(
            function.snapshot().lighting().unwrap().mode,
            Some(LightMode::Temperature)
        );
    });
}

#[test]
fn unsupported_coupling_command_override_is_rejected_before_takeover() {
    use l::ClusterAsyncHandler as _;
    setup!(false, function, receiver, light, clock, ctx);
    function
        .observe_light(LightPatch {
            power: Some(true),
            brightness_percent: Some(50.0),
            ..Default::default()
        })
        .unwrap();
    let action = light
        .begin(
            LightingTask::Level(Task::Move {
                with_on_off: false,
                start: 100,
                move_mode: Direction::Up,
                rate: 1,
            }),
            None,
        )
        .unwrap();
    block_on(async {
        let data = [
            0x15, 0x24, 0, 150, 0x25, 1, 0, 0, 0x24, 2, 2, 0x24, 3, 2, 0x18,
        ];
        let handler = LevelHandler(&light);
        let mut invoke = pin!(
            handler.handle_move_to_level(&ctx, l::MoveToLevelRequest::new(TLVElement::new(&data)))
        );
        let result = poll_once(invoke.as_mut())
            .await
            .expect("unsupported option must fail before queuing");
        assert_eq!(result.unwrap_err().code(), ErrorCode::ConstraintError);
        let result = handler
            .handle_move_to_level_with_on_off(
                &ctx,
                l::MoveToLevelWithOnOffRequest::new(TLVElement::new(&data)),
            )
            .await;
        assert_eq!(result.unwrap_err().code(), ErrorCode::ConstraintError);
        assert!(function.light_action_current(action.token()));
        assert!(frame(&mut receiver).await.is_current());
    });
    assert_eq!(clock.now(), 0);
}

#[test]
fn ignored_ct_step_still_validates_required_bounds_before_success() {
    use c::ClusterAsyncHandler as _;
    setup!(false, function, receiver, light, clock, ctx);
    function
        .observe_light(LightPatch {
            power: Some(false),
            temperature_kelvin: Some(5000),
            ..Default::default()
        })
        .unwrap();
    let action = light
        .begin(
            LightingTask::Color(ColorTask::ColorTemperatureMove {
                start: 200,
                rate_mireds_per_sec: 1,
                min_mireds: 154,
                max_mireds: 370,
            }),
            None,
        )
        .unwrap();
    block_on(async {
        for bytes in [
            &[
                0x15, 0x24, 0, 1, 0x25, 1, 1, 0, 0x25, 2, 0, 0, 0x24, 5, 0, 0x24, 6, 0, 0x18,
            ][..],
            &[
                0x15, 0x24, 0, 1, 0x25, 1, 1, 0, 0x25, 2, 0, 0, 0x29, 3, 0x25, 4, 0, 0, 0x24, 5, 0,
                0x24, 6, 0, 0x18,
            ][..],
            &[
                0x15, 0x24, 0, 1, 0x25, 1, 1, 0, 0x25, 2, 0, 0, 0x25, 3, 0, 0, 0x25, 4, 0, 255,
                0x24, 5, 0, 0x24, 6, 0, 0x18,
            ][..],
        ] {
            let result = ColorHandler::<false>(&light)
                .handle_step_color_temperature(
                    &ctx,
                    c::StepColorTemperatureRequest::new(TLVElement::new(bytes)),
                )
                .await;
            assert!(result.is_err());
            assert!(function.light_action_current(action.token()));
        }
        let bytes = [
            0x15, 0x24, 0, 1, 0x25, 1, 1, 0, 0x25, 2, 0, 0, 0x25, 3, 44, 1, 0x25, 4, 200, 0, 0x24,
            5, 0, 0x24, 6, 0, 0x18,
        ];
        ColorHandler::<false>(&light)
            .handle_step_color_temperature(
                &ctx,
                c::StepColorTemperatureRequest::new(TLVElement::new(&bytes)),
            )
            .await
            .unwrap();
        assert!(function.light_action_current(action.token()));
        assert!(frame(&mut receiver).await.is_current());
    });
    assert_eq!(clock.now(), 0);
}
