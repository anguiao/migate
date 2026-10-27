use super::*;

#[test]
fn startup_waiter_does_not_block_ticks_merge_or_final_and_sent_stop_retains_receipt() {
    setup!(false, function, receiver, light, clock, ctx);
    function
        .observe_light(LightPatch {
            power: Some(true),
            brightness_percent: Some(1.0),
            ..Default::default()
        })
        .unwrap();
    light.refresh();
    block_on(async {
        let action = light
            .begin(
                LightingTask::Level(Task::MoveToLevel {
                    with_on_off: false,
                    start: 1,
                    target: 254,
                    transition_ms: 100,
                }),
                None,
            )
            .unwrap();
        let mut accepted = pin!(action.accepted());
        assert!(poll_once(accepted.as_mut()).await.is_none());
        let request = frame(&mut receiver).await;
        request.accept_unchanged().unwrap();
        assert!(poll_once(accepted.as_mut()).await.is_none());
        let mut run = pin!(light.run(&ctx));
        assert!(poll_once(run.as_mut()).await.is_none());
        clock.set(20);
        assert!(poll_once(run.as_mut()).await.is_none());
        let request = frame(&mut receiver).await;
        assert_eq!(request.token, action.token());
        request.mark_sent().unwrap();
        for now in [40, 80, 100] {
            clock.set(now);
            assert!(poll_once(run.as_mut()).await.is_none());
        }
        assert!(poll_once(accepted.as_mut()).await.is_none());
        request.accept().unwrap();
        accepted.await.unwrap();
        let final_request = frame(&mut receiver).await;
        assert_eq!(final_request.token, action.token());
        assert!(final_request.final_frame);
        assert_eq!(final_request.target.brightness_percent(), Some(100.0));
        accept(&function, final_request);
        assert!(poll_once(run.as_mut()).await.is_none());
        assert!(ctx.changes.borrow().contains(&(2, 8, 0)));
        let action = light
            .begin(
                LightingTask::Level(Task::Move {
                    with_on_off: false,
                    start: 254,
                    move_mode: Direction::Down,
                    rate: 1,
                }),
                None,
            )
            .unwrap();
        let request = frame(&mut receiver).await;
        request.mark_sent().unwrap();
        light.stop(LightDimension::Brightness).unwrap();
        let mut accepted = pin!(action.accepted());
        assert!(poll_once(accepted.as_mut()).await.is_none());
        request.accept().unwrap();
        accepted.await.unwrap();
        clock.set(10000);
        assert!(poll_once(run.as_mut()).await.is_none());
        assert!(poll_once(receiver.next_operation()).await.is_none());
    });
}

#[test]
fn later_failure_takeover_and_new_relative_start_keep_observed_state() {
    let (light, function, mut receiver, clock) = light_fixture(true);
    function
        .observe_light(LightPatch {
            power: Some(true),
            brightness_percent: Some(50.0),
            ..Default::default()
        })
        .unwrap();
    block_on(async {
        let action = light
            .begin(
                LightingTask::Level(Task::Move {
                    with_on_off: false,
                    start: 100,
                    move_mode: Direction::Up,
                    rate: 10,
                }),
                None,
            )
            .unwrap();
        accept(&function, frame(&mut receiver).await);
        action.accepted().await.unwrap();
        clock.set(1000);
        light.advance();
        let request = frame(&mut receiver).await;
        request.mark_sent().unwrap();
        request.reject(CommandError::Rejected("later failure".into()));
        clock.set(2000);
        light.advance();
        assert!(poll_once(receiver.next_operation()).await.is_none());
        assert!(action.ended().await.is_err());
        assert_eq!(light.remaining_time(LightDimension::Brightness), 0);
        let before = function.snapshot().lighting().unwrap().brightness_percent;
        assert_eq!(
            light.refresh().level,
            Some(crate::matter::units::level_from_brightness(before.unwrap()))
        );
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
        function
            .take_over_light(LightDimension::Brightness)
            .unwrap();
        function
            .observe_light(LightPatch {
                brightness_percent: Some(75.5),
                ..Default::default()
            })
            .unwrap();
        light.advance();
        assert!(!function.light_action_current(action.token()));
        assert!(poll_once(receiver.next_operation()).await.is_none());
        let projected = light.refresh().level.unwrap();
        let input = light.plan_input();
        let lighting_plan::Plan::Execute {
            task: LightingTask::Level(next),
        } = lighting_plan::move_level(
            false,
            Direction::Down,
            Some(2),
            lighting_plan::Options::new(0, 0),
            &input,
        )
        .unwrap()
        else {
            panic!("expected a level execution")
        };
        assert_eq!(next.frame(0).0, projected);
    });
}
