use super::*;
use crate::device::CommandReceiver;
use crate::lighting::hs;
use c::ClusterAsyncHandler as _;

fn white_state(function: &Function) {
    function
        .observe_light(LightPatch {
            power: Some(true),
            brightness_percent: Some(50.0),
            color: Some(Rgb::from_packed(0xffffff).unwrap()),
            mode: Some(LightMode::Rgb),
            ..Default::default()
        })
        .unwrap();
}

fn gray_hue_move() -> ColorTask {
    ColorTask::Hs(hs::HsTask {
        hue: hs::HueAxis::move_at(0.0, 20.0),
        saturation: hs::SaturationAxis::Hold(0.0),
    })
}

fn red_target() -> ColorTask {
    ColorTask::Hs(hs::HsTask::stationary(hs::Hs {
        hue: 0.0,
        saturation: 254.0,
    }))
}

fn color_task(use_hs: bool, transition_ms: u64) -> ColorTask {
    if use_hs {
        ColorTask::Hs(hs::HsTask {
            hue: hs::HueAxis::move_to(0.0, 85.0, hs::Direction::Shortest, transition_ms),
            saturation: hs::SaturationAxis::Hold(254.0),
        })
    } else {
        ColorTask::XyMoveTo {
            start_x: 41943,
            start_y: 21627,
            target_x: 9830,
            target_y: 3932,
            transition_ms,
        }
    }
}

async fn accepted_color(
    light: &Lighting,
    function: &Function,
    receiver: &mut CommandReceiver,
    task: ColorTask,
) {
    let mut invoke = pin!(execute_color(light, task));
    assert!(poll_once(invoke.as_mut()).await.is_none());
    accept(function, frame(receiver).await);
    invoke.await.unwrap();
}

#[test]
fn accepted_color_mode_survives_dropped_hs_and_xy_invoke_waiters() {
    for use_hs in [false, true] {
        for transition_ms in [0, 1000] {
            setup!(true, function, receiver, light, clock, ctx);
            white_state(&function);
            block_on(async {
                let handler = ColorHandler::<true>(&light);
                // Establish the opposite representation with a real accepted
                // command, so the default mode cannot make the assertion pass.
                accepted_color(&light, &function, &mut receiver, color_task(!use_hs, 0)).await;
                assert_eq!(
                    handler.color_mode(&ctx).await.unwrap(),
                    if use_hs {
                        c::ColorModeEnum::CurrentXAndCurrentY
                    } else {
                        c::ColorModeEnum::CurrentHueAndCurrentSaturation
                    }
                );

                let mut invoke = Box::pin(execute_color(&light, color_task(use_hs, transition_ms)));
                assert!(poll_once(invoke.as_mut()).await.is_none());
                let startup = frame(&mut receiver).await;
                startup.mark_sent().unwrap();
                // Drop the owned future, not only a Pin<&mut _> reference.
                drop(invoke);
                accept(&function, startup);
                light.advance();
                let expected = if use_hs {
                    c::ColorModeEnum::CurrentHueAndCurrentSaturation
                } else {
                    c::ColorModeEnum::CurrentXAndCurrentY
                };
                assert_eq!(handler.color_mode(&ctx).await.unwrap(), expected);
                assert_eq!(
                    handler.enhanced_color_mode(&ctx).await.unwrap() as u8,
                    expected as u8
                );
                assert_eq!(clock.now(), 0);
            });
        }
    }
}

#[test]
fn accepted_startup_keeps_its_mode_when_a_later_frame_fails_before_invoke_resumes() {
    for use_hs in [false, true] {
        setup!(true, function, receiver, light, clock, ctx);
        white_state(&function);
        block_on(async {
            let handler = ColorHandler::<true>(&light);
            accepted_color(&light, &function, &mut receiver, color_task(!use_hs, 0)).await;
            let previous = handler.color_mode(&ctx).await.unwrap();
            let expected = if use_hs {
                c::ColorModeEnum::CurrentHueAndCurrentSaturation
            } else {
                c::ColorModeEnum::CurrentXAndCurrentY
            };
            assert_ne!(previous, expected);

            let mut invoke = Box::pin(execute_color(&light, color_task(use_hs, 1000)));
            assert!(poll_once(invoke.as_mut()).await.is_none());
            let startup = frame(&mut receiver).await;
            let token = startup.token;
            startup.mark_sent().unwrap();
            clock.set(500);
            light.advance();

            // The device accepts startup while its action still owns Color.
            // Neither the invoke future nor controller/getters run between
            // that success and the subsequent frame's failure.
            accept(&function, startup);
            let later = frame(&mut receiver).await;
            assert_eq!(later.token, token);
            assert!(!later.final_frame);
            later.mark_sent().unwrap();
            later.reject(CommandError::Rejected("later color frame rejected".into()));
            drop(invoke);

            assert!(!function.light_action_current(token));
            assert_eq!(handler.color_mode(&ctx).await.unwrap(), expected);
            assert_eq!(
                handler.enhanced_color_mode(&ctx).await.unwrap() as u8,
                expected as u8
            );
        });
    }
}

#[test]
fn gray_hue_waits_for_older_red_receipt_and_rejects_failed_white_correction() {
    setup!(true, function, receiver, light, clock, ctx);
    white_state(&function);
    block_on(async {
        let mut old_invoke = pin!(execute_color(&light, red_target()));
        assert!(poll_once(old_invoke.as_mut()).await.is_none());
        let old = frame(&mut receiver).await;
        old.mark_sent().unwrap();

        let mut new_invoke = pin!(execute_color(&light, gray_hue_move()));
        assert!(poll_once(new_invoke.as_mut()).await.is_none());
        light.advance();
        assert!(poll_once(new_invoke.as_mut()).await.is_none());
        assert!(poll_once(receiver.next_operation()).await.is_none());

        accept(&function, old);
        old_invoke.await.unwrap();
        let red = Rgb::from_packed(0xff0000).unwrap();
        assert_eq!(function.snapshot().lighting().unwrap().color, Some(red));
        clock.set(1000);
        light.advance();
        assert!(poll_once(new_invoke.as_mut()).await.is_none());
        let correction = frame(&mut receiver).await;
        assert_eq!(
            correction.target.color_target(),
            Some(LightColorTarget::Rgb(Rgb::from_packed(0xffffff).unwrap()))
        );
        correction.mark_sent().unwrap();
        correction.reject(CommandError::Rejected("white correction rejected".into()));
        assert_eq!(new_invoke.await.unwrap_err().code(), ErrorCode::Failure);
        light.advance();
        assert_eq!(function.snapshot().lighting().unwrap().color, Some(red));
        assert!(poll_once(receiver.next_operation()).await.is_none());
        assert_eq!(
            ColorHandler::<true>(&light)
                .current_hue(&ctx)
                .await
                .unwrap(),
            0
        );
    });
}

#[test]
fn gray_hue_waits_for_its_source_receipt_even_when_an_older_write_failed() {
    setup!(true, function, receiver, light, clock, ctx);
    white_state(&function);
    block_on(async {
        let mut old_invoke = pin!(execute_color(&light, red_target()));
        assert!(poll_once(old_invoke.as_mut()).await.is_none());
        let old = frame(&mut receiver).await;
        old.mark_sent().unwrap();
        let mut new_invoke = pin!(execute_color(&light, gray_hue_move()));
        assert!(poll_once(new_invoke.as_mut()).await.is_none());

        old.reject(CommandError::Rejected("old red write rejected".into()));
        assert!(old_invoke.await.is_err());
        clock.set(1000);
        light.advance();
        assert!(poll_once(new_invoke.as_mut()).await.is_none());
        let unchanged = frame(&mut receiver).await;
        assert_eq!(
            unchanged.target.color_target(),
            Some(LightColorTarget::Rgb(Rgb::from_packed(0xffffff).unwrap()))
        );
        accept(&function, unchanged);
        new_invoke.await.unwrap();
        assert_eq!(
            ColorHandler::<true>(&light)
                .current_hue(&ctx)
                .await
                .unwrap(),
            20
        );
        assert!(poll_once(receiver.next_operation()).await.is_none());
    });
}
