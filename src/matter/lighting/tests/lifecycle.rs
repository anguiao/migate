use super::*;

fn controller(function: &Function) -> Lighting {
    Lighting::with_clock(
        function.clone(),
        2,
        Dataver::new(1),
        Dataver::new(2),
        Dataver::new(3),
        crate::matter::publication::map_light(function.description()).unwrap(),
        Rc::new(TestClock::default()),
    )
}

fn level_task(transition_ms: u64) -> LightingTask {
    LightingTask::Level(Task::MoveToLevel {
        with_on_off: false,
        start: 100,
        target: 200,
        transition_ms,
    })
}

#[test]
fn dropping_controller_cancels_owned_actions_without_replaying_sent_frames() {
    for sent in [false, true] {
        let (device, mut receiver) = support::light(true);
        device.set_availability(Availability::Available);
        let function = &device.functions()[0];
        let light = controller(function);
        let level = light.begin(level_task(1000), None).unwrap();
        let color = light
            .begin(
                LightingTask::Color(ColorTask::XyMove {
                    start_x: 20000,
                    start_y: 20000,
                    rate_x: 100,
                    rate_y: 0,
                }),
                None,
            )
            .unwrap();
        block_on(async {
            let request = frame(&mut receiver).await;
            if sent {
                request.mark_sent().unwrap();
            }
            drop(light);
            for action in [&level, &color] {
                assert!(!function.light_action_current(action.token()));
                assert_eq!(action.ended().await, Err(CommandError::Cancelled));
            }
            assert_eq!(color.accepted().await, Err(CommandError::Cancelled));
            if sent {
                assert!(poll_once(level.accepted()).await.is_none());
                assert_eq!(
                    function
                        .light_action_diagnostics()
                        .unwrap()
                        .awaiting_startup,
                    Some(level.token())
                );
                request.accept().unwrap();
                level.accepted().await.unwrap();
            } else {
                assert_eq!(request.mark_sent(), Err(CommandError::Cancelled));
                assert_eq!(level.accepted().await, Err(CommandError::Cancelled));
                drop(request);
            }
            let new = controller(function);
            assert_eq!(new.level_state().remaining_time.value, 0);
            assert_eq!(new.color_state().remaining_time.value, 0);
            new.advance();
            assert!(poll_once(receiver.next_operation()).await.is_none());
            assert!(
                function
                    .light_action_diagnostics()
                    .unwrap()
                    .awaiting_startup
                    .is_none()
            );
        });
    }
}

#[test]
fn dropping_old_controller_and_settling_its_receipt_preserves_the_new_action() {
    let (device, mut receiver) = support::light(false);
    device.set_availability(Availability::Available);
    let function = &device.functions()[0];
    let old = controller(function);
    let old_action = old.begin(level_task(1000), None).unwrap();
    block_on(async {
        let old_request = frame(&mut receiver).await;
        old_request.mark_sent().unwrap();
        let new = controller(function);
        let new_action = new.begin(level_task(0), None).unwrap();
        drop(old);
        assert_eq!(old_action.ended().await, Err(CommandError::Superseded));
        assert!(function.light_action_current(new_action.token()));
        old_request.accept().unwrap();
        assert!(function.light_action_current(new_action.token()));
        let request = frame(&mut receiver).await;
        assert_eq!(request.token, new_action.token());
        accept(function, request);
        new_action.ended().await.unwrap();
        assert!(poll_once(receiver.next_operation()).await.is_none());
    });
}
