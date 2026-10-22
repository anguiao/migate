use super::*;
use c::ClusterAsyncHandler as _;

fn hs_to(hue: u8, saturation: u8, duration: u16) -> Vec<u8> {
    let time = duration.to_le_bytes();
    vec![
        0x15, 0x24, 0, hue, 0x24, 1, saturation, 0x25, 2, time[0], time[1], 0x24, 3, 0, 0x24, 4, 0,
        0x18,
    ]
}

fn axis_move(mode: u8, rate: u8) -> Vec<u8> {
    vec![
        0x15, 0x24, 0, mode, 0x24, 1, rate, 0x24, 2, 0, 0x24, 3, 0, 0x18,
    ]
}

#[test]
fn hs_transition_from_temperature_ignores_the_inactive_rgb_cache() {
    let mut starts = Vec::new();
    for cached in [
        Rgb {
            red: 255,
            green: 0,
            blue: 0,
        },
        Rgb {
            red: 0,
            green: 0,
            blue: 255,
        },
    ] {
        setup!(true, function, receiver, light, clock, ctx);
        function
            .observe_light(LightPatch {
                power: Some(true),
                color: Some(cached),
                temperature_kelvin: Some(2700),
                mode: Some(LightMode::Temperature),
                ..Default::default()
            })
            .unwrap();
        block_on(async {
            let handler = ColorHandler::<true>(&light);
            let data = hs_to(170, 254, 20);
            let mut invoke = pin!(handler.handle_move_to_hue_and_saturation(
                &ctx,
                c::MoveToHueAndSaturationRequest::new(TLVElement::new(&data)),
            ));
            assert!(poll_once(invoke.as_mut()).await.is_none());
            let request = frame(&mut receiver).await;
            let Some(LightColorTarget::Rgb(start)) = request.target.color_target() else {
                panic!("expected RGB");
            };
            assert_ne!(start, cached);
            assert!(start.red > start.blue);
            assert!(start.green > 0 && start.blue > 0);
            assert!(!request.final_frame);
            starts.push(start);
            accept(&function, request);
            invoke.await.unwrap();
            assert_eq!(
                handler.color_mode(&ctx).await.unwrap(),
                c::ColorModeEnum::CurrentHueAndCurrentSaturation
            );
            clock.set(2000);
            light.advance();
            let final_request = frame(&mut receiver).await;
            assert!(final_request.final_frame);
            let Some(LightColorTarget::Rgb(end)) = final_request.target.color_target() else {
                panic!("expected RGB");
            };
            assert_eq!(end.blue, 255);
            assert!(end.red <= 5 && end.green == 0);
            accept(&function, final_request);
        });
    }
    assert_eq!(starts[0], starts[1]);
}

#[test]
fn gray_hue_motion_waits_for_source_acceptance_and_stop_preserves_the_hue_for_saturation() {
    for continuous in [false, true] {
        setup!(true, function, receiver, light, clock, ctx);
        let white = Rgb {
            red: 255,
            green: 255,
            blue: 255,
        };
        function
            .observe_light(LightPatch {
                power: Some(true),
                color: Some(white),
                mode: Some(LightMode::Rgb),
                ..Default::default()
            })
            .unwrap();
        block_on(async {
            let handler = ColorHandler::<true>(&light);
            let data = if continuous {
                axis_move(1, 60)
            } else {
                vec![
                    0x15, 0x24, 0, 120, 0x24, 1, 0, 0x25, 2, 20, 0, 0x24, 3, 0, 0x24, 4, 0, 0x18,
                ]
            };
            let mut invoke = pin!(async {
                if continuous {
                    handler
                        .handle_move_hue(&ctx, c::MoveHueRequest::new(TLVElement::new(&data)))
                        .await
                } else {
                    handler
                        .handle_move_to_hue(&ctx, c::MoveToHueRequest::new(TLVElement::new(&data)))
                        .await
                }
            });
            assert!(poll_once(invoke.as_mut()).await.is_none());
            let token = function.light_action_diagnostics().unwrap().color.unwrap();
            let request = frame(&mut receiver).await;
            assert_eq!(
                request.target.color_target(),
                Some(LightColorTarget::Rgb(white))
            );
            assert!(!request.final_frame);
            request.accept_unchanged().unwrap();
            invoke.await.unwrap();
            assert!(function.light_action_current(token));

            for (millis, hue) in [(500, 30), (1000, 60)] {
                clock.set(millis);
                light.advance();
                assert_eq!(handler.current_hue(&ctx).await.unwrap(), hue);
                assert_eq!(handler.current_saturation(&ctx).await.unwrap(), 0);
                assert_eq!(function.snapshot().lighting().unwrap().color, Some(white));
                assert!(poll_once(receiver.next_operation()).await.is_none());
            }
            let data = axis_move(0, 0);
            handler
                .handle_move_hue(&ctx, c::MoveHueRequest::new(TLVElement::new(&data)))
                .await
                .unwrap();
            assert!(!function.light_action_current(token));
            clock.set(4000);
            light.advance();
            assert_eq!(handler.current_hue(&ctx).await.unwrap(), 60);
            assert_eq!(handler.remaining_time(&ctx).await.unwrap(), 0);
            assert!(poll_once(receiver.next_operation()).await.is_none());

            let data = [
                0x15, 0x24, 0, 254, 0x25, 1, 0, 0, 0x24, 2, 0, 0x24, 3, 0, 0x18,
            ];
            let mut invoke = pin!(handler.handle_move_to_saturation(
                &ctx,
                c::MoveToSaturationRequest::new(TLVElement::new(&data)),
            ));
            assert!(poll_once(invoke.as_mut()).await.is_none());
            let request = frame(&mut receiver).await;
            let Some(LightColorTarget::Rgb(rgb)) = request.target.color_target() else {
                panic!("expected RGB");
            };
            assert_eq!((rgb.green, rgb.blue), (255, 0));
            assert!((140..=160).contains(&rgb.red));
            accept(&function, request);
            invoke.await.unwrap();
            assert!(handler.current_hue(&ctx).await.unwrap().abs_diff(60) <= 1);
            assert_eq!(handler.current_saturation(&ctx).await.unwrap(), 254);
        });
    }
}

#[test]
fn gray_hue_source_rejection_does_not_accept_or_change_representation() {
    for powered_on in [false, true] {
        setup!(true, function, receiver, light, clock, ctx);
        function
            .observe_light(LightPatch {
                power: Some(powered_on),
                color: Some(Rgb::from_packed(0xffffff).unwrap()),
                mode: Some(LightMode::Rgb),
                ..Default::default()
            })
            .unwrap();
        block_on(async {
            let handler = ColorHandler::<true>(&light);
            let before = function.snapshot();
            let mode = handler.color_mode(&ctx).await.unwrap();
            // ExecuteIfOff requires the source result even with a cached white RGB.
            let data = [0x15, 0x24, 0, 1, 0x24, 1, 60, 0x24, 2, 1, 0x24, 3, 1, 0x18];
            let mut invoke = pin!(
                handler.handle_move_hue(&ctx, c::MoveHueRequest::new(TLVElement::new(&data)),)
            );
            assert!(poll_once(invoke.as_mut()).await.is_none());
            let request = frame(&mut receiver).await;
            let token = request.token;
            assert!(!request.final_frame);
            clock.set(1000);
            light.advance();
            assert!(poll_once(invoke.as_mut()).await.is_none());
            request.reject(CommandError::Rejected("no member accepted RGB".into()));
            assert_eq!(invoke.await.unwrap_err().code(), ErrorCode::Failure);
            light.advance();
            assert!(!function.light_action_current(token));
            assert_eq!(handler.color_mode(&ctx).await.unwrap(), mode);
            assert_eq!(handler.current_hue(&ctx).await.unwrap(), 0);
            assert_eq!(function.snapshot(), before);
            assert!(poll_once(receiver.next_operation()).await.is_none());
        });
    }
}

#[test]
fn a_cancelled_hs_startup_receipt_cannot_restore_its_mode_or_gray_hue_over_a_replacement() {
    for external in [false, true] {
        for replacement_hs in [false, true] {
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
                let handler = ColorHandler::<true>(&light);
                let data = hs_to(84, 254, 0);
                let mut old = pin!(handler.handle_move_to_hue_and_saturation(
                    &ctx,
                    c::MoveToHueAndSaturationRequest::new(TLVElement::new(&data)),
                ));
                assert!(poll_once(old.as_mut()).await.is_none());
                let old_request = frame(&mut receiver).await;
                old_request.mark_sent().unwrap();
                if external {
                    function.take_over_light(LightDimension::Color).unwrap();
                    function
                        .observe_light(LightPatch {
                            color: Some(Rgb {
                                red: 0,
                                green: 0,
                                blue: 255,
                            }),
                            ..Default::default()
                        })
                        .unwrap();
                } else {
                    light.stop(LightDimension::Color).unwrap();
                }
                assert!(poll_once(old.as_mut()).await.is_none());
                let data = if replacement_hs {
                    hs_to(170, 0, 0)
                } else {
                    vec![
                        0x15, 0x25, 0, 0xCD, 0x4C, 0x25, 1, 0x9A, 0x99, 0x25, 2, 0, 0, 0x24, 3, 0,
                        0x24, 4, 0, 0x18,
                    ]
                };
                let mut replacement = pin!(async {
                    if replacement_hs {
                        handler
                            .handle_move_to_hue_and_saturation(
                                &ctx,
                                c::MoveToHueAndSaturationRequest::new(TLVElement::new(&data)),
                            )
                            .await
                    } else {
                        handler
                            .handle_move_to_color(
                                &ctx,
                                c::MoveToColorRequest::new(TLVElement::new(&data)),
                            )
                            .await
                    }
                });
                assert!(poll_once(replacement.as_mut()).await.is_none());
                assert!(poll_once(receiver.next_operation()).await.is_none());
                // The adapter preserves observations newer than this original receipt.
                old_request.accept().unwrap();
                accept(&function, frame(&mut receiver).await);
                replacement.await.unwrap();
                let expected_mode = if replacement_hs {
                    c::ColorModeEnum::CurrentHueAndCurrentSaturation
                } else {
                    c::ColorModeEnum::CurrentXAndCurrentY
                };
                let hue = handler.current_hue(&ctx).await.unwrap();
                assert_eq!(handler.color_mode(&ctx).await.unwrap(), expected_mode);
                if replacement_hs {
                    assert_eq!(hue, 170);
                }
                old.await.unwrap();
                assert_eq!(handler.color_mode(&ctx).await.unwrap(), expected_mode);
                assert_eq!(handler.current_hue(&ctx).await.unwrap(), hue);
                clock.set(10_000);
                light.advance();
                assert!(poll_once(receiver.next_operation()).await.is_none());
            });
        }
    }
}

#[test]
fn a_cancelled_hs_receipt_without_a_new_command_does_not_change_the_last_accepted_mode() {
    for external in [false, true] {
        setup!(true, function, receiver, light, clock, ctx);
        function
            .observe_light(LightPatch {
                power: Some(true),
                ..Default::default()
            })
            .unwrap();
        block_on(async {
            let handler = ColorHandler::<true>(&light);
            let data = [
                0x15, 0x25, 0, 0xCD, 0x4C, 0x25, 1, 0x9A, 0x99, 0x25, 2, 0, 0, 0x24, 3, 0, 0x24, 4,
                0, 0x18,
            ];
            let mut xy = pin!(handler.handle_move_to_color(
                &ctx,
                c::MoveToColorRequest::new(TLVElement::new(&data)),
            ));
            assert!(poll_once(xy.as_mut()).await.is_none());
            accept(&function, frame(&mut receiver).await);
            xy.await.unwrap();
            let data = hs_to(170, 254, 0);
            let mut old = pin!(handler.handle_move_to_hue_and_saturation(
                &ctx,
                c::MoveToHueAndSaturationRequest::new(TLVElement::new(&data)),
            ));
            assert!(poll_once(old.as_mut()).await.is_none());
            let old_request = frame(&mut receiver).await;
            old_request.mark_sent().unwrap();
            if external {
                function.take_over_light(LightDimension::Color).unwrap();
                function
                    .observe_light(LightPatch {
                        color: Some(Rgb {
                            red: 255,
                            green: 0,
                            blue: 0,
                        }),
                        ..Default::default()
                    })
                    .unwrap();
            } else {
                light.stop(LightDimension::Color).unwrap();
            }
            old_request.accept().unwrap();
            old.await.unwrap();
            assert_eq!(
                handler.color_mode(&ctx).await.unwrap(),
                c::ColorModeEnum::CurrentXAndCurrentY
            );
            clock.set(1000);
            light.advance();
            assert!(poll_once(receiver.next_operation()).await.is_none());
        });
    }
}

#[test]
fn an_external_rgb_change_versions_and_reports_the_hs_attributes() {
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
    light.reports(&ctx);
    ctx.changes.borrow_mut().clear();
    let version = light.report.color_version.get();
    function
        .observe_light(LightPatch {
            color: Some(Rgb {
                red: 127,
                green: 255,
                blue: 127,
            }),
            ..Default::default()
        })
        .unwrap();
    light.sync_versions();
    assert_ne!(light.report.color_version.get(), version);
    assert!(ctx.changes.borrow().is_empty());
    light.reports(&ctx);
    assert!(ctx.changes.borrow().contains(&(2, 0x300, 0)));
    assert!(ctx.changes.borrow().contains(&(2, 0x300, 1)));
    block_on(async {
        let handler = ColorHandler::<true>(&light);
        assert!(handler.current_hue(&ctx).await.unwrap().abs_diff(85) <= 1);
        assert!(
            handler
                .current_saturation(&ctx)
                .await
                .unwrap()
                .abs_diff(127)
                <= 1
        );
        assert!(poll_once(receiver.next_operation()).await.is_none());
    });
    assert_eq!(clock.now(), 0);
}
