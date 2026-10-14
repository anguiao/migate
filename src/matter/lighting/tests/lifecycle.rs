//! Matter entry waiters and reports use the shared executor.
//! The Matter model owns shutdown cancellation.

use super::*;
use crate::device::{Command, DeviceRequest};
use crate::lighting::LightingExecutor;

fn controller(function: &Function) -> (Lighting, Rc<LightingExecutor>, Rc<TestClock>) {
    let clock = Rc::new(TestClock::default());
    let executor = LightingExecutor::new(clock.clone());
    let light = Lighting::new(
        Light::Single(function.clone()),
        2,
        Dataver::new(1),
        Dataver::new(2),
        Dataver::new(3),
        crate::matter::publication::map_light(function.description()).unwrap(),
        executor.clone(),
    );
    (light, executor, clock)
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
fn dropping_the_invoke_waiter_does_not_cancel_the_committed_action() {
    let (device, mut receiver) = support::light(false);
    device.set_availability(Availability::Available);
    let function = device.functions()[0].clone();
    let (light, executor, clock) = controller(&function);
    function
        .observe_light(LightPatch {
            power: Some(true),
            brightness_percent: Some(1.0),
            ..Default::default()
        })
        .unwrap();
    block_on(async {
        let action = light.begin(level_task(1000), None).unwrap();
        let token = action.token();
        drop(action);
        let startup = frame(&mut receiver).await;
        assert!(startup.is_current());
        startup.mark_sent().unwrap();
        startup.accept().unwrap();

        clock.set(1000);
        executor.advance();
        let last = frame(&mut receiver).await;
        assert_eq!(last.token, token);
        assert!(last.final_frame);
        last.mark_sent().unwrap();
        last.accept().unwrap();
        assert!(!function.light_action_current(token));
        assert!(executor.ending(function.function_key(), LightDimension::Brightness));
    });
}

#[test]
fn handler_report_loop_never_waits_on_a_past_action_deadline() {
    setup!(false, function, receiver, light, clock, ctx);
    function
        .observe_light(LightPatch {
            power: Some(true),
            brightness_percent: Some(1.0),
            ..Default::default()
        })
        .unwrap();
    block_on(async {
        let action = light.begin(level_task(1000), None).unwrap();
        let startup = frame(&mut receiver).await;
        startup.mark_sent().unwrap();
        startup.accept().unwrap();
        action.accepted().await.unwrap();
        // Move the clock past the committed deadline without advancing the
        // executor. A report loop that waits on the lapsed deadline would
        // spin forever; with a forward cadence it parks.
        clock.set(5000);
        let mut run = pin!(light.run(&ctx));
        assert!(poll_once(run.as_mut()).await.is_none());
        assert_eq!(light.level_state().remaining_time.value, 0);
    });
}

#[test]
fn power_waiter_is_woken_by_device_accept_or_reject_without_a_runner() {
    for reject in [false, true] {
        let (device, mut receiver) = support::light(false);
        device.set_availability(Availability::Available);
        let function = device.functions()[0].clone();
        let (light, _executor, _clock) = controller(&function);
        block_on(async {
            let mut on = pin!(light.power(Command::On));
            assert!(poll_once(on.as_mut()).await.is_none());
            let DeviceRequest::Power(request) = receiver.next_operation().await.unwrap() else {
                panic!("expected a power dispatch");
            };
            request.mark_sent().unwrap();
            if reject {
                request.reject(CommandError::Rejected("power rejected".into()));
                assert!(on.await.is_err());
            } else {
                request.accept().unwrap();
                on.await.unwrap();
                assert_eq!(function.snapshot().power.value(), Some(true));
            }
        });
        // No runner tick was needed: the caller waker polls the
        // executor-owned future at the physical acceptance boundary.
        drop(device);
    }
}

#[test]
fn matter_power_waiter_drop_leaves_dispatch_with_the_executor() {
    let (device, mut receiver) = support::light(false);
    device.set_availability(Availability::Available);
    let function = device.functions()[0].clone();
    let (light, executor, _clock) = controller(&function);
    block_on(async {
        let mut on = Box::pin(light.power(Command::On));
        assert!(poll_once(on.as_mut()).await.is_none());
        drop(on);
        let DeviceRequest::Power(request) = receiver.next_operation().await.unwrap() else {
            panic!("expected a power dispatch");
        };
        request.mark_sent().unwrap();
        request.accept().unwrap();
        executor.advance();
        assert_eq!(function.snapshot().power.value(), Some(true));
    });
}
