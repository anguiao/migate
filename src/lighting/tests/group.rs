//! Group fanout coverage for the shared executor.
//!
//! These tests drive the real command queues with fake receivers. They fix the
//! behavior the group entry must provide: per-member actual starts, one origin
//! per trigger, partial failure, early success without cancelling the rest,
//! remaining-time aggregation over current members.

use super::{TestClock, accept_light, light_with_id, next_light};
use crate::device::{
    Availability, Capability, Category, CommandError, CommandReceiver, DeviceKey, Function,
    FunctionDescription, Group, Light, LightColorTarget, LightDimension, LightMode, LightPatch,
    NumericRange, PhysicalDevice, PhysicalDeviceDescription,
};
use crate::lighting::{
    LightingExecutor,
    clock::Clock,
    controllers::MoveDirection,
    group::Fanout,
    planning::{ColorPlan, LevelPlan},
};
use futures_lite::future::{block_on, poll_once};
use std::{cell::Cell, future::Future, pin::Pin, rc::Rc};

fn light_with_caps(
    id: &str,
    capabilities: Vec<Capability>,
) -> (PhysicalDevice, Function, CommandReceiver) {
    let description = PhysicalDeviceDescription {
        key: DeviceKey::new("xiaomi", id),
        label: id.into(),
        functions: vec![FunctionDescription {
            id: "main".into(),
            label: "Light".into(),
            category: Category::Light,
            capabilities,
        }],
    };
    let (device, mut receivers) = PhysicalDevice::new(description);
    device.set_availability(Availability::Available);
    let function = device.functions()[0].clone();
    (device, function, receivers.remove(0))
}

fn group_of(key: &str, functions: &[&Function]) -> Group {
    Group::new(
        DeviceKey::new("xiaomi", key),
        key,
        functions
            .iter()
            .map(|function| (*function).clone())
            .collect(),
    )
    .unwrap()
}

fn observe(function: &Function, patch: LightPatch) {
    function.observe_light(patch).unwrap();
}

fn moving_to(target: u8, transition_ms: u64) -> Fanout {
    Fanout::Level(LevelPlan::MoveTo {
        target,
        transition_ms,
        with_on_off: false,
    })
}

#[derive(Default)]
struct TickClock {
    millis: Cell<u64>,
}
impl TickClock {
    fn set(&self, millis: u64) {
        self.millis.set(millis);
    }
}
impl Clock for TickClock {
    fn now(&self) -> u64 {
        let value = self.millis.get();
        self.millis.set(value + 1);
        value
    }
    fn wait_until(&self, deadline: u64) -> Pin<Box<dyn Future<Output = ()> + '_>> {
        Box::pin(async move {
            while self.millis.get() < deadline {
                futures_lite::future::yield_now().await;
            }
        })
    }
}

#[test]
fn group_fanout_plans_each_member_from_its_actual_start() {
    let (device_a, a, mut receiver_a) = light_with_id("a");
    let (device_b, b, mut receiver_b) = light_with_id("b");
    observe(
        &a,
        LightPatch {
            power: Some(true),
            brightness_percent: Some(10.0),
            ..Default::default()
        },
    );
    observe(
        &b,
        LightPatch {
            power: Some(true),
            brightness_percent: Some(90.0),
            ..Default::default()
        },
    );
    let group = group_of("group", &[&a, &b]);
    let clock = Rc::new(TestClock::default());
    let executor = LightingExecutor::new(clock.clone());
    block_on(async {
        let dispatch = executor.fanout(&group, moving_to(128, 1000), None);
        let startup_a = next_light(&mut receiver_a).await;
        let startup_b = next_light(&mut receiver_b).await;
        assert!((startup_a.target.brightness_percent.unwrap() - 10.0).abs() < 0.01);
        assert!((startup_b.target.brightness_percent.unwrap() - 89.83).abs() < 0.01);
        assert_ne!(startup_a.token, startup_b.token);
        // One trigger fixes one origin for every member.
        assert_eq!(
            executor.remaining_ms(a.function_key(), LightDimension::Brightness),
            Some(1000)
        );
        assert_eq!(
            executor.remaining_ms(b.function_key(), LightDimension::Brightness),
            Some(1000)
        );
        startup_a.mark_sent().unwrap();
        startup_a.accept().unwrap();
        startup_b.mark_sent().unwrap();
        startup_b.accept().unwrap();
        dispatch.accepted().await.unwrap();

        clock.set(500);
        executor.advance();
        let mid_a = next_light(&mut receiver_a).await;
        let mid_b = next_light(&mut receiver_b).await;
        assert!(mid_a.target.brightness_percent.unwrap() > 10.0);
        assert!(mid_b.target.brightness_percent.unwrap() < 89.83);
        mid_a.mark_sent().unwrap();
        mid_a.accept().unwrap();
        mid_b.mark_sent().unwrap();
        mid_b.accept().unwrap();
    });
    drop(device_a);
    drop(device_b);
}

#[test]
fn group_move_continues_each_member_from_its_actual_level() {
    let (device_a, a, mut receiver_a) = light_with_id("a");
    let (device_b, b, mut receiver_b) = light_with_id("b");
    observe(
        &a,
        LightPatch {
            power: Some(true),
            brightness_percent: Some(20.0),
            ..Default::default()
        },
    );
    observe(
        &b,
        LightPatch {
            power: Some(true),
            brightness_percent: Some(80.0),
            ..Default::default()
        },
    );
    let group = group_of("group", &[&a, &b]);
    let clock = Rc::new(TestClock::default());
    let executor = LightingExecutor::new(clock.clone());
    block_on(async {
        executor.fanout(
            &group,
            Fanout::Level(LevelPlan::Move {
                direction: MoveDirection::Up,
                rate: 20,
                with_on_off: false,
            }),
            None,
        );
        let startup_a = next_light(&mut receiver_a).await;
        let startup_b = next_light(&mut receiver_b).await;
        assert!((startup_a.target.brightness_percent.unwrap() - 20.0).abs() < 0.2);
        assert!(startup_b.target.brightness_percent.unwrap() > 79.0);
        startup_a.mark_sent().unwrap();
        startup_a.accept().unwrap();
        startup_b.mark_sent().unwrap();
        startup_b.accept().unwrap();
        clock.set(2000);
        executor.advance();
        let mid_a = next_light(&mut receiver_a).await;
        let mid_b = next_light(&mut receiver_b).await;
        assert!(mid_a.target.brightness_percent.unwrap() > 20.0);
        assert!(mid_b.target.brightness_percent.unwrap() > 80.0);
    });
    drop(device_a);
    drop(device_b);
}

#[test]
fn group_member_without_a_start_fails_alone_and_the_rest_execute() {
    let (device_a, a, mut receiver_a) = light_with_id("a");
    let (device_b, b, mut receiver_b) = light_with_id("b");
    observe(
        &a,
        LightPatch {
            power: Some(true),
            brightness_percent: Some(20.0),
            ..Default::default()
        },
    );
    let group = group_of("group", &[&a, &b]);
    let clock = Rc::new(TestClock::default());
    let executor = LightingExecutor::new(clock.clone());
    block_on(async {
        let dispatch = executor.fanout(&group, moving_to(128, 1000), None);
        let startup_a = next_light(&mut receiver_a).await;
        // The member without a start never receives a frame.
        assert!(poll_once(receiver_b.next_operation()).await.is_none());
        startup_a.mark_sent().unwrap();
        startup_a.accept().unwrap();
        dispatch.accepted().await.unwrap();
    });
    drop(device_a);
    drop(device_b);
}

#[test]
fn group_early_success_leaves_other_members_executing() {
    let (device_a, a, mut receiver_a) = light_with_id("a");
    let (device_b, b, mut receiver_b) = light_with_id("b");
    for (function, brightness) in [(&a, 20.0), (&b, 80.0)] {
        observe(
            function,
            LightPatch {
                power: Some(true),
                brightness_percent: Some(brightness),
                ..Default::default()
            },
        );
    }
    let group = group_of("group", &[&a, &b]);
    let clock = Rc::new(TestClock::default());
    let executor = LightingExecutor::new(clock.clone());
    block_on(async {
        let dispatch = executor.fanout(&group, moving_to(128, 2000), None);
        let startup_a = next_light(&mut receiver_a).await;
        startup_a.mark_sent().unwrap();
        startup_a.accept().unwrap();
        // Early success ends only the waiter; the other member is untouched.
        dispatch.accepted().await.unwrap();
        let startup_b = next_light(&mut receiver_b).await;
        assert!(startup_b.is_current());
        startup_b.mark_sent().unwrap();
        startup_b.accept().unwrap();
        clock.set(2000);
        executor.advance();
        for receiver in [&mut receiver_a, &mut receiver_b] {
            let final_frame = next_light(receiver).await;
            assert!(final_frame.final_frame);
            assert_eq!(
                final_frame.target.brightness_percent,
                Some(crate::device::brightness_from_matter_level(128).unwrap())
            );
            final_frame.mark_sent().unwrap();
            final_frame.accept().unwrap();
        }
    });
    drop(device_a);
    drop(device_b);
}

#[test]
fn group_waiter_drop_before_any_accept_still_dispatches() {
    let (device_a, a, mut receiver_a) = light_with_id("a");
    let (device_b, b, mut receiver_b) = light_with_id("b");
    for (function, brightness) in [(&a, 30.0), (&b, 70.0)] {
        observe(
            function,
            LightPatch {
                power: Some(true),
                brightness_percent: Some(brightness),
                ..Default::default()
            },
        );
    }
    let group = group_of("group", &[&a, &b]);
    let clock = Rc::new(TestClock::default());
    let executor = LightingExecutor::new(clock.clone());
    block_on(async {
        let dispatch = executor.fanout(&group, moving_to(200, 1000), None);
        drop(dispatch);
        let startup_a = next_light(&mut receiver_a).await;
        let startup_b = next_light(&mut receiver_b).await;
        startup_a.mark_sent().unwrap();
        startup_a.accept().unwrap();
        startup_b.mark_sent().unwrap();
        startup_b.accept().unwrap();
        executor.advance();
        assert!(executor.has_active(a.function_key()));
        assert!(executor.has_active(b.function_key()));
    });
    drop(device_a);
    drop(device_b);
}

#[test]
fn group_with_every_member_failing_returns_the_overall_failure() {
    let (device_a, a, _receiver_a) = light_with_id("a");
    let (device_b, b, _receiver_b) = light_with_id("b");
    let group = group_of("group", &[&a, &b]);
    let clock = Rc::new(TestClock::default());
    let executor = LightingExecutor::new(clock.clone());
    block_on(async {
        let dispatch = executor.fanout(&group, moving_to(128, 1000), None);
        assert_eq!(dispatch.accepted().await, Err(CommandError::UnknownState));
    });
    drop(device_a);
    drop(device_b);
}

#[test]
fn group_temperature_members_quantize_the_common_target_on_their_own_grid() {
    let (device_a, a, mut receiver_a) = light_with_caps(
        "a",
        vec![
            Capability::OnOff,
            Capability::ColorTemperature(NumericRange {
                min: 2700,
                max: 6500,
                step: 100,
            }),
        ],
    );
    let (device_b, b, mut receiver_b) = light_with_caps(
        "b",
        vec![
            Capability::OnOff,
            Capability::ColorTemperature(NumericRange {
                min: 3000,
                max: 6000,
                step: 50,
            }),
        ],
    );
    for (function, kelvin) in [(&a, 3100), (&b, 5900)] {
        observe(
            function,
            LightPatch {
                power: Some(true),
                temperature_kelvin: Some(kelvin),
                ..Default::default()
            },
        );
    }
    let group = group_of("group", &[&a, &b]);
    let clock = Rc::new(TestClock::default());
    let executor = LightingExecutor::new(clock.clone());
    block_on(async {
        executor.fanout(
            &group,
            Fanout::Color(ColorPlan::TemperatureMoveTo {
                target_mireds: 250,
                transition_ms: 1000,
            }),
            None,
        );
        let startup_a = next_light(&mut receiver_a).await;
        let startup_b = next_light(&mut receiver_b).await;
        for request in [&startup_a, &startup_b] {
            assert!(request.is_current());
        }
        startup_a.mark_sent().unwrap();
        startup_a.accept().unwrap();
        startup_b.mark_sent().unwrap();
        startup_b.accept().unwrap();
        clock.set(500);
        executor.advance();
        let mid_a = next_light(&mut receiver_a).await;
        let mid_b = next_light(&mut receiver_b).await;
        let kelvin_a = match mid_a.target.color.unwrap() {
            crate::device::LightColorTarget::Temperature(value) => value,
            other => panic!("expected temperature, got {other:?}"),
        };
        let kelvin_b = match mid_b.target.color.unwrap() {
            crate::device::LightColorTarget::Temperature(value) => value,
            other => panic!("expected temperature, got {other:?}"),
        };
        // Each member stays on its own quantization grid and inside the
        // common interval.
        assert_eq!(kelvin_a % 100, 0);
        assert_eq!(kelvin_b % 50, 0);
        assert!((3000..=6000).contains(&kelvin_a));
        assert!((3000..=6000).contains(&kelvin_b));
        assert!(kelvin_a > 3100);
        assert!(kelvin_b < 5900);
        mid_a.mark_sent().unwrap();
        mid_a.accept().unwrap();
        mid_b.mark_sent().unwrap();
        mid_b.accept().unwrap();
        clock.set(1000);
        executor.advance();
        let final_a = next_light(&mut receiver_a).await;
        let final_b = next_light(&mut receiver_b).await;
        assert!(final_a.final_frame);
        assert!(final_b.final_frame);
        assert!(matches!(
            final_a.target.color,
            Some(crate::device::LightColorTarget::Temperature(4000))
        ));
        assert!(matches!(
            final_b.target.color,
            Some(crate::device::LightColorTarget::Temperature(4000))
        ));
    });
    drop(device_a);
    drop(device_b);
}

#[test]
fn group_temperature_bounds_are_the_common_interval_not_the_member_range() {
    let (device_a, a, mut receiver_a) = light_with_caps(
        "a",
        vec![
            Capability::OnOff,
            Capability::ColorTemperature(NumericRange {
                min: 2700,
                max: 6500,
                step: 100,
            }),
        ],
    );
    let (device_b, b, mut receiver_b) = light_with_caps(
        "b",
        vec![
            Capability::OnOff,
            Capability::ColorTemperature(NumericRange {
                min: 3000,
                max: 6000,
                step: 100,
            }),
        ],
    );
    for function in [&a, &b] {
        observe(
            function,
            LightPatch {
                power: Some(true),
                temperature_kelvin: Some(3000),
                ..Default::default()
            },
        );
    }
    let group = group_of("group", &[&a, &b]);
    let clock = Rc::new(TestClock::default());
    let executor = LightingExecutor::new(clock.clone());
    block_on(async {
        // Cooling below the common 6000K bound must stop at that bound even
        // though member A alone could reach 6500K.
        executor.fanout(
            &group,
            Fanout::Color(ColorPlan::TemperatureMove {
                direction: MoveDirection::Down,
                rate: 166,
                min_mireds: 167,
                max_mireds: 333,
            }),
            None,
        );
        accept_light(&mut receiver_a).await;
        accept_light(&mut receiver_b).await;
        clock.set(1000);
        executor.advance();
        let final_a = next_light(&mut receiver_a).await;
        let final_b = next_light(&mut receiver_b).await;
        assert!(final_a.final_frame);
        assert!(final_b.final_frame);
        assert!(matches!(
            final_a.target.color,
            Some(crate::device::LightColorTarget::Temperature(6000))
        ));
        assert!(matches!(
            final_b.target.color,
            Some(crate::device::LightColorTarget::Temperature(6000))
        ));
    });
    drop(device_a);
    drop(device_b);
}

#[test]
fn group_remaining_time_tracks_member_takeover() {
    let (device_a, a, mut receiver_a) = light_with_id("a");
    let (device_b, b, mut receiver_b) = light_with_id("b");
    for (function, brightness) in [(&a, 20.0), (&b, 80.0)] {
        observe(
            function,
            LightPatch {
                power: Some(true),
                brightness_percent: Some(brightness),
                ..Default::default()
            },
        );
    }
    let group = group_of("group", &[&a, &b]);
    let clock = Rc::new(TestClock::default());
    let executor = LightingExecutor::new(clock.clone());
    block_on(async {
        executor.fanout(&group, moving_to(128, 1000), None);
        accept_light(&mut receiver_a).await;
        accept_light(&mut receiver_b).await;
        let keys = group.keys();
        assert_eq!(
            executor.max_remaining_ms(keys.iter(), LightDimension::Brightness),
            Some(1000)
        );

        // A single takeover on the member only replaces that member's plan.
        let action = executor
            .begin(
                &b,
                crate::lighting::Plan::Level(
                    crate::lighting::controllers::level::Task::MoveToLevel {
                        with_on_off: false,
                        start: 1,
                        target: 254,
                        transition_ms: 3000,
                    },
                ),
                None,
            )
            .unwrap();
        let startup_b = next_light(&mut receiver_b).await;
        startup_b.mark_sent().unwrap();
        startup_b.accept().unwrap();
        action.accepted().await.unwrap();
        assert_eq!(
            executor.max_remaining_ms(keys.iter(), LightDimension::Brightness),
            Some(3000)
        );

        assert_eq!(
            executor.remaining_ms(a.function_key(), LightDimension::Brightness),
            Some(1000)
        );
    });
    drop(device_a);
    drop(device_b);
}

#[test]
fn shared_groups_take_over_one_control_chain_and_stop_only_current_member_dimensions() {
    let (device_a, a, mut receiver_a) = light_with_id("a");
    let (device_b, b, mut receiver_b) = light_with_id("b");
    let (device_c, c, mut receiver_c) = light_with_id("c");
    for (function, brightness) in [(&a, 20.0), (&b, 80.0), (&c, 50.0)] {
        observe(
            function,
            LightPatch {
                power: Some(true),
                brightness_percent: Some(brightness),
                temperature_kelvin: Some(4000),
                mode: Some(LightMode::Temperature),
                ..Default::default()
            },
        );
    }
    let group_a = group_of("group-a", &[&a, &b]);
    let group_b = group_of("group-b", &[&b, &c]);
    let executor = LightingExecutor::new(Rc::new(TestClock::default()));
    block_on(async {
        executor.fanout(&group_a, moving_to(128, 1000), None);
        let first_a = next_light(&mut receiver_a).await;
        let first_b = next_light(&mut receiver_b).await;
        let token_a = first_a.token;
        let token_b = first_b.token;
        first_a.mark_sent().unwrap();
        first_a.accept().unwrap();
        first_b.mark_sent().unwrap();
        first_b.accept().unwrap();

        executor.fanout(
            &group_b,
            Fanout::Color(ColorPlan::TemperatureMoveTo {
                target_mireds: 300,
                transition_ms: 3000,
            }),
            None,
        );
        accept_light(&mut receiver_b).await;
        accept_light(&mut receiver_c).await;
        executor.fanout(&group_b, moving_to(200, 3000), None);
        accept_light(&mut receiver_b).await;
        accept_light(&mut receiver_c).await;

        // Both groups address the same physical brightness slot for B.
        assert!(!b.light_action_current(token_b));
        assert!(a.light_action_current(token_a));
        assert_eq!(
            executor.max_remaining_ms(group_a.keys().iter(), LightDimension::Brightness),
            Some(3000)
        );

        // Stopping A also stops B's newer group-B brightness action, while
        // C's brightness and both color actions keep running.
        executor
            .fanout(&group_a, Fanout::Level(LevelPlan::Stop), None)
            .accepted()
            .await
            .unwrap();
        for function in [&a, &b] {
            assert_eq!(
                executor.remaining_ms(function.function_key(), LightDimension::Brightness),
                None
            );
        }
        assert_eq!(
            executor.remaining_ms(c.function_key(), LightDimension::Brightness),
            Some(3000)
        );
        for function in [&b, &c] {
            assert_eq!(
                executor.remaining_ms(function.function_key(), LightDimension::Color),
                Some(3000)
            );
        }
        assert_eq!(
            executor.max_remaining_ms(group_a.keys().iter(), LightDimension::Brightness),
            None
        );
    });
    drop(device_a);
    drop(device_b);
    drop(device_c);
}

#[test]
fn group_power_fanout_sends_the_same_definite_target_to_every_member() {
    let (device_a, a, mut receiver_a) = light_with_id("a");
    let (device_b, b, mut receiver_b) = light_with_id("b");
    for (function, power) in [(&a, true), (&b, false)] {
        observe(
            function,
            LightPatch {
                power: Some(power),
                ..Default::default()
            },
        );
    }
    let group = group_of("group", &[&a, &b]);
    let clock = Rc::new(TestClock::default());
    let executor = LightingExecutor::new(clock.clone());
    block_on(async {
        let dispatch = executor.fanout(&group, Fanout::Power(false), None);
        let off_a = match receiver_a.next_operation().await.unwrap() {
            crate::device::DeviceRequest::Power(request) => request,
            crate::device::DeviceRequest::Light(_) => panic!("expected power"),
        };
        let off_b = match receiver_b.next_operation().await.unwrap() {
            crate::device::DeviceRequest::Power(request) => request,
            crate::device::DeviceRequest::Light(_) => panic!("expected power"),
        };
        // The entry resolved one definite target; neither member inverts a
        // mixed-power group state on its own.
        assert!(!off_a.target);
        assert!(!off_b.target);
        off_a.accept().unwrap();
        off_b.accept().unwrap();
        dispatch.accepted().await.unwrap();
        assert_eq!(b.snapshot().power.value(), Some(false));
    });
    drop(device_a);
    drop(device_b);
}

#[test]
fn group_fanout_samples_one_origin_before_committing_each_member() {
    let (device_a, a, _receiver_a) = light_with_id("a");
    let (device_b, b, _receiver_b) = light_with_id("b");
    for (function, brightness) in [(&a, 20.0), (&b, 80.0)] {
        observe(
            function,
            LightPatch {
                power: Some(true),
                brightness_percent: Some(brightness),
                ..Default::default()
            },
        );
    }
    let group = group_of("group", &[&a, &b]);
    let clock = Rc::new(TickClock::default());
    clock.set(100);
    let executor = LightingExecutor::new(clock.clone());
    executor.fanout(&group, moving_to(128, 1000), None);
    // A clock that advances on every read exposes per-member resampling.
    let actions = executor.actions.borrow();
    for function in [&a, &b] {
        let active = actions[function.function_key()].slots[0].as_ref().unwrap();
        assert_eq!(active.start, 100);
        assert_eq!(active.plan.duration(), 1000);
    }
    drop(device_a);
    drop(device_b);
}

fn ct_range(min: u32, max: u32, step: u32) -> Vec<Capability> {
    vec![
        Capability::OnOff,
        Capability::ColorTemperature(NumericRange { min, max, step }),
    ]
}

#[test]
fn group_temperature_step_target_stays_inside_the_common_bounds() {
    let (device_a, a, mut receiver_a) = light_with_caps("a", ct_range(2700, 6500, 1));
    let (device_b, b, mut receiver_b) = light_with_caps("b", ct_range(3000, 6000, 1));
    for (function, kelvin) in [(&a, 2700), (&b, 6000)] {
        observe(
            function,
            LightPatch {
                power: Some(true),
                temperature_kelvin: Some(kelvin),
                mode: Some(LightMode::Temperature),
                ..Default::default()
            },
        );
    }
    let group = group_of("group", &[&a, &b]);
    // Member A reads 2700K below the common interval; the aggregate keeps the
    // reading unknown instead of clamping it into the group range.
    assert_eq!(
        Light::Group(group.clone())
            .state()
            .lighting
            .unwrap()
            .temperature_kelvin,
        None
    );
    let (lo, hi) = NumericRange {
        min: 3000,
        max: 6000,
        step: 1,
    }
    .mired_bounds()
    .unwrap();
    let clock = Rc::new(TestClock::default());
    let executor = LightingExecutor::new(clock.clone());
    block_on(async {
        executor.fanout(
            &group,
            Fanout::Color(ColorPlan::TemperatureStep {
                direction: MoveDirection::Up,
                size: 1,
                transition_ms: 1000,
                min_mireds: lo,
                max_mireds: hi,
            }),
            None,
        );
        let start_a = next_light(&mut receiver_a).await;
        let start_b = next_light(&mut receiver_b).await;
        // Startup frames carry each actual start, including the out-of-range
        // member; only the target is bounded.
        assert!(matches!(
            start_a.target.color,
            Some(LightColorTarget::Temperature(2703))
        ));
        assert!(matches!(
            start_b.target.color,
            Some(LightColorTarget::Temperature(5988))
        ));
        start_a.mark_sent().unwrap();
        start_a.accept().unwrap();
        start_b.mark_sent().unwrap();
        start_b.accept().unwrap();
        clock.set(37_000);
        executor.advance();
        let final_a = next_light(&mut receiver_a).await;
        let final_b = next_light(&mut receiver_b).await;
        assert!(matches!(
            final_a.target.color,
            Some(LightColorTarget::Temperature(3003))
        ));
        assert!(matches!(
            final_b.target.color,
            Some(LightColorTarget::Temperature(5952))
        ));
    });
    drop(device_a);
    drop(device_b);
}

#[test]
fn group_temperature_step_target_stays_inside_narrower_request_bounds() {
    let (device_a, a, mut receiver_a) = light_with_caps("a", ct_range(2700, 6500, 1));
    let (device_b, b, mut receiver_b) = light_with_caps("b", ct_range(3000, 6000, 1));
    for (function, kelvin) in [(&a, 2700), (&b, 6000)] {
        observe(
            function,
            LightPatch {
                power: Some(true),
                temperature_kelvin: Some(kelvin),
                mode: Some(LightMode::Temperature),
                ..Default::default()
            },
        );
    }
    let group = group_of("group", &[&a, &b]);
    let clock = Rc::new(TestClock::default());
    let executor = LightingExecutor::new(clock.clone());
    block_on(async {
        executor.fanout(
            &group,
            Fanout::Color(ColorPlan::TemperatureStep {
                direction: MoveDirection::Down,
                size: 1,
                transition_ms: 1000,
                min_mireds: 200,
                max_mireds: 300,
            }),
            None,
        );
        let start_a = next_light(&mut receiver_a).await;
        let start_b = next_light(&mut receiver_b).await;
        assert!(matches!(
            start_a.target.color,
            Some(LightColorTarget::Temperature(2703))
        ));
        assert!(matches!(
            start_b.target.color,
            Some(LightColorTarget::Temperature(5988))
        ));
        start_a.mark_sent().unwrap();
        start_a.accept().unwrap();
        start_b.mark_sent().unwrap();
        start_b.accept().unwrap();
        clock.set(71_000);
        executor.advance();
        let final_a = next_light(&mut receiver_a).await;
        let final_b = next_light(&mut receiver_b).await;
        // A clamps from 370 to the request max, B from 167 to the request min.
        assert!(matches!(
            final_a.target.color,
            Some(LightColorTarget::Temperature(3333))
        ));
        assert!(matches!(
            final_b.target.color,
            Some(LightColorTarget::Temperature(5000))
        ));
    });
    drop(device_a);
    drop(device_b);
}
