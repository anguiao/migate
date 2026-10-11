use super::super::controllers::hs;
use super::*;
use crate::device::LightTarget;
use c::ClusterAsyncHandler as _;
use rs_matter::error::Error;

fn hs_to(hue: u8, saturation: u8, duration: u16) -> Vec<u8> {
    let time = duration.to_le_bytes();
    vec![
        0x15, 0x24, 0, hue, 0x24, 1, saturation, 0x25, 2, time[0], time[1], 0x24, 3, 0, 0x24, 4, 0,
        0x18,
    ]
}

fn hue_to(hue: u8, direction: u8, duration: u16) -> Vec<u8> {
    let time = duration.to_le_bytes();
    vec![
        0x15, 0x24, 0, hue, 0x24, 1, direction, 0x25, 2, time[0], time[1], 0x24, 3, 0, 0x24, 4, 0,
        0x18,
    ]
}

fn saturation_to(saturation: u8, duration: u16) -> Vec<u8> {
    let time = duration.to_le_bytes();
    vec![
        0x15, 0x24, 0, saturation, 0x25, 1, time[0], time[1], 0x24, 2, 0, 0x24, 3, 0, 0x18,
    ]
}

fn axis_move(mode: u8, rate: u8) -> Vec<u8> {
    vec![
        0x15, 0x24, 0, mode, 0x24, 1, rate, 0x24, 2, 0, 0x24, 3, 0, 0x18,
    ]
}

fn axis_step(mode: u8, size: u8, duration: u8) -> Vec<u8> {
    vec![
        0x15, 0x24, 0, mode, 0x24, 1, size, 0x24, 2, duration, 0x24, 3, 0, 0x24, 4, 0, 0x18,
    ]
}

async fn applied(
    function: &Function,
    receiver: &mut crate::device::CommandReceiver,
    invoke: impl Future<Output = Result<(), Error>>,
) -> LightTarget {
    let mut invoke = pin!(invoke);
    assert!(poll_once(invoke.as_mut()).await.is_none());
    let request = frame(receiver).await;
    let target = request.target;
    accept(function, request);
    invoke.await.unwrap();
    target
}

fn assert_near(actual: u8, expected: u8) {
    assert!(actual.abs_diff(expected) <= 1, "{actual} != {expected}");
}

#[test]
fn hs_absolute_unknown_start_and_color_mode_follow_the_accepted_command() {
    setup!(true, function, receiver, light, clock, ctx);
    function
        .observe_light(LightPatch {
            power: Some(true),
            ..Default::default()
        })
        .unwrap();
    block_on(async {
        let handler = ColorHandler::<true, true>(&light);
        assert!(handler.current_hue(&ctx).await.is_err());
        let data = hs_to(0, 254, 0);
        let target = applied(
            &function,
            &mut receiver,
            handler.handle_move_to_hue_and_saturation(
                &ctx,
                c::MoveToHueAndSaturationRequest::new(TLVElement::new(&data)),
            ),
        )
        .await;
        assert_eq!(
            target.color,
            Some(LightColorTarget::Rgb(Rgb {
                red: 255,
                green: 0,
                blue: 0
            }))
        );
        assert_eq!(handler.current_hue(&ctx).await.unwrap(), 0);
        assert_eq!(handler.current_saturation(&ctx).await.unwrap(), 254);
        assert_eq!(
            handler.color_mode(&ctx).await.unwrap(),
            c::ColorModeEnum::CurrentHueAndCurrentSaturation
        );
        assert_eq!(
            handler.enhanced_color_mode(&ctx).await.unwrap(),
            c::EnhancedColorModeEnum::CurrentHueAndCurrentSaturation
        );
        assert_eq!(handler.color_capabilities(&ctx).await.unwrap().bits(), 0x19);

        let data = [
            0x15, 0x25, 0, 0xCD, 0x4C, 0x25, 1, 0x9A, 0x99, 0x25, 2, 0, 0, 0x24, 3, 0, 0x24, 4, 0,
            0x18,
        ];
        applied(
            &function,
            &mut receiver,
            handler.handle_move_to_color(&ctx, c::MoveToColorRequest::new(TLVElement::new(&data))),
        )
        .await;
        assert_eq!(
            handler.color_mode(&ctx).await.unwrap(),
            c::ColorModeEnum::CurrentXAndCurrentY
        );
        assert_eq!(
            handler.enhanced_color_mode(&ctx).await.unwrap(),
            c::EnhancedColorModeEnum::CurrentXAndCurrentY
        );
        assert_near(handler.current_hue(&ctx).await.unwrap(), 85);

        let data = [
            0x15, 0x25, 0, 200, 0, 0x25, 1, 0, 0, 0x24, 2, 0, 0x24, 3, 0, 0x18,
        ];
        applied(
            &function,
            &mut receiver,
            handler.handle_move_to_color_temperature(
                &ctx,
                c::MoveToColorTemperatureRequest::new(TLVElement::new(&data)),
            ),
        )
        .await;
        assert_eq!(
            handler.color_mode(&ctx).await.unwrap(),
            c::ColorModeEnum::ColorTemperatureMireds
        );
        assert_eq!(
            handler.enhanced_color_mode(&ctx).await.unwrap(),
            c::EnhancedColorModeEnum::ColorTemperatureMireds
        );
        assert_eq!(clock.now(), 0);
    });
}

#[test]
fn gray_hue_selection_is_used_when_saturation_increases() {
    setup!(true, function, receiver, light, clock, ctx);
    function
        .observe_light(LightPatch {
            power: Some(true),
            ..Default::default()
        })
        .unwrap();
    block_on(async {
        let handler = ColorHandler::<true, true>(&light);
        let data = hs_to(84, 0, 0);
        applied(
            &function,
            &mut receiver,
            handler.handle_move_to_hue_and_saturation(
                &ctx,
                c::MoveToHueAndSaturationRequest::new(TLVElement::new(&data)),
            ),
        )
        .await;
        let data = hue_to(170, 0, 0);
        let target = applied(
            &function,
            &mut receiver,
            handler.handle_move_to_hue(&ctx, c::MoveToHueRequest::new(TLVElement::new(&data))),
        )
        .await;
        assert_eq!(
            target.color,
            Some(LightColorTarget::Rgb(Rgb {
                red: 255,
                green: 255,
                blue: 255
            }))
        );
        assert_eq!(handler.current_hue(&ctx).await.unwrap(), 170);
        assert_eq!(handler.current_saturation(&ctx).await.unwrap(), 0);
        let data = saturation_to(254, 0);
        let target = applied(
            &function,
            &mut receiver,
            handler.handle_move_to_saturation(
                &ctx,
                c::MoveToSaturationRequest::new(TLVElement::new(&data)),
            ),
        )
        .await;
        let Some(LightColorTarget::Rgb(rgb)) = target.color else {
            panic!("expected RGB")
        };
        assert!(rgb.red <= 5);
        assert_eq!((rgb.green, rgb.blue), (0, 255));
        assert_eq!(handler.current_hue(&ctx).await.unwrap(), 170);
        assert_eq!(clock.now(), 0);
    });
}

#[test]
fn invalid_and_ignored_hs_requests_preserve_the_existing_action() {
    setup!(true, function, receiver, light, clock, ctx);
    function
        .observe_light(LightPatch {
            power: Some(true),
            color: Some(Rgb {
                red: 255,
                green: 0,
                blue: 0,
            }),
            mode: Some(LightMode::Rgb),
            ..Default::default()
        })
        .unwrap();
    block_on(async {
        let action = light
            .begin(
                LightingTask::Color(ColorTask::Hs(hs::HsTask {
                    hue: hs::HueAxis::move_at(0.0, 10.0),
                    saturation: hs::SaturationAxis::Hold(254.0),
                })),
                None,
            )
            .unwrap();
        accept(&function, frame(&mut receiver).await);
        action.accepted().await.unwrap();
        let handler = ColorHandler::<true, true>(&light);
        for (hue, direction, duration) in [(255, 0, 0), (50, 4, 0), (50, 0, u16::MAX)] {
            let data = hue_to(hue, direction, duration);
            assert!(
                handler
                    .handle_move_to_hue(&ctx, c::MoveToHueRequest::new(TLVElement::new(&data)))
                    .await
                    .is_err()
            );
            assert!(function.light_action_current(action.token()));
        }
        let data = axis_move(1, 0);
        assert_eq!(
            handler
                .handle_move_saturation(&ctx, c::MoveSaturationRequest::new(TLVElement::new(&data)))
                .await
                .unwrap_err()
                .code(),
            ErrorCode::InvalidCommand
        );
        let data = axis_step(1, 0, 0);
        assert_eq!(
            handler
                .handle_step_hue(&ctx, c::StepHueRequest::new(TLVElement::new(&data)))
                .await
                .unwrap_err()
                .code(),
            ErrorCode::InvalidCommand
        );
        let mut data = hs_to(0, 254, 0);
        data[13] = 2;
        assert_eq!(
            handler
                .handle_move_to_hue_and_saturation(
                    &ctx,
                    c::MoveToHueAndSaturationRequest::new(TLVElement::new(&data))
                )
                .await
                .unwrap_err()
                .code(),
            ErrorCode::ConstraintError
        );
        assert!(function.light_action_current(action.token()));
        function
            .observe_light(LightPatch {
                power: Some(false),
                ..Default::default()
            })
            .unwrap();
        let data = hs_to(127, 254, 0);
        handler
            .handle_move_to_hue_and_saturation(
                &ctx,
                c::MoveToHueAndSaturationRequest::new(TLVElement::new(&data)),
            )
            .await
            .unwrap();
        assert!(function.light_action_current(action.token()));
        assert!(poll_once(receiver.next_operation()).await.is_none());
        assert_eq!(clock.now(), 0);
    });
}

#[test]
fn hue_and_saturation_commands_keep_each_others_timeline_and_stop_both_axes() {
    setup!(true, function, receiver, light, clock, ctx);
    function
        .observe_light(LightPatch {
            power: Some(true),
            color: Some(Rgb {
                red: 255,
                green: 0,
                blue: 0,
            }),
            mode: Some(LightMode::Rgb),
            ..Default::default()
        })
        .unwrap();
    block_on(async {
        let handler = ColorHandler::<true, true>(&light);
        let data = axis_move(1, 20);
        applied(
            &function,
            &mut receiver,
            handler.handle_move_hue(&ctx, c::MoveHueRequest::new(TLVElement::new(&data))),
        )
        .await;
        clock.set(1000);
        light.advance();
        accept(&function, frame(&mut receiver).await);
        let data = axis_move(3, 30);
        applied(
            &function,
            &mut receiver,
            handler.handle_move_saturation(
                &ctx,
                c::MoveSaturationRequest::new(TLVElement::new(&data)),
            ),
        )
        .await;
        clock.set(2000);
        light.advance();
        accept(&function, frame(&mut receiver).await);
        assert_near(handler.current_hue(&ctx).await.unwrap(), 40);
        assert_near(handler.current_saturation(&ctx).await.unwrap(), 224);
        let data = hue_to(127, 0, 10);
        applied(
            &function,
            &mut receiver,
            handler.handle_move_to_hue(&ctx, c::MoveToHueRequest::new(TLVElement::new(&data))),
        )
        .await;
        clock.set(3000);
        light.advance();
        let request = frame(&mut receiver).await;
        assert!(!request.final_frame);
        accept(&function, request);
        assert_near(handler.current_hue(&ctx).await.unwrap(), 127);
        assert_near(handler.current_saturation(&ctx).await.unwrap(), 194);
        let data = axis_move(0, 0);
        handler
            .handle_move_hue(&ctx, c::MoveHueRequest::new(TLVElement::new(&data)))
            .await
            .unwrap();
        clock.set(4000);
        light.advance();
        assert!(poll_once(receiver.next_operation()).await.is_none());
        assert_eq!(handler.remaining_time(&ctx).await.unwrap(), 0);
        assert_eq!(
            handler.color_mode(&ctx).await.unwrap(),
            c::ColorModeEnum::CurrentHueAndCurrentSaturation
        );

        let data = axis_move(3, 10);
        applied(
            &function,
            &mut receiver,
            handler.handle_move_hue(&ctx, c::MoveHueRequest::new(TLVElement::new(&data))),
        )
        .await;
        let data = axis_move(0, 255);
        handler
            .handle_move_saturation(&ctx, c::MoveSaturationRequest::new(TLVElement::new(&data)))
            .await
            .unwrap();
        clock.set(5000);
        light.advance();
        assert!(poll_once(receiver.next_operation()).await.is_none());
    });
}

#[test]
fn hs_stop_does_not_cancel_xy_or_temperature_actions() {
    setup!(true, function, receiver, light, clock, ctx);
    function
        .observe_light(LightPatch {
            power: Some(true),
            temperature_kelvin: Some(5000),
            color: Some(Rgb {
                red: 255,
                green: 0,
                blue: 0,
            }),
            mode: Some(LightMode::Temperature),
            ..Default::default()
        })
        .unwrap();
    block_on(async {
        let handler = ColorHandler::<true, true>(&light);
        for task in [
            ColorTask::ColorTemperatureMoveTo {
                start: 200,
                target_mireds: 250,
                transition_ms: 1000,
            },
            ColorTask::XyMoveTo {
                start_x: 41943,
                start_y: 21627,
                target_x: 19661,
                target_y: 39322,
                transition_ms: 1000,
            },
        ] {
            let action = light.begin(LightingTask::Color(task), None).unwrap();
            accept(&function, frame(&mut receiver).await);
            action.accepted().await.unwrap();
            let before = handler.color_mode(&ctx).await.unwrap();
            let data = axis_move(0, 0);
            handler
                .handle_move_hue(&ctx, c::MoveHueRequest::new(TLVElement::new(&data)))
                .await
                .unwrap();
            handler
                .handle_move_saturation(&ctx, c::MoveSaturationRequest::new(TLVElement::new(&data)))
                .await
                .unwrap();
            assert!(function.light_action_current(action.token()));
            assert_eq!(handler.color_mode(&ctx).await.unwrap(), before);
            clock.set(clock.now() + 100);
            light.advance();
            let request = frame(&mut receiver).await;
            assert_eq!(request.token, action.token());
            accept(&function, request);
            light.stop(LightDimension::Color).unwrap();
        }
    });
}

#[test]
fn hue_direction_and_steps_wrap_while_saturation_clips_for_the_full_requested_time() {
    setup!(true, function, receiver, light, clock, ctx);
    function
        .observe_light(LightPatch {
            power: Some(true),
            ..Default::default()
        })
        .unwrap();
    block_on(async {
        let handler = ColorHandler::<true, true>(&light);
        for (direction, midpoint) in [(0, 0), (1, 127), (2, 0), (3, 127)] {
            let data = hs_to(250, 254, 0);
            applied(
                &function,
                &mut receiver,
                handler.handle_move_to_hue_and_saturation(
                    &ctx,
                    c::MoveToHueAndSaturationRequest::new(TLVElement::new(&data)),
                ),
            )
            .await;
            let data = hue_to(4, direction, 10);
            applied(
                &function,
                &mut receiver,
                handler.handle_move_to_hue(&ctx, c::MoveToHueRequest::new(TLVElement::new(&data))),
            )
            .await;
            clock.set(clock.now() + 500);
            light.advance();
            accept(&function, frame(&mut receiver).await);
            let hue = handler.current_hue(&ctx).await.unwrap();
            if midpoint == 0 {
                assert!(hue <= 1 || hue >= 253);
            } else {
                assert_near(hue, midpoint);
            }
            clock.set(clock.now() + 500);
            light.advance();
            accept(&function, frame(&mut receiver).await);
            assert_near(handler.current_hue(&ctx).await.unwrap(), 4);
        }
        let data = hs_to(250, 240, 0);
        applied(
            &function,
            &mut receiver,
            handler.handle_move_to_hue_and_saturation(
                &ctx,
                c::MoveToHueAndSaturationRequest::new(TLVElement::new(&data)),
            ),
        )
        .await;
        let data = axis_step(1, 10, 10);
        applied(
            &function,
            &mut receiver,
            handler.handle_step_hue(&ctx, c::StepHueRequest::new(TLVElement::new(&data))),
        )
        .await;
        clock.set(clock.now() + 1000);
        light.advance();
        accept(&function, frame(&mut receiver).await);
        assert_near(handler.current_hue(&ctx).await.unwrap(), 6);
        let data = axis_step(1, 100, 10);
        applied(
            &function,
            &mut receiver,
            handler.handle_step_saturation(
                &ctx,
                c::StepSaturationRequest::new(TLVElement::new(&data)),
            ),
        )
        .await;
        clock.set(clock.now() + 500);
        light.advance();
        let request = frame(&mut receiver).await;
        assert!(!request.final_frame);
        accept(&function, request);
        assert_near(handler.current_saturation(&ctx).await.unwrap(), 247);
        clock.set(clock.now() + 500);
        light.advance();
        let request = frame(&mut receiver).await;
        assert!(request.final_frame);
        accept(&function, request);
        assert_eq!(handler.current_saturation(&ctx).await.unwrap(), 254);
        let data = axis_step(3, 255, 255);
        applied(
            &function,
            &mut receiver,
            handler.handle_step_hue(&ctx, c::StepHueRequest::new(TLVElement::new(&data))),
        )
        .await;
        clock.set(clock.now() + 25500);
        light.advance();
        let request = frame(&mut receiver).await;
        assert!(request.final_frame);
        accept(&function, request);
        assert_near(handler.current_hue(&ctx).await.unwrap(), 5);
    });
}
