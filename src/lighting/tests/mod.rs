//! Shared lighting executor lifecycle tests.
//!
//! These drive the real `Function` command queue with a fake clock. They cover
//! committed plans and normal single-light lifecycle, including takeover,
//! current progress, and cancellation without replay when scheduling resets.

mod group;

use super::{
    LightingExecutor, Plan,
    clock::Clock,
    controllers::{color, hs, level},
};
use crate::device::{
    Availability, Capability, Category, Command, CommandError, CommandReceiver, DeviceKey,
    DeviceRequest, Function, FunctionDescription, FunctionKey, LightColorTarget, LightDimension,
    NumericRange, PhysicalDevice, PhysicalDeviceDescription, Power, Rgb,
    brightness_from_matter_level,
};
use futures_lite::future::{block_on, poll_once};
use std::{
    cell::Cell,
    future::Future,
    pin::{Pin, pin},
    rc::Rc,
};

#[derive(Default)]
pub(crate) struct TestClock {
    millis: Cell<u64>,
    changed: event_listener::Event,
}
impl TestClock {
    pub(crate) fn set(&self, millis: u64) {
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

pub(crate) fn light_with_id(id: &str) -> (PhysicalDevice, Function, CommandReceiver) {
    let description = PhysicalDeviceDescription {
        key: DeviceKey::new("xiaomi", id),
        label: format!("Test Light {id}"),
        functions: vec![FunctionDescription {
            id: "main".into(),
            label: "Light".into(),
            category: Category::Light,
            capabilities: vec![
                Capability::OnOff,
                Capability::Brightness,
                Capability::ColorTemperature(NumericRange {
                    min: 2700,
                    max: 6500,
                    step: 1,
                }),
                Capability::ColorRgb,
            ],
        }],
    };
    let (device, mut receivers) = PhysicalDevice::new(description);
    device.set_availability(Availability::Available);
    let function = device.functions()[0].clone();
    (device, function, receivers.remove(0))
}

fn light() -> (PhysicalDevice, Function, CommandReceiver) {
    light_with_id("test-light")
}

fn level_plan(start: u8, target: u8, transition_ms: u64) -> Plan {
    Plan::Level(level::Task::MoveToLevel {
        with_on_off: false,
        start,
        target,
        transition_ms,
    })
}

pub(crate) async fn next_light(receiver: &mut CommandReceiver) -> crate::device::LightRequest {
    match receiver.next_operation().await.unwrap() {
        DeviceRequest::Light(request) => request,
        DeviceRequest::Power(_) => panic!("expected a lighting frame"),
    }
}

async fn next_power(receiver: &mut CommandReceiver) -> crate::device::CommandRequest {
    match receiver.next_operation().await.unwrap() {
        DeviceRequest::Power(request) => request,
        DeviceRequest::Light(_) => panic!("expected a power command"),
    }
}

pub(crate) async fn accept_light(receiver: &mut CommandReceiver) {
    let request = next_light(receiver).await;
    request.mark_sent().unwrap();
    request.accept().unwrap();
}

#[test]
fn committed_plan_fixes_start_target_duration_and_remaining_source() {
    let (device, function, mut receiver) = light();
    let key = function.function_key().clone();
    let clock = Rc::new(TestClock::default());
    let executor = LightingExecutor::new(clock.clone());
    block_on(async {
        let action = executor
            .begin(&function, level_plan(1, 254, 1000), None)
            .unwrap();
        assert_eq!(
            executor.remaining_ms(&key, LightDimension::Brightness),
            Some(1000)
        );
        let startup = next_light(&mut receiver).await;
        assert_eq!(
            startup.target.brightness_percent,
            Some(brightness_from_matter_level(1).unwrap())
        );
        assert!(!startup.final_frame);
        startup.mark_sent().unwrap();
        startup.accept().unwrap();
        action.accepted().await.unwrap();
        assert!(poll_once(action.ended()).await.is_none());

        clock.set(500);
        executor.advance();
        let mid = next_light(&mut receiver).await;
        assert!(!mid.final_frame);
        assert!(mid.target.brightness_percent.unwrap() > 49.0);
        assert!(mid.target.brightness_percent.unwrap() < 51.0);
        assert_eq!(
            executor.remaining_ms(&key, LightDimension::Brightness),
            Some(500)
        );
        mid.mark_sent().unwrap();
        mid.accept().unwrap();

        clock.set(1000);
        executor.advance();
        let last = next_light(&mut receiver).await;
        assert!(last.final_frame);
        assert_eq!(last.target.brightness_percent, Some(100.0));
        assert_eq!(
            executor.remaining_ms(&key, LightDimension::Brightness),
            Some(0)
        );
        last.mark_sent().unwrap();
        last.accept().unwrap();
        action.ended().await.unwrap();
        assert!(executor.ending(&key, LightDimension::Brightness));
        executor.advance();
        assert_eq!(
            executor.remaining_ms(&key, LightDimension::Brightness),
            None
        );
    });
    drop(device);
}

#[test]
fn remaining_api_spans_functions_and_member_takeover_drops_only_its_slot() {
    let (device_a, function_a, mut receiver_a) = light_with_id("a");
    let (device_b, function_b, mut receiver_b) = light_with_id("b");
    let key_a = function_a.function_key().clone();
    let key_b = function_b.function_key().clone();
    let clock = Rc::new(TestClock::default());
    let executor = LightingExecutor::new(clock.clone());
    block_on(async {
        executor
            .begin(&function_a, level_plan(1, 254, 1000), None)
            .unwrap();
        executor
            .begin(&function_b, level_plan(1, 254, 3000), None)
            .unwrap();
        accept_light(&mut receiver_a).await;
        accept_light(&mut receiver_b).await;
        let keys = [&key_a, &key_b];
        assert_eq!(
            executor.max_remaining_ms(keys, LightDimension::Brightness),
            Some(3000)
        );

        // A later single-control operation on one member only takes over that
        // member's dimension; the shared plan for the other member continues.
        function_b
            .take_over_light(LightDimension::Brightness)
            .unwrap();
        executor.advance();
        assert_eq!(
            executor.remaining_ms(&key_b, LightDimension::Brightness),
            None
        );
        assert_eq!(
            executor.remaining_ms(&key_a, LightDimension::Brightness),
            Some(1000)
        );
        assert_eq!(
            executor.max_remaining_ms(keys, LightDimension::Brightness),
            Some(1000)
        );

        executor
            .stop(&function_a, LightDimension::Brightness)
            .unwrap();
        assert_eq!(
            executor.max_remaining_ms(keys, LightDimension::Brightness),
            None
        );
    });
    drop(device_a);
    drop(device_b);
}

#[test]
fn stop_takeover_revoke_close_and_restart_end_actions_without_replay() {
    let (device, function, mut receiver) = light();
    let key = function.function_key().clone();
    let clock = Rc::new(TestClock::default());
    let executor = LightingExecutor::new(clock.clone());
    block_on(async {
        // Stop releases the actual device queue and the executor slot.
        let action = executor
            .begin(&function, level_plan(1, 254, 1000), None)
            .unwrap();
        accept_light(&mut receiver).await;
        executor
            .stop(&function, LightDimension::Brightness)
            .unwrap();
        assert_eq!(action.ended().await, Err(CommandError::Stopped));
        assert_eq!(
            executor.remaining_ms(&key, LightDimension::Brightness),
            None
        );
        assert!(executor.ending(&key, LightDimension::Brightness));

        // A real takeover cancels the device queue; the next tick drops the slot.
        let action = executor
            .begin(&function, level_plan(1, 254, 1000), None)
            .unwrap();
        accept_light(&mut receiver).await;
        function
            .take_over_light(LightDimension::Brightness)
            .unwrap();
        executor.advance();
        assert_eq!(action.ended().await, Err(CommandError::Superseded));
        assert_eq!(
            executor.remaining_ms(&key, LightDimension::Brightness),
            None
        );

        // Revoking all physical control paths ends the action without replay.
        let action = executor
            .begin(&function, level_plan(1, 254, 1000), None)
            .unwrap();
        accept_light(&mut receiver).await;
        device.set_availability(Availability::Unavailable("revoked".into()));
        executor.advance();
        assert!(action.ended().await.is_err());
        assert_eq!(
            executor.remaining_ms(&key, LightDimension::Brightness),
            None
        );

        // Physical close (adapter session gone) also ends it.
        device.set_availability(Availability::Available);
        let action = executor
            .begin(&function, level_plan(1, 254, 1000), None)
            .unwrap();
        let _startup = next_light(&mut receiver).await;
        drop(receiver);
        executor.advance();
        assert!(action.ended().await.is_err());
        assert_eq!(
            executor.remaining_ms(&key, LightDimension::Brightness),
            None
        );

        // A fresh process-local executor has no restored or replayed actions.
        let restarted = LightingExecutor::new(clock.clone());
        assert_eq!(
            restarted.remaining_ms(&key, LightDimension::Brightness),
            None
        );
        assert!(restarted.ending(&key, LightDimension::Brightness));
    });
    drop(device);
}

#[test]
fn reset_cancels_schedules_and_pending_power_without_replay() {
    let (device, function, mut receiver) = light();
    let clock = Rc::new(TestClock::default());
    let executor = LightingExecutor::new(clock.clone());
    block_on(async {
        let completed = executor.dispatch_power(&function, Command::On);
        next_power(&mut receiver).await.accept().unwrap();
        completed.accepted().await.unwrap();
        let action = executor
            .begin(&function, level_plan(1, 254, 1000), None)
            .unwrap();
        accept_light(&mut receiver).await;
        let pending = executor.dispatch_power(&function, Command::On);
        executor.reset();
        assert_eq!(action.ended().await, Err(CommandError::Cancelled));
        assert_eq!(pending.accepted().await, Err(CommandError::Cancelled));
        assert!(completed.accepted().await.is_ok());
        assert!(!executor.has_active(function.function_key()));
        clock.set(2000);
        executor.advance();
        assert!(poll_once(receiver.next_operation()).await.is_none());
    });
    drop(device);
}

#[test]
fn reset_leaves_a_newer_member_control_action_untouched() {
    let (device, function, mut receiver) = light();
    let clock = Rc::new(TestClock::default());
    let executor = LightingExecutor::new(clock);
    block_on(async {
        let old = executor
            .begin(&function, level_plan(1, 254, 1000), None)
            .unwrap();
        accept_light(&mut receiver).await;
        let current = function
            .begin_light_action(
                LightDimension::Brightness,
                crate::device::LightTarget {
                    brightness_percent: Some(70.0),
                    ..Default::default()
                },
                true,
            )
            .unwrap();
        executor.reset();
        assert_eq!(old.ended().await, Err(CommandError::Superseded));
        assert!(function.light_action_current(current.token()));
        accept_light(&mut receiver).await;
        current.ended().await.unwrap();
        assert!(!executor.has_active(function.function_key()));
    });
    drop(device);
}

#[test]
fn executor_owned_power_dispatch_survives_waiter_drop_before_and_after_dequeue() {
    for sent_before_drop in [false, true] {
        let (device, function, mut receiver) = light();
        let clock = Rc::new(TestClock::default());
        let executor = LightingExecutor::new(clock.clone());
        block_on(async {
            let waiter = executor.dispatch_power(&function, Command::On);
            let observer = waiter.clone();
            let mut run = pin!(executor.clone().run());
            assert!(poll_once(run.as_mut()).await.is_none());
            if sent_before_drop {
                let request = next_power(&mut receiver).await;
                request.mark_sent().unwrap();
                drop(waiter);
                request.accept().unwrap();
            } else {
                // The waiter is gone while the command is still unsent. The
                // executor owns the `Function::execute` future, so its
                // `Submission` guard cannot withdraw the queued command.
                drop(waiter);
                let request = next_power(&mut receiver).await;
                assert!(request.is_current());
                request.mark_sent().unwrap();
                request.accept().unwrap();
            }
            // No waiter is polled from here: the runtime runner alone drains
            // the executor-owned dispatch on its clock tick.
            clock.set(20);
            assert!(poll_once(run.as_mut()).await.is_none());
            let result = observer.result().expect("dispatch completed");
            assert!(result.is_ok());
            assert_eq!(function.snapshot().power.value(), Some(true));
            assert_eq!(function.snapshot().power, Power::On);
        });
        drop(device);
    }
}

#[test]
fn runner_progresses_committed_actions_with_the_shared_clock() {
    let (device, function, mut receiver) = light();
    let key = function.function_key().clone();
    let clock = Rc::new(TestClock::default());
    let executor = LightingExecutor::new(clock.clone());
    block_on(async {
        let mut run = pin!(executor.clone().run());
        assert!(poll_once(run.as_mut()).await.is_none());
        let action = executor
            .begin(&function, level_plan(1, 254, 1000), None)
            .unwrap();
        assert!(poll_once(run.as_mut()).await.is_none());
        let startup = next_light(&mut receiver).await;
        startup.mark_sent().unwrap();
        startup.accept().unwrap();
        clock.set(1000);
        assert!(poll_once(run.as_mut()).await.is_none());
        let last = next_light(&mut receiver).await;
        assert!(last.final_frame);
        last.mark_sent().unwrap();
        last.accept().unwrap();
        action.ended().await.unwrap();
        assert!(poll_once(run.as_mut()).await.is_none());
        assert_eq!(
            executor.remaining_ms(&key, LightDimension::Brightness),
            None
        );
    });
    drop(device);
}

#[test]
fn active_hs_metadata_is_readable_and_resumable_from_the_executor() {
    let (device, function, mut receiver) = light();
    let key = function.function_key().clone();
    let clock = Rc::new(TestClock::default());
    let executor = LightingExecutor::new(clock.clone());
    let plan = Plan::Color {
        task: color::Task::Hs(hs::HsTask {
            hue: hs::HueAxis::move_at(0.0, 20.0),
            saturation: hs::SaturationAxis::Hold(254.0),
        }),
        temperature: None,
    };
    block_on(async {
        let action = executor.begin(&function, plan, None).unwrap();
        let startup = next_light(&mut receiver).await;
        assert_eq!(
            startup.target.color,
            Some(LightColorTarget::Rgb(Rgb {
                red: 255,
                green: 0,
                blue: 0
            }))
        );
        startup.mark_sent().unwrap();
        startup.accept().unwrap();
        action.accepted().await.unwrap();
        clock.set(500);
        let resumed = executor.resumed_hs(&key).expect("active HS trajectory");
        let point = resumed.frame(0).0;
        assert!((point.hue - 10.0).abs() < 1e-9);
        assert_eq!(point.saturation, 254.0);
        executor.accept_color_metadata(&key);
        assert_eq!(executor.rgb_mode(&key), 0);
    });
    drop(device);
}

#[test]
fn multiple_functions_can_share_one_plan_shape_for_a_group_fanout() {
    let (device_a, function_a, mut receiver_a) = light_with_id("a");
    let (device_b, function_b, mut receiver_b) = light_with_id("b");
    let key_a = function_a.function_key().clone();
    let key_b = function_b.function_key().clone();
    let clock = Rc::new(TestClock::default());
    let executor = LightingExecutor::new(clock.clone());
    block_on(async {
        let plan = Plan::Color {
            task: color::Task::ColorTemperatureMoveTo {
                start: 200,
                target_mireds: 250,
                transition_ms: 1000,
            },
            temperature: Some(NumericRange {
                min: 2700,
                max: 6500,
                step: 1,
            }),
        };
        executor.begin(&function_a, plan, None).unwrap();
        executor.begin(&function_b, plan, None).unwrap();
        accept_light(&mut receiver_a).await;
        accept_light(&mut receiver_b).await;
        clock.set(500);
        executor.advance();
        let a = next_light(&mut receiver_a).await;
        let b = next_light(&mut receiver_b).await;
        assert_eq!(a.target.color, b.target.color);
        assert!(matches!(
            a.target.color,
            Some(LightColorTarget::Temperature(_))
        ));
        for key in [&key_a, &key_b] {
            assert_eq!(executor.remaining_ms(key, LightDimension::Color), Some(500));
        }
        assert_eq!(
            executor.max_remaining_ms([&key_a, &key_b], LightDimension::Color),
            Some(500)
        );
    });
    drop(device_a);
    drop(device_b);
}

#[test]
fn receipt_metadata_survives_a_finished_action_without_matter_reads() {
    let (device, function, mut receiver) = light();
    let key = function.function_key().clone();
    let clock = Rc::new(TestClock::default());
    let executor = LightingExecutor::new(clock.clone());
    let plan = Plan::Color {
        task: color::Task::Hs(hs::HsTask {
            hue: hs::HueAxis::move_to(0.0, 120.0, hs::HueDirection::Shortest, 1000),
            saturation: hs::SaturationAxis::move_to(254.0, 0.0, 1000),
        }),
        temperature: None,
    };
    block_on(async {
        let action = executor.begin(&function, plan, None).unwrap();
        let startup = next_light(&mut receiver).await;
        startup.mark_sent().unwrap();
        startup.accept().unwrap();
        action.accepted().await.unwrap();
        clock.set(1000);
        executor.advance();
        let last = next_light(&mut receiver).await;
        assert!(last.final_frame);
        last.mark_sent().unwrap();
        last.accept().unwrap();
        action.ended().await.unwrap();
        // The executor tick, not a Matter getter, commits the accepted mode
        // and the final achromatic hue before clearing the ended slot.
        executor.advance();
        assert_eq!(executor.remaining_ms(&key, LightDimension::Color), None);
        assert_eq!(executor.rgb_mode(&key), 0);
        let white = Rgb {
            red: 255,
            green: 255,
            blue: 255,
        };
        assert!((executor.hs_for_rgb(&key, white).hue - 120.0).abs() < 1e-9);
    });
    drop(device);
}

#[test]
fn invalid_plan_is_rejected_before_any_takeover() {
    let (device, function, mut receiver) = light();
    let clock = Rc::new(TestClock::default());
    let executor = LightingExecutor::new(clock.clone());
    block_on(async {
        let action = executor
            .begin(&function, level_plan(1, 254, 1000), None)
            .unwrap();
        let startup = next_light(&mut receiver).await;
        startup.mark_sent().unwrap();

        // The committed brightness action keeps ownership when a later plan
        // cannot produce a valid first frame.
        let invalid = Plan::Level(level::Task::MoveToLevel {
            with_on_off: false,
            start: 0,
            target: 254,
            transition_ms: 1000,
        });
        assert!(executor.begin(&function, invalid, None).is_err());
        assert!(function.light_action_current(action.token()));
        assert_eq!(
            executor.remaining_ms(function.function_key(), LightDimension::Brightness),
            Some(1000)
        );
        assert_eq!(clock.now(), 0);
    });
    drop(device);
}

impl LightingExecutor {
    pub(crate) fn remaining_ms(&self, key: &FunctionKey, dimension: LightDimension) -> Option<u64> {
        self.max_remaining_ms([key], dimension)
    }

    pub(crate) fn max_remaining_ms<'a>(
        &self,
        functions: impl IntoIterator<Item = &'a FunctionKey>,
        dimension: LightDimension,
    ) -> Option<u64> {
        self.transition(functions, dimension)
            .map(|transition| transition.remaining_ms(self.clock.now()))
    }
}
