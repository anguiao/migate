use crate::device::{
    Availability, Capability, Category, CommandReceiver, DeviceKey, DeviceRequest, Function,
    FunctionDescription, LightDimension, LightPatch, NumericRange, PhysicalDevice,
    PhysicalDeviceDescription,
};
use crate::lighting::{Scheduler, Task, clock::Clock, level};
use futures_lite::future::{block_on, poll_once};
use std::{cell::Cell, future::Future, pin::Pin, rc::Rc};

#[derive(Default)]
struct TestClock {
    millis: Cell<u64>,
    changed: event_listener::Event,
}

impl TestClock {
    fn set(&self, millis: u64) {
        self.millis.set(millis);
        self.changed.notify(usize::MAX);
    }
}

impl Clock for TestClock {
    fn now(&self) -> u64 {
        self.millis.get()
    }
    fn wait_until(&self, deadline: u64) -> Pin<Box<dyn Future<Output = ()> + '_>> {
        Box::pin(async move {
            loop {
                let changed = self.changed.listen();
                if self.now() >= deadline {
                    return;
                }
                changed.await;
            }
        })
    }
}

fn temperature_range() -> NumericRange {
    NumericRange {
        min: 2700,
        max: 6500,
        step: 1,
    }
}

fn light_device() -> (Function, CommandReceiver) {
    let description = PhysicalDeviceDescription {
        key: DeviceKey::new("test", "shared-light"),
        label: "Shared Light".into(),
        functions: vec![FunctionDescription {
            id: "main".into(),
            label: "Main light".into(),
            category: Category::Light,
            capabilities: vec![
                Capability::OnOff,
                Capability::Brightness,
                Capability::ColorTemperature(temperature_range()),
            ],
        }],
    };
    let (device, mut receivers) = PhysicalDevice::new(description);
    device.set_availability(Availability::Available);
    (device.functions()[0].clone(), receivers.remove(0))
}

async fn next_light(receiver: &mut CommandReceiver) -> crate::device::LightRequest {
    match receiver.next_operation().await.unwrap() {
        DeviceRequest::Light(request) => request,
        DeviceRequest::Power(_) => panic!("expected lighting frame"),
    }
}

fn level_task() -> Task {
    Task::Level(level::Task::MoveToLevel {
        with_on_off: false,
        start: 1,
        target: 254,
        transition_ms: 100,
    })
}

#[test]
fn canceled_token_suppresses_frames_and_delayed_receipt_keeps_the_original_deadline() {
    let (function, mut receiver) = light_device();
    let clock = Rc::new(TestClock::default());
    let scheduler = Scheduler::new(function.clone(), Some(temperature_range()), clock.clone());
    function
        .observe_light(LightPatch {
            power: Some(true),
            brightness_percent: Some(1.0),
            ..Default::default()
        })
        .unwrap();
    block_on(async {
        // A receipt delayed past the deadline must not restart the trajectory:
        // the engine advances to the original final frame directly.
        let action = scheduler.begin(level_task(), None).unwrap();
        let first = next_light(&mut receiver).await;
        assert_eq!(first.target.brightness_percent, Some(1.0));
        assert!(!first.final_frame);
        first.mark_sent().unwrap();
        clock.set(250);
        first.accept().unwrap();
        scheduler.advance();
        let final_frame = next_light(&mut receiver).await;
        assert_eq!(final_frame.token, action.token());
        assert!(final_frame.final_frame);
        assert_eq!(final_frame.target.brightness_percent, Some(100.0));
        assert_eq!(scheduler.remaining_time(LightDimension::Brightness), 0);
        final_frame.mark_sent().unwrap();
        final_frame.accept().unwrap();
        action.ended().await.unwrap();
        assert!(poll_once(receiver.next_operation()).await.is_none());

        // A canceled token must suppress every remaining frame, even after
        // its original deadline has passed.
        let action = scheduler.begin(level_task(), None).unwrap();
        let first = next_light(&mut receiver).await;
        first.mark_sent().unwrap();
        first.accept().unwrap();
        action.accepted().await.unwrap();
        assert_eq!(scheduler.remaining_time(LightDimension::Brightness), 1);
        scheduler.stop(LightDimension::Brightness).unwrap();
        assert!(action.ended().await.is_err());
        clock.set(1000);
        scheduler.advance();
        assert!(poll_once(receiver.next_operation()).await.is_none());
        assert_eq!(scheduler.remaining_time(LightDimension::Brightness), 0);
        assert!(!function.light_action_current(action.token()));
    });
}
