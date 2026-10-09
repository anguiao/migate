use super::{new_device, outlet};
use crate::device::{
    Availability, Capability, Category, Command, CommandError, CommandReceiver, Function,
    NumericRange, PhysicalDevice,
};
use futures_lite::future::{block_on, poll_once};

fn ready_light(rgb: bool) -> (PhysicalDevice, CommandReceiver) {
    let mut description = outlet();
    description.functions[0].category = Category::Light;
    description.functions[0].capabilities.extend([
        Capability::Brightness,
        Capability::ColorTemperature(NumericRange {
            min: 2700,
            max: 6500,
            step: 1,
        }),
    ]);
    if rgb {
        description.functions[0]
            .capabilities
            .push(Capability::ColorRgb);
    }
    let (physical, receiver) = new_device(description);
    physical.set_availability(Availability::Available);
    (physical, receiver)
}

#[test]
fn light_toggle_requires_known_state_before_entering_the_send_queue() {
    block_on(async {
        let (physical, mut receiver) = ready_light(true);
        let function = &physical.functions()[0];
        assert_eq!(
            poll_once(function.execute(Command::Toggle)).await,
            Some(Err(CommandError::UnknownState))
        );
        assert!(poll_once(receiver.next()).await.is_none());
    });
}

use crate::device::{
    DeviceRequest, LightAction, LightColorTarget, LightDimension, LightMode, LightPatch,
    LightRequest, LightTarget, Rgb,
};

fn brightness(value: f64) -> LightTarget {
    LightTarget {
        brightness_percent: Some(value),
        ..LightTarget::default()
    }
}
fn temperature(value: u32) -> LightTarget {
    LightTarget {
        color: Some(LightColorTarget::Temperature(value)),
        ..LightTarget::default()
    }
}
async fn frame(receiver: &mut CommandReceiver) -> LightRequest {
    match receiver.next_operation().await.unwrap() {
        DeviceRequest::Light(request) => request,
        DeviceRequest::Power(_) => panic!("expected a lighting frame"),
    }
}
fn begin(function: &Function, value: f64, final_frame: bool) -> LightAction {
    function
        .begin_light_action(LightDimension::Brightness, brightness(value), final_frame)
        .unwrap()
}

#[test]
fn one_inflight_frame_coalesces_ticks_and_keeps_the_final_target_and_original_waiter() {
    block_on(async {
        let (physical, mut receiver) = ready_light(true);
        let function = &physical.functions()[0];
        let action = begin(function, 20.0, false);
        let first = frame(&mut receiver).await;
        first.mark_sent().unwrap();
        for value in [30.0, 40.0] {
            function
                .submit_light_frame(action.token(), brightness(value), false)
                .unwrap();
        }
        function
            .submit_light_frame(action.token(), brightness(70.0), true)
            .unwrap();
        assert!(poll_once(receiver.next_operation()).await.is_none());
        assert!(poll_once(action.accepted()).await.is_none());
        first.accept().unwrap();
        assert!(action.accepted().await.is_ok());
        assert!(function.light_action_current(action.token()));
        let last = frame(&mut receiver).await;
        assert_eq!(last.token, action.token());
        assert_eq!(last.target, brightness(70.0));
        assert!(last.final_frame);
        last.mark_sent().unwrap();
        last.accept().unwrap();
        assert!(action.ended().await.is_ok());
        assert!(!function.light_action_current(action.token()));
        assert_eq!(
            function.snapshot().lighting.unwrap().brightness_percent,
            None
        );
        assert!(poll_once(receiver.next_operation()).await.is_none());
    });
}

#[test]
fn independent_dimensions_progress_fairly_and_temperature_rgb_share_a_slot() {
    block_on(async {
        let (physical, mut receiver) = ready_light(true);
        let function = &physical.functions()[0];
        let level = begin(function, 20.0, false);
        let first = frame(&mut receiver).await;
        first.mark_sent().unwrap();
        let white = function
            .begin_light_action(LightDimension::Color, temperature(3100), false)
            .unwrap();
        function
            .submit_light_frame(level.token(), brightness(40.0), false)
            .unwrap();
        let red = LightTarget {
            color: Some(LightColorTarget::Rgb(Rgb {
                red: 255,
                green: 0,
                blue: 0,
            })),
            ..LightTarget::default()
        };
        let color = function
            .begin_light_action(LightDimension::Color, red, true)
            .unwrap();
        assert_eq!(white.accepted().await, Err(CommandError::Superseded));
        first.accept().unwrap();
        let next = frame(&mut receiver).await;
        // The earlier retained frame progresses even while the other dimension keeps ticking.
        assert_eq!(next.token, level.token());
        next.mark_sent().unwrap();
        next.accept().unwrap();
        let next = frame(&mut receiver).await;
        assert_eq!(next.token, color.token());
        assert_eq!(next.target, red);
        next.mark_sent().unwrap();
        next.accept().unwrap();
        assert!(color.accepted().await.is_ok());
        assert!(function.light_action_current(level.token()));
    });
}

#[test]
fn invalid_new_targets_leave_the_existing_action_and_pending_frame_intact() {
    block_on(async {
        let (physical, mut receiver) = ready_light(false);
        let function = &physical.functions()[0];
        let valid = begin(function, 20.0, false);
        for invalid in [
            brightness(f64::NAN),
            brightness(-1.0),
            brightness(101.0),
            temperature(3100),
            LightTarget::default(),
        ] {
            assert!(
                function
                    .begin_light_action(LightDimension::Brightness, invalid, false)
                    .is_err()
            );
        }
        let rgb = LightTarget {
            color: Some(LightColorTarget::Rgb(Rgb {
                red: 0,
                green: 255,
                blue: 0,
            })),
            ..LightTarget::default()
        };
        assert!(matches!(
            function.begin_light_action(LightDimension::Color, rgb, true),
            Err(CommandError::Unsupported)
        ));
        assert!(function.light_action_current(valid.token()));
        assert_eq!(frame(&mut receiver).await.token, valid.token());
    });
}

#[test]
fn explicit_unsent_startup_is_superseded_but_same_action_ticks_keep_its_waiter() {
    block_on(async {
        let (physical, mut receiver) = ready_light(true);
        let function = &physical.functions()[0];
        let old = begin(function, 20.0, true);
        let new = begin(function, 40.0, false);
        assert_eq!(old.accepted().await, Err(CommandError::Superseded));
        assert_eq!(old.ended().await, Err(CommandError::Superseded));
        function
            .submit_light_frame(new.token(), brightness(60.0), true)
            .unwrap();
        assert!(poll_once(new.accepted()).await.is_none());
        let latest = frame(&mut receiver).await;
        assert_eq!(latest.target, brightness(60.0));
        latest.mark_sent().unwrap();
        latest.accept().unwrap();
        assert!(new.accepted().await.is_ok());
        assert!(new.ended().await.is_ok());
        assert!(poll_once(receiver.next_operation()).await.is_none());
    });
}

#[test]
fn stopping_a_dequeued_unsent_frame_prevents_transmission_and_preserves_the_stopped_result() {
    block_on(async {
        let (physical, mut receiver) = ready_light(true);
        let function = &physical.functions()[0];
        let action = begin(function, 20.0, false);
        let dequeued = frame(&mut receiver).await;
        assert!(dequeued.is_current());
        function.stop_light(LightDimension::Brightness).unwrap();
        assert!(!dequeued.is_current());
        assert_eq!(dequeued.mark_sent(), Err(CommandError::Stopped));
        assert_eq!(action.accepted().await, Err(CommandError::Stopped));
        dequeued.reject(CommandError::Cancelled);
        assert_eq!(action.ended().await, Err(CommandError::Stopped));
        assert!(poll_once(receiver.next_operation()).await.is_none());
    });
}

#[test]
fn stopping_a_sent_startup_waits_for_its_receipt_and_does_not_restore_pending_steps() {
    block_on(async {
        let (physical, mut receiver) = ready_light(true);
        let function = &physical.functions()[0];
        let action = begin(function, 20.0, false);
        let sent = frame(&mut receiver).await;
        sent.mark_sent().unwrap();
        function
            .submit_light_frame(action.token(), brightness(70.0), true)
            .unwrap();
        function.stop_light(LightDimension::Brightness).unwrap();
        assert_eq!(action.ended().await, Err(CommandError::Stopped));
        assert!(poll_once(action.accepted()).await.is_none());
        sent.accept().unwrap();
        assert!(action.accepted().await.is_ok());
        assert!(!function.light_action_current(action.token()));
        assert!(
            function
                .submit_light_frame(action.token(), brightness(70.0), true)
                .is_err()
        );
        assert!(poll_once(receiver.next_operation()).await.is_none());
    });
}

#[test]
fn ordinary_observation_does_not_take_over_and_explicit_external_takeover_is_dimension_scoped() {
    block_on(async {
        let (physical, mut receiver) = ready_light(true);
        let function = &physical.functions()[0];
        let level = begin(function, 20.0, false);
        let color = function
            .begin_light_action(LightDimension::Color, temperature(3100), false)
            .unwrap();
        function
            .observe_light(LightPatch {
                brightness_percent: Some(30.0),
                ..LightPatch::default()
            })
            .unwrap();
        assert!(function.light_action_current(level.token()));
        function
            .take_over_light(LightDimension::Brightness)
            .unwrap();
        function
            .observe_light(LightPatch {
                brightness_percent: Some(40.0),
                ..LightPatch::default()
            })
            .unwrap();
        assert_eq!(level.accepted().await, Err(CommandError::Superseded));
        assert!(function.light_action_current(color.token()));
        let next = frame(&mut receiver).await;
        assert_eq!(next.token, color.token());
        next.mark_sent().unwrap();
        next.accept().unwrap();
        assert_eq!(
            function.snapshot().lighting.unwrap().brightness_percent,
            Some(40.0)
        );
    });
}

#[test]
fn lamp_power_and_lighting_share_one_gate_and_valid_off_cancels_both_dimensions() {
    block_on(async {
        let (physical, mut receiver) = ready_light(true);
        let function = &physical.functions()[0];
        let level = begin(function, 20.0, false);
        let sent = frame(&mut receiver).await;
        sent.mark_sent().unwrap();
        let color = function
            .begin_light_action(LightDimension::Color, temperature(3100), false)
            .unwrap();
        let mut off = Box::pin(function.execute(Command::Off));
        assert!(poll_once(off.as_mut()).await.is_none());
        assert!(!function.light_action_current(level.token()));
        assert!(!function.light_action_current(color.token()));
        assert_eq!(color.accepted().await, Err(CommandError::Superseded));
        assert!(poll_once(receiver.next_operation()).await.is_none());
        sent.accept().unwrap();
        let power = match receiver.next_operation().await.unwrap() {
            DeviceRequest::Power(request) => request,
            DeviceRequest::Light(_) => panic!("expected off"),
        };
        assert!(!power.target);
        let newer = begin(function, 50.0, true);
        assert!(poll_once(receiver.next_operation()).await.is_none());
        power.accept().unwrap();
        assert!(off.await.is_ok());
        let latest = frame(&mut receiver).await;
        assert_eq!(latest.token, newer.token());
    });
}

#[test]
fn unavailable_paths_clear_pending_actions_without_finishing_sent_startup_early() {
    block_on(async {
        let (physical, mut receiver) = ready_light(true);
        let function = &physical.functions()[0];
        let sent_action = begin(function, 20.0, false);
        let sent = frame(&mut receiver).await;
        sent.mark_sent().unwrap();
        let pending = function
            .begin_light_action(LightDimension::Color, temperature(3100), false)
            .unwrap();
        let unavailable = CommandError::Unavailable("no control path".into());
        physical.set_availability(Availability::Unavailable("no control path".into()));
        assert_eq!(pending.accepted().await, Err(unavailable.clone()));
        assert_eq!(sent_action.ended().await, Err(unavailable));
        assert!(poll_once(sent_action.accepted()).await.is_none());
        sent.accept().unwrap();
        assert!(sent_action.accepted().await.is_ok());
        physical.set_availability(Availability::Available);
        assert!(poll_once(receiver.next_operation()).await.is_none());
        assert!(!function.light_action_current(sent_action.token()));
    });
}

#[test]
fn partial_acceptance_updates_one_snapshot_and_failure_stops_only_the_original_action() {
    block_on(async {
        let (physical, mut receiver) = ready_light(true);
        let function = &physical.functions()[0];
        let old = function
            .begin_light_action(LightDimension::Color, temperature(3100), false)
            .unwrap();
        let request = frame(&mut receiver).await;
        request.mark_sent().unwrap();
        let new = function
            .begin_light_action(LightDimension::Color, temperature(5000), true)
            .unwrap();
        let revision = function.snapshot().revision;
        function
            .observe_light(LightPatch {
                power: Some(true),
                temperature_kelvin: Some(3100),
                ..LightPatch::default()
            })
            .unwrap();
        assert_eq!(function.snapshot().revision, revision + 1);
        let failure = CommandError::Rejected("mode write rejected".into());
        request.reject(failure.clone());
        assert_eq!(old.accepted().await, Err(failure));
        assert_eq!(old.ended().await, Err(CommandError::Superseded));
        assert!(function.light_action_current(new.token()));
        let request = frame(&mut receiver).await;
        request.mark_sent().unwrap();
        request.accept().unwrap();
        assert!(new.accepted().await.is_ok());
        assert_eq!(
            function.snapshot().lighting.unwrap().temperature_kelvin,
            Some(3100)
        );
    });
}

#[test]
fn later_frame_failure_ends_an_already_accepted_action_and_clears_its_final_frame() {
    block_on(async {
        let (physical, mut receiver) = ready_light(true);
        let function = &physical.functions()[0];
        let action = begin(function, 20.0, false);
        let first = frame(&mut receiver).await;
        first.mark_sent().unwrap();
        first.accept().unwrap();
        assert!(action.accepted().await.is_ok());
        function
            .submit_light_frame(action.token(), brightness(40.0), false)
            .unwrap();
        let next = frame(&mut receiver).await;
        next.mark_sent().unwrap();
        function
            .submit_light_frame(action.token(), brightness(70.0), true)
            .unwrap();
        next.reject(CommandError::Timeout);
        assert_eq!(action.ended().await, Err(CommandError::Timeout));
        assert!(!function.light_action_current(action.token()));
        assert!(poll_once(receiver.next_operation()).await.is_none());
    });
}

#[test]
fn unchanged_nonfinal_frames_release_the_gate_but_do_not_accept_startup() {
    block_on(async {
        let (physical, mut receiver) = ready_light(true);
        let function = &physical.functions()[0];
        let action = begin(function, 20.0, false);
        frame(&mut receiver).await.skip_unchanged().unwrap();
        assert!(poll_once(action.accepted()).await.is_none());
        function
            .submit_light_frame(action.token(), brightness(20.1), false)
            .unwrap();
        frame(&mut receiver).await.skip_unchanged().unwrap();
        assert!(poll_once(action.accepted()).await.is_none());
        function
            .submit_light_frame(action.token(), brightness(30.0), true)
            .unwrap();
        let changed = frame(&mut receiver).await;
        changed.mark_sent().unwrap();
        changed.accept().unwrap();
        assert!(action.accepted().await.is_ok());
        assert!(action.ended().await.is_ok());
    });
}

#[test]
fn an_entire_unchanged_final_operation_accepts_locally_and_does_not_fabricate_values() {
    block_on(async {
        let (physical, mut receiver) = ready_light(true);
        let function = &physical.functions()[0];
        function
            .observe_light(LightPatch {
                brightness_percent: Some(20.0),
                ..LightPatch::default()
            })
            .unwrap();
        let before = function.snapshot();
        let action = begin(function, 20.0, true);
        frame(&mut receiver).await.skip_unchanged().unwrap();
        assert_eq!(action.accepted().await.unwrap(), before);
        assert_eq!(action.ended().await.unwrap(), before);
        assert_eq!(function.snapshot(), before);
    });
}

#[test]
fn state_only_startup_accepts_known_targets_without_consuming_the_frame_or_ending_the_action() {
    let gray = Rgb {
        red: 128,
        green: 128,
        blue: 128,
    };
    for (dimension, target, mode) in [
        (
            LightDimension::Color,
            LightTarget {
                color: Some(LightColorTarget::Rgb(gray)),
                ..LightTarget::default()
            },
            LightMode::Rgb,
        ),
        (
            LightDimension::Color,
            temperature(4000),
            LightMode::Temperature,
        ),
        (LightDimension::Brightness, brightness(50.0), LightMode::Rgb),
    ] {
        for sent in [false, true] {
            block_on(async {
                let (physical, mut receiver) = ready_light(true);
                let function = &physical.functions()[0];
                function
                    .observe_light(LightPatch {
                        power: Some(true),
                        brightness_percent: Some(50.0),
                        temperature_kelvin: Some(4000),
                        color: Some(gray),
                        mode: Some(mode),
                    })
                    .unwrap();
                let before = function.snapshot();
                let target = LightTarget {
                    power: Some(true),
                    ..target
                };
                let action = function
                    .begin_light_action(dimension, target, false)
                    .unwrap();
                assert!(!action.accepted_while_current());
                let mut changes = function.subscribe_light_actions().unwrap();
                assert!(
                    function
                        .accept_light_state_only_startup(action.token())
                        .unwrap()
                );
                assert_eq!(action.accepted().await.unwrap(), before);
                assert!(action.accepted_while_current());
                assert!(poll_once(action.ended()).await.is_none());
                assert!(function.light_action_current(action.token()));
                let accepted = changes.changed().await.last.unwrap();
                assert_eq!(accepted.token, action.token());
                assert_eq!(accepted.result, Ok(()));
                assert!(
                    !function
                        .accept_light_state_only_startup(action.token())
                        .unwrap()
                );

                let request = frame(&mut receiver).await;
                assert_eq!(request.token, action.token());
                assert_eq!(request.target, target);
                assert!(!request.final_frame);
                if sent {
                    request.mark_sent().unwrap();
                    request.accept().unwrap();
                } else {
                    request.skip_unchanged().unwrap();
                }
                assert_eq!(function.snapshot(), before);
                assert!(poll_once(action.ended()).await.is_none());
                function.stop_light(dimension).unwrap();
                assert_eq!(action.accepted().await.unwrap(), before);
                assert_eq!(action.ended().await, Err(CommandError::Stopped));
                assert!(!function.light_action_current(action.token()));
            });
        }
    }
}

#[test]
fn state_only_startup_requires_every_explicit_target_and_its_mode_to_be_known_and_equal() {
    let gray = Rgb {
        red: 128,
        green: 128,
        blue: 128,
    };
    let rgb = LightTarget {
        color: Some(LightColorTarget::Rgb(gray)),
        ..LightTarget::default()
    };
    let known_rgb = LightPatch {
        color: Some(gray),
        mode: Some(LightMode::Rgb),
        ..LightPatch::default()
    };
    for (target, observed) in [
        (rgb, LightPatch::default()),
        (
            rgb,
            LightPatch {
                mode: Some(LightMode::Rgb),
                ..LightPatch::default()
            },
        ),
        (
            rgb,
            LightPatch {
                mode: None,
                ..known_rgb
            },
        ),
        (
            rgb,
            LightPatch {
                mode: Some(LightMode::Temperature),
                ..known_rgb
            },
        ),
        (
            rgb,
            LightPatch {
                color: Some(Rgb { red: 129, ..gray }),
                ..known_rgb
            },
        ),
        (
            LightTarget {
                power: Some(true),
                ..rgb
            },
            known_rgb,
        ),
        (
            LightTarget {
                power: Some(true),
                ..rgb
            },
            LightPatch {
                power: Some(false),
                ..known_rgb
            },
        ),
        (
            temperature(4000),
            LightPatch {
                mode: Some(LightMode::Temperature),
                ..LightPatch::default()
            },
        ),
        (
            temperature(4000),
            LightPatch {
                temperature_kelvin: Some(4000),
                mode: Some(LightMode::Rgb),
                ..LightPatch::default()
            },
        ),
        (
            temperature(4000),
            LightPatch {
                temperature_kelvin: Some(4100),
                mode: Some(LightMode::Temperature),
                ..LightPatch::default()
            },
        ),
        (brightness(50.0), LightPatch::default()),
        (
            brightness(50.0),
            LightPatch {
                brightness_percent: Some(51.0),
                ..LightPatch::default()
            },
        ),
    ] {
        block_on(async {
            let (physical, _receiver) = ready_light(true);
            let function = &physical.functions()[0];
            function.observe_light(observed).unwrap();
            let dimension = if target.color.is_some() {
                LightDimension::Color
            } else {
                LightDimension::Brightness
            };
            let action = function
                .begin_light_action(dimension, target, false)
                .unwrap();
            let before = function.light_action_diagnostics();
            assert!(
                !function
                    .accept_light_state_only_startup(action.token())
                    .unwrap()
            );
            assert_eq!(function.light_action_diagnostics(), before);
            assert!(poll_once(action.accepted()).await.is_none());
            assert!(poll_once(action.ended()).await.is_none());
            assert!(function.light_action_current(action.token()));
        });
    }
}

#[test]
fn state_only_startup_cannot_accept_dequeued_or_sent_work_in_place_of_its_receipt() {
    block_on(async {
        let (physical, mut receiver) = ready_light(true);
        let function = &physical.functions()[0];
        function
            .observe_light(LightPatch {
                brightness_percent: Some(50.0),
                ..LightPatch::default()
            })
            .unwrap();
        let action = begin(function, 50.0, false);
        let unsent = frame(&mut receiver).await;
        assert!(
            !function
                .accept_light_state_only_startup(action.token())
                .unwrap()
        );
        function
            .submit_light_frame(action.token(), brightness(50.0), false)
            .unwrap();
        assert!(
            !function
                .accept_light_state_only_startup(action.token())
                .unwrap()
        );
        unsent.reject(CommandError::Superseded);

        let sent = frame(&mut receiver).await;
        sent.mark_sent().unwrap();
        function
            .submit_light_frame(action.token(), brightness(50.0), false)
            .unwrap();
        assert!(
            !function
                .accept_light_state_only_startup(action.token())
                .unwrap()
        );
        assert!(poll_once(action.accepted()).await.is_none());
        sent.accept().unwrap();
        assert!(action.accepted().await.is_ok());
        assert!(
            !function
                .accept_light_state_only_startup(action.token())
                .unwrap()
        );
        function.stop_light(LightDimension::Brightness).unwrap();
        assert_eq!(action.ended().await, Err(CommandError::Stopped));
    });
}

#[test]
fn state_only_startup_waits_for_an_old_sent_token_and_reevaluates_its_settled_snapshot() {
    let white = Rgb {
        red: 255,
        green: 255,
        blue: 255,
    };
    let red = Rgb {
        red: 255,
        green: 0,
        blue: 0,
    };
    for old_receipt_changes_color in [false, true] {
        block_on(async {
            let (physical, mut receiver) = ready_light(true);
            let function = &physical.functions()[0];
            function
                .observe_light(LightPatch {
                    color: Some(white),
                    mode: Some(LightMode::Rgb),
                    ..LightPatch::default()
                })
                .unwrap();
            let old = function
                .begin_light_action(
                    LightDimension::Color,
                    LightTarget {
                        color: Some(LightColorTarget::Rgb(red)),
                        ..LightTarget::default()
                    },
                    false,
                )
                .unwrap();
            let sent = frame(&mut receiver).await;
            sent.mark_sent().unwrap();
            let target = LightTarget {
                color: Some(LightColorTarget::Rgb(white)),
                ..LightTarget::default()
            };
            let new = function
                .begin_light_action(LightDimension::Color, target, false)
                .unwrap();
            assert_eq!(old.ended().await, Err(CommandError::Superseded));
            assert!(
                !function
                    .accept_light_state_only_startup(new.token())
                    .unwrap()
            );
            assert!(poll_once(new.accepted()).await.is_none());
            assert!(poll_once(receiver.next_operation()).await.is_none());

            if old_receipt_changes_color {
                function
                    .observe_light(LightPatch {
                        color: Some(red),
                        ..LightPatch::default()
                    })
                    .unwrap();
            }
            sent.accept().unwrap();
            old.accepted().await.unwrap();
            if old_receipt_changes_color {
                assert!(
                    !function
                        .accept_light_state_only_startup(new.token())
                        .unwrap()
                );
                assert!(poll_once(new.accepted()).await.is_none());
                function
                    .observe_light(LightPatch {
                        color: Some(white),
                        ..LightPatch::default()
                    })
                    .unwrap();
            }
            let settled = function.snapshot();
            assert!(
                function
                    .accept_light_state_only_startup(new.token())
                    .unwrap()
            );
            assert_eq!(new.accepted().await.unwrap(), settled);
            let pending = frame(&mut receiver).await;
            assert_eq!(pending.token, new.token());
            assert_eq!(pending.target, target);
            pending.skip_unchanged().unwrap();
            function.stop_light(LightDimension::Color).unwrap();
            assert_eq!(new.ended().await, Err(CommandError::Stopped));
        });
    }
}

#[test]
fn state_only_startup_also_waits_for_the_power_send_gate() {
    block_on(async {
        let (physical, mut receiver) = ready_light(true);
        let function = &physical.functions()[0];
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
                ..LightPatch::default()
            })
            .unwrap();
        let mut on = Box::pin(function.execute(Command::On));
        assert!(poll_once(on.as_mut()).await.is_none());
        let sent = match receiver.next_operation().await.unwrap() {
            DeviceRequest::Power(request) => request,
            DeviceRequest::Light(_) => panic!("expected power"),
        };
        sent.mark_sent().unwrap();
        let action = function
            .begin_light_action(
                LightDimension::Color,
                LightTarget {
                    power: Some(true),
                    color: Some(LightColorTarget::Rgb(white)),
                    ..LightTarget::default()
                },
                false,
            )
            .unwrap();
        assert!(
            !function
                .accept_light_state_only_startup(action.token())
                .unwrap()
        );
        assert!(poll_once(action.accepted()).await.is_none());
        sent.accept().unwrap();
        on.await.unwrap();
        assert!(
            function
                .accept_light_state_only_startup(action.token())
                .unwrap()
        );
        action.accepted().await.unwrap();
        frame(&mut receiver).await.skip_unchanged().unwrap();
        function.stop_light(LightDimension::Color).unwrap();
        assert_eq!(action.ended().await, Err(CommandError::Stopped));
    });
}

#[test]
fn state_only_startup_waits_its_turn_behind_queued_brightness_and_power_operations() {
    for queued_power in [false, true] {
        block_on(async {
            let (physical, mut receiver) = ready_light(true);
            let function = &physical.functions()[0];
            let white = Rgb {
                red: 255,
                green: 255,
                blue: 255,
            };
            function
                .observe_light(LightPatch {
                    power: Some(true),
                    brightness_percent: Some(50.0),
                    color: Some(white),
                    mode: Some(LightMode::Rgb),
                    ..LightPatch::default()
                })
                .unwrap();
            let mut power = Box::pin(function.execute(Command::On));
            let brightness = if queued_power {
                assert!(poll_once(power.as_mut()).await.is_none());
                None
            } else {
                Some(begin(function, 70.0, true))
            };
            let color = function
                .begin_light_action(
                    LightDimension::Color,
                    LightTarget {
                        color: Some(LightColorTarget::Rgb(white)),
                        ..LightTarget::default()
                    },
                    false,
                )
                .unwrap();
            assert!(
                !function
                    .accept_light_state_only_startup(color.token())
                    .unwrap()
            );
            assert!(poll_once(color.accepted()).await.is_none());

            match receiver.next_operation().await.unwrap() {
                DeviceRequest::Power(request) => {
                    assert!(queued_power);
                    request.mark_sent().unwrap();
                    request.accept().unwrap();
                }
                DeviceRequest::Light(request) => {
                    assert!(!queued_power);
                    assert_eq!(request.token.dimension(), LightDimension::Brightness);
                    request.mark_sent().unwrap();
                    function
                        .observe_light(LightPatch {
                            brightness_percent: Some(70.0),
                            ..LightPatch::default()
                        })
                        .unwrap();
                    request.accept().unwrap();
                }
            }
            if let Some(brightness) = brightness {
                brightness.accepted().await.unwrap();
            } else {
                power.await.unwrap();
            }
            assert!(
                function
                    .accept_light_state_only_startup(color.token())
                    .unwrap()
            );
            color.accepted().await.unwrap();
            let next = frame(&mut receiver).await;
            assert_eq!(next.token, color.token());
            next.skip_unchanged().unwrap();
            function.stop_light(LightDimension::Color).unwrap();
            assert_eq!(color.ended().await, Err(CommandError::Stopped));
        });
    }
}

#[test]
fn state_only_startup_rejects_stale_ended_foreign_and_unavailable_actions() {
    let (physical, receiver) = ready_light(true);
    let function = &physical.functions()[0];
    function
        .observe_light(LightPatch {
            brightness_percent: Some(50.0),
            ..LightPatch::default()
        })
        .unwrap();
    let old = begin(function, 50.0, false);
    let current = begin(function, 50.0, false);
    assert_eq!(
        function.accept_light_state_only_startup(old.token()),
        Err(CommandError::Superseded)
    );
    let (other, _other_receiver) = ready_light(true);
    let foreign = begin(&other.functions()[0], 50.0, false);
    assert_eq!(
        function.accept_light_state_only_startup(foreign.token()),
        Err(CommandError::Superseded)
    );
    function.stop_light(LightDimension::Brightness).unwrap();
    assert_eq!(
        function.accept_light_state_only_startup(current.token()),
        Err(CommandError::Superseded)
    );
    let unavailable = begin(function, 50.0, false);
    physical.set_availability(Availability::Unavailable("no control path".into()));
    assert_eq!(
        function.accept_light_state_only_startup(unavailable.token()),
        Err(CommandError::Unavailable("no control path".into()))
    );
    physical.set_availability(Availability::Available);
    let closed = begin(function, 50.0, false);
    drop(receiver);
    assert_eq!(
        function.accept_light_state_only_startup(closed.token()),
        Err(CommandError::Cancelled)
    );
}

#[test]
fn tokens_cannot_cross_function_instances_and_close_invalidates_pending_without_revival() {
    block_on(async {
        let (old_physical, mut old_receiver) = ready_light(true);
        let old_function = &old_physical.functions()[0];
        let old_action = begin(old_function, 20.0, false);
        let old_request = frame(&mut old_receiver).await;
        old_request.mark_sent().unwrap();
        let (new_physical, mut new_receiver) = ready_light(true);
        let new_function = &new_physical.functions()[0];
        let new_action = begin(new_function, 70.0, true);
        assert!(!new_function.light_action_current(old_action.token()));
        assert!(
            new_function
                .submit_light_frame(old_action.token(), brightness(10.0), true)
                .is_err()
        );
        drop(old_receiver);
        assert_eq!(old_action.ended().await, Err(CommandError::Cancelled));
        assert!(poll_once(old_action.accepted()).await.is_none());
        old_request.accept().unwrap();
        assert!(old_action.accepted().await.is_ok());
        assert!(new_function.light_action_current(new_action.token()));
        assert_eq!(frame(&mut new_receiver).await.token, new_action.token());
    });
}

#[test]
fn dropping_a_frame_cancels_its_action_without_blocking_later_work() {
    block_on(async {
        let (physical, mut receiver) = ready_light(true);
        let function = &physical.functions()[0];
        let action = begin(function, 20.0, false);
        drop(frame(&mut receiver).await);
        assert_eq!(action.accepted().await, Err(CommandError::Cancelled));
        assert_eq!(action.ended().await, Err(CommandError::Cancelled));
        let next = begin(function, 70.0, true);
        assert_eq!(frame(&mut receiver).await.token, next.token());
    });
}

#[test]
fn pending_superseded_and_stopped_actions_wake_diagnostics_before_any_dequeue() {
    block_on(async {
        let (physical, _receiver) = ready_light(true);
        let function = &physical.functions()[0];
        let mut changes = function.subscribe_light_actions().unwrap();
        let old = begin(function, 20.0, false);
        let first = changes.changed().await;
        assert_eq!(first.brightness, Some(old.token()));
        let new = begin(function, 40.0, false);
        let superseded = changes.changed().await;
        assert_eq!(superseded.brightness, Some(new.token()));
        assert_eq!(superseded.last.as_ref().unwrap().token, old.token());
        assert_eq!(
            superseded.last.unwrap().result,
            Err(CommandError::Superseded)
        );
        function.stop_light(LightDimension::Brightness).unwrap();
        let stopped = changes.changed().await;
        assert_eq!(stopped.brightness, None);
        assert_eq!(stopped.last.as_ref().unwrap().token, new.token());
        assert_eq!(stopped.last.unwrap().result, Err(CommandError::Stopped));
        assert!(poll_once(changes.changed()).await.is_none());
    });
}

#[test]
fn final_submission_cannot_be_overwritten_by_late_ticks_and_ends_only_after_acceptance() {
    block_on(async {
        let (physical, mut receiver) = ready_light(true);
        let function = &physical.functions()[0];
        let action = begin(function, 20.0, false);
        function
            .submit_light_frame(action.token(), brightness(70.0), true)
            .unwrap();
        assert!(
            function
                .submit_light_frame(action.token(), brightness(40.0), false)
                .is_err()
        );
        assert!(function.light_action_current(action.token()));
        assert!(poll_once(action.ended()).await.is_none());
        let final_request = frame(&mut receiver).await;
        assert_eq!(final_request.target, brightness(70.0));
        final_request.mark_sent().unwrap();
        assert!(poll_once(action.ended()).await.is_none());
        final_request.accept().unwrap();
        assert!(action.ended().await.is_ok());
    });
}

#[test]
fn zero_level_target_is_rejected_before_taking_over_an_existing_action() {
    let (physical, _receiver) = ready_light(true);
    let function = &physical.functions()[0];
    let old = begin(function, 20.0, false);
    for target in [
        brightness(0.0),
        LightTarget {
            power: Some(false),
            ..brightness(0.0)
        },
    ] {
        assert!(
            function
                .begin_light_action(LightDimension::Brightness, target, true)
                .is_err()
        );
    }
    assert!(function.light_action_current(old.token()));
}

#[test]
fn linked_off_invalidates_both_old_dimensions_and_retains_its_own_action() {
    block_on(async {
        let (physical, mut receiver) = ready_light(true);
        let function = &physical.functions()[0];
        let old = begin(function, 20.0, false);
        let color = function
            .begin_light_action(
                LightDimension::Color,
                LightTarget {
                    power: Some(true),
                    ..temperature(3100)
                },
                false,
            )
            .unwrap();
        let off = function
            .begin_light_action(
                LightDimension::Brightness,
                LightTarget {
                    power: Some(false),
                    ..brightness(1.0)
                },
                true,
            )
            .unwrap();
        assert_eq!(old.accepted().await, Err(CommandError::Superseded));
        assert!(!function.light_action_current(color.token()));
        assert_eq!(color.accepted().await, Err(CommandError::Superseded));
        assert!(function.light_action_current(off.token()));
        assert_eq!(frame(&mut receiver).await.token, off.token());
    });
}

#[test]
fn linked_off_tick_cancels_the_other_dimension_and_preserves_its_token() {
    block_on(async {
        let (physical, mut receiver) = ready_light(true);
        let function = &physical.functions()[0];
        let level = begin(function, 20.0, false);
        let color = function
            .begin_light_action(LightDimension::Color, temperature(3100), false)
            .unwrap();
        function
            .submit_light_frame(
                level.token(),
                LightTarget {
                    power: Some(false),
                    ..brightness(1.0)
                },
                true,
            )
            .unwrap();
        assert!(!function.light_action_current(color.token()));
        assert_eq!(color.accepted().await, Err(CommandError::Superseded));
        assert!(function.light_action_current(level.token()));
        assert_eq!(frame(&mut receiver).await.token, level.token());
    });
}

#[test]
fn external_off_withdraws_dequeued_unsent_power_but_owns_sent_power_receipt() {
    block_on(async {
        let (physical, mut receiver) = ready_light(true);
        let function = &physical.functions()[0];
        let mut on = Box::pin(function.execute(Command::On));
        assert!(poll_once(on.as_mut()).await.is_none());
        let request = match receiver.next_operation().await.unwrap() {
            DeviceRequest::Power(request) => request,
            DeviceRequest::Light(_) => panic!("expected power"),
        };
        function.take_over_light_all().unwrap();
        assert!(!request.is_current());
        assert_eq!(request.mark_sent(), Err(CommandError::Superseded));
        assert_eq!(on.await, Err(CommandError::Superseded));
        request.reject(CommandError::Cancelled);

        let mut on = Box::pin(function.execute(Command::On));
        assert!(poll_once(on.as_mut()).await.is_none());
        let request = match receiver.next_operation().await.unwrap() {
            DeviceRequest::Power(request) => request,
            DeviceRequest::Light(_) => panic!("expected power"),
        };
        request.mark_sent().unwrap();
        function.take_over_light_all().unwrap();
        assert!(poll_once(on.as_mut()).await.is_none());
        request.accept_preserving_state().unwrap();
        assert!(on.await.is_ok());
    });
}

#[test]
fn restoring_availability_does_not_release_the_original_sent_gate() {
    block_on(async {
        let (physical, mut receiver) = ready_light(true);
        let function = &physical.functions()[0];
        let old = begin(function, 20.0, false);
        let sent = frame(&mut receiver).await;
        sent.mark_sent().unwrap();
        physical.set_availability(Availability::Unavailable("no control path".into()));
        physical.set_availability(Availability::Available);
        let new = begin(function, 70.0, true);
        assert!(poll_once(receiver.next_operation()).await.is_none());
        assert!(poll_once(old.accepted()).await.is_none());
        sent.accept().unwrap();
        assert!(old.accepted().await.is_ok());
        assert!(function.light_action_current(new.token()));
        assert_eq!(frame(&mut receiver).await.token, new.token());
    });
}

#[test]
fn a_new_valid_off_withdraws_older_pending_and_dequeued_unsent_light_power_on() {
    block_on(async {
        let (physical, mut receiver) = ready_light(true);
        let function = &physical.functions()[0];
        let mut on = Box::pin(function.execute(Command::On));
        assert!(poll_once(on.as_mut()).await.is_none());
        let mut off = Box::pin(function.execute(Command::Off));
        assert!(poll_once(off.as_mut()).await.is_none());
        assert_eq!(
            poll_once(on.as_mut()).await,
            Some(Err(CommandError::Superseded))
        );
        let request = match receiver.next_operation().await.unwrap() {
            DeviceRequest::Power(request) => request,
            DeviceRequest::Light(_) => panic!("expected power"),
        };
        assert!(!request.target);
        request.accept().unwrap();
        off.await.unwrap();

        let mut on = Box::pin(function.execute(Command::On));
        assert!(poll_once(on.as_mut()).await.is_none());
        let request = match receiver.next_operation().await.unwrap() {
            DeviceRequest::Power(request) => request,
            DeviceRequest::Light(_) => panic!("expected power"),
        };
        let mut off = Box::pin(function.execute(Command::Off));
        assert!(poll_once(off.as_mut()).await.is_none());
        assert!(!request.is_current());
        assert_eq!(request.mark_sent(), Err(CommandError::Superseded));
        assert_eq!(on.await, Err(CommandError::Superseded));
        request.reject(CommandError::Cancelled);
        let request = match receiver.next_operation().await.unwrap() {
            DeviceRequest::Power(request) => request,
            DeviceRequest::Light(_) => panic!("expected power"),
        };
        assert!(!request.target);
        request.accept().unwrap();
        off.await.unwrap();
    });
}

#[test]
fn unknown_toggle_does_not_cancel_either_existing_light_dimension() {
    block_on(async {
        let (physical, _receiver) = ready_light(true);
        let function = &physical.functions()[0];
        let level = begin(function, 20.0, false);
        let color = function
            .begin_light_action(LightDimension::Color, temperature(3100), false)
            .unwrap();
        assert_eq!(
            function.execute(Command::Toggle).await,
            Err(CommandError::UnknownState)
        );
        assert!(function.light_action_current(level.token()));
        assert!(function.light_action_current(color.token()));
    });
}

#[test]
fn action_tokens_do_not_cross_different_device_keys() {
    let (old_physical, _old_receiver) = ready_light(true);
    let old = begin(&old_physical.functions()[0], 20.0, false);
    let mut description = old_physical.description();
    description.key.id = "different-light".into();
    let (new_physical, _new_receiver) = new_device(description);
    new_physical.set_availability(Availability::Available);
    let new_function = &new_physical.functions()[0];
    let new = begin(new_function, 70.0, true);
    assert!(!new_function.light_action_current(old.token()));
    assert!(
        new_function
            .submit_light_frame(old.token(), brightness(50.0), true)
            .is_err()
    );
    assert!(new_function.light_action_current(new.token()));
}

#[test]
fn diagnostics_distinguish_cancelled_action_from_its_pending_original_receipt() {
    block_on(async {
        let (physical, mut receiver) = ready_light(true);
        let function = &physical.functions()[0];
        let action = begin(function, 20.0, false);
        let sent = frame(&mut receiver).await;
        assert_eq!(
            function
                .light_action_diagnostics()
                .unwrap()
                .awaiting_startup,
            None
        );
        sent.mark_sent().unwrap();
        assert_eq!(
            function
                .light_action_diagnostics()
                .unwrap()
                .awaiting_startup,
            Some(action.token())
        );
        function.stop_light(LightDimension::Brightness).unwrap();
        let stopped = function.light_action_diagnostics().unwrap();
        assert_eq!(stopped.brightness, None);
        assert_eq!(stopped.awaiting_startup, Some(action.token()));
        assert_eq!(stopped.last.unwrap().result, Err(CommandError::Stopped));
        sent.accept().unwrap();
        let ended = function.light_action_diagnostics().unwrap();
        assert_eq!(ended.awaiting_startup, None);
        assert_eq!(ended.last.unwrap().result, Err(CommandError::Stopped));
    });
}

#[test]
fn accepted_startup_is_reported_while_a_nonfinal_action_remains_current() {
    block_on(async {
        let (physical, mut receiver) = ready_light(true);
        let function = &physical.functions()[0];
        let action = begin(function, 20.0, false);
        let sent = frame(&mut receiver).await;
        sent.mark_sent().unwrap();
        sent.accept().unwrap();
        let diagnostics = function.light_action_diagnostics().unwrap();
        assert_eq!(diagnostics.brightness, Some(action.token()));
        assert_eq!(diagnostics.awaiting_startup, None);
        let result = diagnostics.last.unwrap();
        assert_eq!(result.token, action.token());
        assert_eq!(result.result, Ok(()));
    });
}

#[test]
fn startup_acceptance_ownership_survives_a_later_frame_failure() {
    block_on(async {
        let (physical, mut receiver) = ready_light(true);
        let function = &physical.functions()[0];
        let action = begin(function, 20.0, false);
        assert!(!action.accepted_while_current());
        let first = frame(&mut receiver).await;
        first.mark_sent().unwrap();
        first.accept().unwrap();
        action.accepted().await.unwrap();
        assert!(action.accepted_while_current());
        function
            .submit_light_frame(action.token(), brightness(30.0), false)
            .unwrap();
        let later = frame(&mut receiver).await;
        later.mark_sent().unwrap();
        later.reject(CommandError::Timeout);
        assert_eq!(action.ended().await, Err(CommandError::Timeout));
        assert!(action.accepted_while_current());
        assert!(!function.light_action_current(action.token()));
    });
}

#[test]
fn a_startup_receipt_after_cancellation_does_not_acquire_acceptance_ownership() {
    for external in [false, true] {
        block_on(async {
            let (physical, mut receiver) = ready_light(true);
            let function = &physical.functions()[0];
            let action = begin(function, 20.0, false);
            let first = frame(&mut receiver).await;
            first.mark_sent().unwrap();
            if external {
                function
                    .take_over_light(LightDimension::Brightness)
                    .unwrap();
            } else {
                function.stop_light(LightDimension::Brightness).unwrap();
            }
            assert!(action.ended().await.is_err());
            assert!(poll_once(action.accepted()).await.is_none());
            assert!(!action.accepted_while_current());
            first.accept().unwrap();
            action.accepted().await.unwrap();
            assert!(!action.accepted_while_current());
        });
    }
}

#[test]
fn merging_unsent_ticks_retains_startup_power_until_a_frame_is_submitted() {
    block_on(async {
        let (physical, mut receiver) = ready_light(true);
        let function = &physical.functions()[0];
        let action = function
            .begin_light_action(
                LightDimension::Brightness,
                LightTarget {
                    power: Some(true),
                    ..brightness(20.0)
                },
                false,
            )
            .unwrap();
        function
            .submit_light_frame(action.token(), brightness(30.0), false)
            .unwrap();
        function
            .submit_light_frame(action.token(), brightness(70.0), true)
            .unwrap();
        let request = frame(&mut receiver).await;
        assert_eq!(
            request.target,
            LightTarget {
                power: Some(true),
                ..brightness(70.0)
            }
        );
        request.mark_sent().unwrap();
        request.accept().unwrap();
        assert!(action.accepted().await.is_ok());
    });
}

#[test]
fn a_new_explicit_linked_power_value_overrides_the_pending_startup_value() {
    block_on(async {
        let (physical, mut receiver) = ready_light(true);
        let function = &physical.functions()[0];
        let action = function
            .begin_light_action(
                LightDimension::Brightness,
                LightTarget {
                    power: Some(true),
                    ..brightness(20.0)
                },
                false,
            )
            .unwrap();
        function
            .submit_light_frame(
                action.token(),
                LightTarget {
                    power: Some(false),
                    ..brightness(1.0)
                },
                true,
            )
            .unwrap();
        assert_eq!(frame(&mut receiver).await.target.power, Some(false));
    });
}

#[test]
fn dequeued_startup_power_is_not_repeated_in_later_controller_ticks() {
    block_on(async {
        let (physical, mut receiver) = ready_light(true);
        let function = &physical.functions()[0];
        let action = function
            .begin_light_action(
                LightDimension::Brightness,
                LightTarget {
                    power: Some(true),
                    ..brightness(20.0)
                },
                false,
            )
            .unwrap();
        let first = frame(&mut receiver).await;
        assert_eq!(first.target.power, Some(true));
        first.mark_sent().unwrap();
        function
            .submit_light_frame(action.token(), brightness(70.0), true)
            .unwrap();
        first.accept().unwrap();
        assert_eq!(frame(&mut receiver).await.target.power, None);
    });
}

#[test]
fn newer_ticks_withdraw_dequeued_unsent_frames_without_ending_the_action() {
    for power in [None, Some(false)] {
        block_on(async {
            let (physical, mut receiver) = ready_light(true);
            let function = &physical.functions()[0];
            let action = function
                .begin_light_action(
                    LightDimension::Brightness,
                    LightTarget {
                        power: Some(true),
                        ..brightness(20.0)
                    },
                    false,
                )
                .unwrap();
            let old = frame(&mut receiver).await;
            function
                .submit_light_frame(
                    action.token(),
                    LightTarget {
                        power,
                        ..brightness(70.0)
                    },
                    true,
                )
                .unwrap();
            assert!(!old.is_current());
            assert_eq!(old.mark_sent(), Err(CommandError::Superseded));
            assert!(function.light_action_current(action.token()));
            assert!(poll_once(action.accepted()).await.is_none());
            assert!(poll_once(action.ended()).await.is_none());
            old.reject(CommandError::Superseded);
            let latest = frame(&mut receiver).await;
            assert_eq!(latest.target.power, power.or(Some(true)));
            assert_eq!(latest.target.brightness_percent, Some(70.0));
            assert_eq!(latest.token, action.token());
            assert!(latest.final_frame);
            latest.mark_sent().unwrap();
            latest.accept().unwrap();
            action.accepted().await.unwrap();
            action.ended().await.unwrap();
        });
    }
}
