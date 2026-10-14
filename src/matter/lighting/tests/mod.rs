use super::controllers::{
    RemainingTime,
    color::{self, Task as ColorTask},
    level::LevelControlState,
};
use crate::{
    device::Rgb,
    lighting::{Transition, controllers::level::Task},
};
use rs_matter::{
    dm::clusters::decl::level_control::{MoveModeEnum, StepModeEnum},
    error::ErrorCode,
};

#[test]
fn srgb_xy_uses_65536_inverse_gamma_and_deterministic_black() {
    assert_eq!(
        color::rgb_to_xy(Rgb {
            red: 0,
            green: 255,
            blue: 0
        }),
        Some((19661, 39322))
    );
    assert_eq!(
        color::rgb_to_xy(Rgb {
            red: 0,
            green: 0,
            blue: 255
        }),
        Some((9830, 3932))
    );
    for y in 1..=6 {
        let rgb = color::xy_to_rgb(30000, y);
        assert_ne!(
            rgb,
            Rgb {
                red: 0,
                green: 0,
                blue: 0
            }
        );
        assert_eq!(color::xy_to_rgb(30000, y), rgb);
    }
    assert_eq!(
        color::rgb_to_xy(Rgb {
            red: 20,
            green: 20,
            blue: 20
        }),
        color::rgb_to_xy(Rgb {
            red: 255,
            green: 255,
            blue: 255
        })
    );
    assert_eq!(
        color::rgb_to_xy(Rgb {
            red: 255,
            green: 0,
            blue: 0
        }),
        Some((41943, 21627))
    );
    assert_eq!(
        color::rgb_to_xy(Rgb {
            red: 255,
            green: 255,
            blue: 255
        }),
        Some((20493, 21561))
    );
    assert_eq!(
        color::rgb_to_xy(Rgb {
            red: 255,
            green: 128,
            blue: 64
        }),
        Some((33935, 25220))
    );
    assert_eq!(
        color::rgb_to_xy(Rgb {
            red: 0,
            green: 0,
            blue: 0
        }),
        None
    );
    assert_eq!(
        color::xy_to_rgb(30000, 0),
        Rgb {
            red: 0,
            green: 0,
            blue: 0
        }
    );
    let color = color::xy_to_rgb(33935, 25220);
    assert!((color.red as i16 - 255).abs() <= 1);
    assert!((color.green as i16 - 128).abs() <= 1);
    assert!((color.blue as i16 - 64).abs() <= 1);
}

#[test]
fn short_level_transition_uses_deadline_and_move_does_not_follow_slow_ack() {
    let task = Task::MoveToLevel {
        with_on_off: true,
        start: 1,
        target: 254,
        transition_ms: 100,
    };
    assert_eq!(task.frame(0), (1, false, Some(true)));
    assert_eq!(task.frame(20).0, 51);
    assert!(!task.frame(99).1);
    assert_eq!(task.frame(100), (254, true, Some(true)));
    let task = Task::Move {
        with_on_off: false,
        start: 100,
        direction: MoveDirection::Up,
        rate: 10,
    };
    assert_eq!(task.frame(1000).0, 110);
    assert_eq!(task.frame(2000).0, 120);
}

#[test]
fn wire_zero_clamps_minimum_and_invalid_or_ignored_requests_do_not_create_tasks() {
    let state = LevelControlState {
        current_level: Some(100),
        ..Default::default()
    };
    for wire in [0, 1] {
        assert_eq!(
            state
                .move_to_level(wire, true, Some(0), 0, 0, Some(true))
                .unwrap()
                .unwrap()
                .resolve(state.current_level)
                .unwrap()
                .frame(0),
            (1, true, Some(false))
        );
        assert_eq!(
            state
                .move_to_level(wire, false, Some(0), 0, 0, Some(true))
                .unwrap()
                .unwrap()
                .resolve(state.current_level)
                .unwrap()
                .frame(0),
            (1, true, None)
        );
    }
    assert_eq!(
        state
            .move_to_level(255, true, Some(0), 0, 0, Some(true))
            .unwrap_err()
            .code(),
        ErrorCode::InvalidCommand
    );
    assert!(
        state
            .move_to_level(50, false, Some(0), 0, 0, Some(false))
            .unwrap()
            .is_none()
    );
    let task = state
        .step(
            false,
            StepModeEnum::Up,
            255,
            Some(u16::MAX),
            0,
            0,
            Some(true),
        )
        .unwrap()
        .unwrap()
        .resolve(state.current_level)
        .unwrap();
    assert_eq!(task.duration(), u16::MAX as u64 * 154 / 255 * 100);
}

#[test]
fn long_color_move_remaining_time_saturates_without_wrapping() {
    let task = ColorTask::XyMove {
        start_x: 0,
        start_y: 0,
        rate_x: 1,
        rate_y: 0,
    };
    assert_eq!(task.duration(), 65279000);
    assert_eq!(task.frame(65279000), (color::Target::Xy(65279, 0), true));
    let mut remaining = RemainingTime::default();
    let transition = Some(Transition {
        start: 0,
        duration: task.duration(),
    });
    assert!(remaining.write(transition, 0, 0xfffe));
    assert_eq!(remaining.value, 0xfffe);
    assert!(!remaining.write(transition, task.duration() - 1000, 0xfffe));
    assert!(remaining.write(None, task.duration(), 0xfffe));
}

use super::{
    Lighting,
    controllers::clock::Clock,
    handlers::{ColorHandler, LevelHandler},
};
use crate::{
    device::{
        Availability, CommandError, DeviceRequest, Function, Light, LightColorTarget,
        LightDimension, LightMode, LightPatch, LightRequest,
    },
    lighting::{LightingExecutor, Plan as LightingTask, controllers::MoveDirection},
    matter::{
        model,
        storage::StoreAdapter,
        tests::{handlers::Context, support},
    },
    storage::Store,
};
use futures_lite::future::{block_on, poll_once};
use rs_matter::{
    MATTER_PORT, Matter,
    crypto::default_crypto,
    dm::{
        Dataver,
        clusters::decl::{color_control as c, level_control as l},
        devices::test::{DAC_PRIVKEY, TEST_DEV_ATT, TEST_DEV_COMM},
        networks::eth::EthNetwork,
    },
    im::{EthInteractionModelState, InteractionModel},
    tlv::TLVElement,
    transport::exchange::MatterBuffers,
};
use std::{
    cell::Cell,
    future::Future,
    pin::{Pin, pin},
    rc::Rc,
};

fn color_plan(light: &Lighting, task: ColorTask) -> LightingTask {
    LightingTask::Color {
        task,
        temperature: light.mapping.temperature.map(|published| published.range),
    }
}

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
macro_rules! setup {
    ($rgb:expr,$function:ident,$receiver:ident,$light:ident,$clock:ident,$ctx:ident) => {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let identity = store.load_identity().unwrap();

        let (device, mut $receiver) = support::light($rgb);
        let $function = device.functions()[0].clone();
        device.set_availability(Availability::Available);

        let registry = support::test_registry(&store, &device);
        let handler = model::handler(
            &registry,
            &store,
            StoreAdapter::new(store.matter()),
            support::test_executor(),
            rand::rng(),
        )
        .unwrap();

        let info = model::basic_info(&identity);
        let matter = Matter::new(&info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
        let buffers: MatterBuffers = MatterBuffers::new();

        let state: EthInteractionModelState =
            EthInteractionModelState::new(EthNetwork::new_default());
        let crypto = default_crypto(rand::rng(), DAC_PRIVKEY);
        let kv = matter.kv(StoreAdapter::new(store.matter()));

        let im = InteractionModel::new(&matter, &crypto, &buffers, &handler, &kv, &state);
        let $ctx = Context::new(&im, 8, 0);

        let $clock = Rc::new(TestClock::default());
        let mapping = crate::matter::publication::map_light($function.description()).unwrap();
        let $light = Lighting::new(
            Light::Single($function.clone()),
            2,
            Dataver::new(1),
            Dataver::new(2),
            Dataver::new(3),
            mapping,
            LightingExecutor::new($clock.clone()),
        );
    };
}
async fn frame(receiver: &mut crate::device::CommandReceiver) -> LightRequest {
    match receiver.next_operation().await.unwrap() {
        DeviceRequest::Light(frame) => frame,
        DeviceRequest::Power(_) => panic!("expected lighting frame"),
    }
}
fn accept(function: &Function, request: LightRequest) {
    request.mark_sent().unwrap();
    let target = request.target;
    let mut patch = LightPatch {
        power: target.power,
        brightness_percent: target.brightness_percent,
        ..Default::default()
    };
    match target.color {
        Some(LightColorTarget::Temperature(value)) => {
            patch.temperature_kelvin = Some(value);
            patch.mode = Some(LightMode::Temperature);
        }
        Some(LightColorTarget::Rgb(value)) => {
            patch.color = Some(value);
            patch.mode = Some(LightMode::Rgb);
        }
        None => {}
    }
    function.observe_light(patch).unwrap();
    request.accept().unwrap();
}

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
        let handler = ColorHandler::<true, false>(&light);
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
            request.target.color,
            Some(LightColorTarget::Temperature(6494))
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
        light.executor.advance();
        let request = frame(&mut receiver).await;
        assert!(request.final_frame);
        assert_eq!(
            request.target.color,
            Some(LightColorTarget::Temperature(4000))
        );
    });
}

#[test]
fn ct_step_with_narrower_bounds_clamps_the_target_not_the_actual_reading() {
    use c::ClusterAsyncHandler as _;
    setup!(false, function, receiver, light, clock, ctx);
    function
        .observe_light(LightPatch {
            power: Some(true),
            temperature_kelvin: Some(2700),
            mode: Some(LightMode::Temperature),
            ..Default::default()
        })
        .unwrap();
    block_on(async {
        // request bounds 200..300 mireds are narrower than the physical range.
        let data = [
            0x15, 0x24, 0, 1, 0x25, 1, 1, 0, 0x25, 2, 10, 0, 0x25, 3, 200, 0, 0x25, 4, 44, 1, 0x24,
            5, 0, 0x24, 6, 0, 0x18,
        ];
        let handler = ColorHandler::<true, false>(&light);
        let mut invoke = pin!(handler.handle_step_color_temperature(
            &ctx,
            c::StepColorTemperatureRequest::new(TLVElement::new(&data))
        ));
        let first = poll_once(invoke.as_mut()).await;
        assert!(first.is_none(), "first poll completed: {first:?}");
        let request = frame(&mut receiver).await;
        // The startup frame anchors on the real 2700K reading, not the step
        // target and not a clamped replacement value.
        assert!(matches!(
            request.target.color,
            Some(LightColorTarget::Temperature(2703))
        ));
        assert_eq!(
            function.snapshot().lighting.unwrap().temperature_kelvin,
            Some(2700)
        );
        accept(&function, request);
        invoke.await.unwrap();
        clock.set(70_000);
        light.executor.advance();
        let request = frame(&mut receiver).await;
        assert!(request.final_frame);
        assert!(matches!(
            request.target.color,
            Some(LightColorTarget::Temperature(3333))
        ));
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
                direction: MoveDirection::Up,
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
fn short_remaining_time_is_readable_without_subscription_reports() {
    let mut remaining = RemainingTime::default();
    let short = Some(Transition {
        start: 0,
        duration: 100,
    });
    assert!(!remaining.write(short, 0, 0xfffe));
    assert_eq!(remaining.value, 1);
    assert!(!remaining.write(None, 100, 0xfffe));
    let boundary = Some(Transition {
        start: 100,
        duration: 1000,
    });
    assert!(!remaining.write(boundary, 100, 0xfffe));
    assert!(remaining.write(None, 1100, 0xfffe));
    let long = Some(Transition {
        start: 1100,
        duration: 2000,
    });
    assert!(remaining.write(long, 1100, 0xfffe));
    assert!(!remaining.write(long, 3000, 0xfffe));
    assert!(remaining.write(None, 3100, 0xfffe));
    // An external long action first observed near its end is still long.
    let delayed = Some(Transition {
        start: 3100,
        duration: 2000,
    });
    assert!(!remaining.write(delayed, 5000, 0xfffe));
    assert!(remaining.write(None, 5100, 0xfffe));
}

#[test]
fn observed_zero_is_known_minimum_without_changing_power_and_relative_start() {
    use l::ClusterAsyncHandler as _;
    setup!(false, function, receiver, light, clock, ctx);
    function
        .observe_light(LightPatch {
            power: Some(true),
            brightness_percent: Some(0.0),
            ..Default::default()
        })
        .unwrap();
    block_on(async {
        assert_eq!(
            LevelHandler(&light)
                .current_level(&ctx)
                .await
                .unwrap()
                .into_option(),
            Some(1)
        );
        assert_eq!(function.snapshot().power.value(), Some(true));
        let task = light
            .level_state()
            .move_level(false, MoveModeEnum::Up, Some(1), 0, 0, Some(true))
            .unwrap()
            .unwrap()
            .resolve(light.level_state().current_level)
            .unwrap();
        assert_eq!(task.frame(0).0, 1);
        let action = light.begin(LightingTask::Level(task), None).unwrap();
        let request = frame(&mut receiver).await;
        assert_eq!(request.target.brightness_percent, Some(1.0));
        accept(&function, request);
        action.accepted().await.unwrap();
        clock.set(1000);
        light.executor.advance();
        let request = frame(&mut receiver).await;
        assert!(request.target.brightness_percent.unwrap() > 1.0);
    });
}

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
        request.skip_unchanged().unwrap();
        assert!(poll_once(accepted.as_mut()).await.is_none());
        let mut run = pin!(light.run(&ctx));
        assert!(poll_once(run.as_mut()).await.is_none());
        clock.set(20);
        light.executor.advance();
        assert!(poll_once(run.as_mut()).await.is_none());
        let request = frame(&mut receiver).await;
        assert_eq!(request.token, action.token());
        request.mark_sent().unwrap();
        for now in [40, 80, 100] {
            clock.set(now);
            light.executor.advance();
            assert!(poll_once(run.as_mut()).await.is_none());
        }
        assert!(poll_once(accepted.as_mut()).await.is_none());
        request.accept().unwrap();
        accepted.await.unwrap();
        let final_request = frame(&mut receiver).await;
        assert_eq!(final_request.token, action.token());
        assert!(final_request.final_frame);
        assert_eq!(final_request.target.brightness_percent, Some(100.0));
        accept(&function, final_request);
        assert!(poll_once(run.as_mut()).await.is_none());
        assert!(ctx.changes.borrow().contains(&(2, 8, 0)));
        let action = light
            .begin(
                LightingTask::Level(Task::Move {
                    with_on_off: false,
                    start: 254,
                    direction: MoveDirection::Down,
                    rate: 1,
                }),
                None,
            )
            .unwrap();
        let request = frame(&mut receiver).await;
        request.mark_sent().unwrap();
        light.stop(LightDimension::Brightness).await.unwrap();
        let mut accepted = pin!(action.accepted());
        assert!(poll_once(accepted.as_mut()).await.is_none());
        request.accept().unwrap();
        accepted.await.unwrap();
        clock.set(10000);
        light.executor.advance();
        assert!(poll_once(run.as_mut()).await.is_none());
        assert!(poll_once(receiver.next_operation()).await.is_none());
    });
}

#[test]
fn later_failure_takeover_and_new_relative_start_keep_observed_state() {
    setup!(true, function, receiver, light, clock, ctx);
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
                    direction: MoveDirection::Up,
                    rate: 10,
                }),
                None,
            )
            .unwrap();
        accept(&function, frame(&mut receiver).await);
        action.accepted().await.unwrap();
        clock.set(1000);
        light.executor.advance();
        let request = frame(&mut receiver).await;
        request.mark_sent().unwrap();
        request.reject(CommandError::Rejected("later failure".into()));
        clock.set(2000);
        light.executor.advance();
        light.progress();
        assert!(poll_once(receiver.next_operation()).await.is_none());
        assert!(action.ended().await.is_err());
        assert_eq!(light.level_state().remaining_time.value, 0);
        let before = function.snapshot().lighting.unwrap().brightness_percent;
        assert_eq!(
            light.refresh().level,
            Some(crate::device::brightness_to_matter_level(before.unwrap()).unwrap())
        );
        let action = light
            .begin(
                LightingTask::Level(Task::Move {
                    with_on_off: false,
                    start: 100,
                    direction: MoveDirection::Up,
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
        light.executor.advance();
        assert!(!function.light_action_current(action.token()));
        assert!(poll_once(receiver.next_operation()).await.is_none());
        let projected = light.refresh().level.unwrap();
        let next = light
            .level_state()
            .move_level(false, MoveModeEnum::Down, Some(2), 0, 0, Some(true))
            .unwrap()
            .unwrap()
            .resolve(light.level_state().current_level)
            .unwrap();
        assert_eq!(next.frame(0).0, projected);
        assert!(ctx.changes.borrow().is_empty());
    });
}

#[test]
fn color_quiet_reports_ignore_unrelated_unknown_caches_and_remaining_time_changes_version() {
    for rgb in [false, true] {
        setup!(rgb, function, receiver, light, clock, ctx);
        let patch = if rgb {
            LightPatch {
                color: Some(Rgb {
                    red: 255,
                    green: 0,
                    blue: 0,
                }),
                mode: Some(LightMode::Rgb),
                ..Default::default()
            }
        } else {
            LightPatch {
                temperature_kelvin: Some(5000),
                mode: Some(LightMode::Temperature),
                ..Default::default()
            }
        };
        function
            .observe_light(LightPatch {
                power: Some(true),
                ..patch
            })
            .unwrap();
        light.reports(&ctx);
        ctx.changes.borrow_mut().clear();
        let task = if rgb {
            ColorTask::XyMove {
                start_x: 41943,
                start_y: 21627,
                rate_x: -1000,
                rate_y: 0,
            }
        } else {
            ColorTask::ColorTemperatureMove {
                start: 200,
                rate_mireds_per_sec: 10,
                min_mireds: 154,
                max_mireds: 370,
            }
        };
        let action = light.begin(color_plan(&light, task), None).unwrap();
        block_on(async {
            accept(&function, frame(&mut receiver).await);
            action.accepted().await.unwrap();
            light.reports(&ctx);
            ctx.changes.borrow_mut().clear();
            for now in [100, 200, 300, 900] {
                clock.set(now);
                light.executor.advance();
                accept(&function, frame(&mut receiver).await);
                light.reports(&ctx);
            }
            assert_eq!(
                ctx.changes
                    .borrow()
                    .iter()
                    .filter(|(_, c, a)| *c == 0x300 && matches!(*a, 0 | 1 | 3 | 4 | 7))
                    .count(),
                0
            );
            clock.set(1000);
            light.executor.advance();
            accept(&function, frame(&mut receiver).await);
            light.reports(&ctx);
            let mut attributes: Vec<_> = ctx
                .changes
                .borrow()
                .iter()
                .filter_map(|(_, c, a)| {
                    (*c == 0x300 && matches!(*a, 0 | 1 | 3 | 4 | 7)).then_some(*a)
                })
                .collect();
            attributes.sort_unstable();
            assert_eq!(attributes, if rgb { vec![0, 1, 3, 4] } else { vec![7] });
        });
        let old = light.color_version.get();
        let transition = Some(Transition {
            start: clock.now(),
            duration: 10000,
        });
        light.update_remaining(LightDimension::Color, transition);
        light.remaining_dirty.set([false, false]);
        clock.set(clock.now() + 100);
        light.update_remaining(LightDimension::Color, transition);
        assert_ne!(light.color_version.get(), old);
        assert_eq!(light.color_state().remaining_time.value, 99);
    }
}

#[test]
fn version_reads_preserve_quiet_final_and_unknown_reports() {
    use c::ClusterAsyncHandler as _;
    use l::ClusterAsyncHandler as _;

    for dimension in [LightDimension::Brightness, LightDimension::Color] {
        setup!(false, function, receiver, light, clock, ctx);
        function
            .observe_light(LightPatch {
                power: Some(true),
                brightness_percent: Some(1.0),
                temperature_kelvin: Some(5000),
                ..Default::default()
            })
            .unwrap();
        light.reports(&ctx);
        let task = if dimension == LightDimension::Brightness {
            LightingTask::Level(Task::MoveToLevel {
                with_on_off: false,
                start: 1,
                target: 254,
                transition_ms: 1500,
            })
        } else {
            color_plan(
                &light,
                ColorTask::ColorTemperatureMoveTo {
                    start: 200,
                    target_mireds: 300,
                    transition_ms: 1500,
                },
            )
        };
        let action = light.begin(task, None).unwrap();
        block_on(async {
            accept(&function, frame(&mut receiver).await);
            action.accepted().await.unwrap();
            light.reports(&ctx);
            ctx.changes.borrow_mut().clear();
            let (cluster, attribute) = if dimension == LightDimension::Brightness {
                (8, 0)
            } else {
                (0x300, 7)
            };
            for now in [100, 900, 1000, 1100, 1500] {
                clock.set(now);
                light.executor.advance();
                accept(&function, frame(&mut receiver).await);
                if dimension == LightDimension::Brightness {
                    LevelHandler(&light).dataver();
                } else {
                    ColorHandler::<true, false>(&light).dataver();
                }
                assert!(ctx.changes.borrow().is_empty());
                light.reports(&ctx);
                assert_eq!(
                    ctx.changes.borrow().contains(&(2, cluster, attribute)),
                    matches!(now, 1000 | 1500)
                );
                ctx.changes.borrow_mut().clear();
            }
            action.ended().await.unwrap();
        });
    }

    setup!(true, function, receiver, light, clock, ctx);
    function
        .observe_light(LightPatch {
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
    function
        .observe_light(LightPatch {
            color: Some(Rgb {
                red: 0,
                green: 0,
                blue: 0,
            }),
            ..Default::default()
        })
        .unwrap();
    ColorHandler::<true, true>(&light).dataver();
    assert!(ctx.changes.borrow().is_empty());
    light.reports(&ctx);
    assert!(ctx.changes.borrow().contains(&(2, 0x300, 3)));
    assert!(ctx.changes.borrow().contains(&(2, 0x300, 4)));
    assert!(block_on(poll_once(receiver.next_operation())).is_none());
}

#[test]
fn ordinary_onoff_honors_on_level_and_repeated_off_cancels_without_reapplying_preset() {
    use crate::device::Command;
    setup!(false, function, receiver, light, clock, ctx);
    function
        .observe_light(LightPatch {
            power: Some(false),
            brightness_percent: Some(60.0),
            ..Default::default()
        })
        .unwrap();
    light.level_state_mut().on_level = Some(1);
    block_on(async {
        let mut on = pin!(light.power(Command::On));
        assert!(poll_once(on.as_mut()).await.is_none());
        let request = frame(&mut receiver).await;
        assert_eq!(request.target.power, Some(true));
        assert_eq!(request.target.brightness_percent, Some(1.0));
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
        // The caller waker wakes this waiter at the physical acceptance;
        // no runner tick is required for a single dispatch.
        repeated.await.unwrap();
        assert_eq!(
            function.snapshot().lighting.unwrap().brightness_percent,
            Some(60.0)
        );
        let mut off = pin!(light.power(Command::Off));
        assert!(poll_once(off.as_mut()).await.is_none());
        let request = frame(&mut receiver).await;
        assert_eq!(request.target.power, Some(false));
        assert_eq!(request.target.brightness_percent, Some(1.0));
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
                    direction: MoveDirection::Up,
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
            function.snapshot().lighting.unwrap().brightness_percent,
            Some(30.0)
        );
        light.level_state_mut().on_level = None;
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
    assert!(ctx.changes.borrow().is_empty());
}

#[test]
fn ct_motion_outside_caller_bounds_never_reverses_or_jumps_on_first_frame() {
    let up = ColorTask::ColorTemperatureMove {
        start: 250,
        rate_mireds_per_sec: 10,
        min_mireds: 150,
        max_mireds: 200,
    };
    assert_eq!(up.duration(), 0);
    assert_eq!(up.frame(0), (color::Target::Temperature(250), true));
    let down = ColorTask::ColorTemperatureMove {
        start: 100,
        rate_mireds_per_sec: -10,
        min_mireds: 150,
        max_mireds: 200,
    };
    assert_eq!(down.duration(), 0);
    assert_eq!(down.frame(0), (color::Target::Temperature(100), true));
    let entering = ColorTask::ColorTemperatureMove {
        start: 100,
        rate_mireds_per_sec: 10,
        min_mireds: 150,
        max_mireds: 200,
    };
    assert_eq!(entering.frame(0), (color::Target::Temperature(100), false));
    assert_eq!(
        entering.frame(10000),
        (color::Target::Temperature(200), true)
    );
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
            color_plan(
                &light,
                ColorTask::ColorTemperatureMove {
                    start: 200,
                    rate_mireds_per_sec: 1,
                    min_mireds: 154,
                    max_mireds: 370,
                },
            ),
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
            let result = ColorHandler::<true, false>(&light)
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
        ColorHandler::<true, false>(&light)
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
        let handler = ColorHandler::<true, true>(&light);
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
        light.executor.advance();
        assert!(poll_once(run.as_mut()).await.is_none());
        let final_request = frame(&mut receiver).await;
        assert!(final_request.final_frame);
        assert_eq!(
            final_request.target.color,
            Some(LightColorTarget::Temperature(4950))
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
            function.snapshot().lighting.unwrap().mode,
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
                direction: MoveDirection::Up,
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
            color_plan(
                &light,
                ColorTask::ColorTemperatureMove {
                    start: 200,
                    rate_mireds_per_sec: 1,
                    min_mireds: 154,
                    max_mireds: 370,
                },
            ),
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
            let result = ColorHandler::<true, false>(&light)
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
        ColorHandler::<true, false>(&light)
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

mod hs_handlers;

mod group;

mod hs_lifecycle;

mod hs_endpoints;

mod hs_async;

mod color_reports;

mod lifecycle;
