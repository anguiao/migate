use super::super::plan::{
    self as planner, Input, Options, Plan, PlanError, PowerPlan, Settings, State, Trajectory,
};
use crate::device::{LightDimension, LightProfile, TemperatureInterval};
use crate::lighting::{Task, color, hs};

/// A valid published light profile. Bounds are always derived from the logical
/// interval, so the planner never sees a contradictory capability combination.
fn profile(rgb: bool) -> LightProfile {
    let interval = TemperatureInterval::new(2700, 6500).unwrap();
    if rgb {
        LightProfile::ExtendedColor(interval)
    } else {
        LightProfile::ColorTemperature(interval)
    }
}

fn feedback(power: Option<bool>, level: Option<u8>) -> State {
    State {
        power,
        level,
        ..State::default()
    }
}

fn input(profile: LightProfile, settings: Settings, state: State) -> Input {
    Input {
        profile,
        settings,
        state,
        trajectory: None,
    }
}

fn options(mask: u8, override_: u8) -> Options {
    Options::new(mask, override_)
}

#[test]
fn wire_zero_clamps_minimum_and_invalid_or_ignored_requests_do_not_create_tasks() {
    let profile = profile(false);
    let settings = Settings::default();
    for wire in [0, 1] {
        let Plan::Execute {
            task: Task::Level(task),
        } = planner::move_to_level(
            wire,
            true,
            Some(0),
            options(0, 0),
            &input(profile, settings, feedback(Some(true), Some(100))),
        )
        .unwrap()
        else {
            panic!("expected a level execution")
        };
        assert_eq!(task.frame(0), (1, true, Some(false)));
        let Plan::Execute {
            task: Task::Level(task),
        } = planner::move_to_level(
            wire,
            false,
            Some(0),
            options(0, 0),
            &input(profile, settings, feedback(Some(true), Some(100))),
        )
        .unwrap()
        else {
            panic!("expected a level execution")
        };
        assert_eq!(task.frame(0), (1, true, None));
    }
    assert_eq!(
        planner::move_to_level(
            255,
            true,
            Some(0),
            options(0, 0),
            &input(profile, settings, feedback(Some(true), Some(100))),
        )
        .unwrap_err(),
        PlanError::InvalidCommand
    );
    assert!(matches!(
        planner::move_to_level(
            50,
            false,
            Some(0),
            options(0, 0),
            &input(profile, settings, feedback(Some(false), Some(100))),
        )
        .unwrap(),
        Plan::Ignore
    ));
    let Plan::Execute {
        task: Task::Level(task),
    } = planner::step(
        false,
        planner::StepMode::Up,
        255,
        Some(u16::MAX),
        options(0, 0),
        &input(profile, settings, feedback(Some(true), Some(100))),
    )
    .unwrap()
    else {
        panic!("expected a level execution")
    };
    assert_eq!(task.duration(), u16::MAX as u64 * 154 / 255 * 100);
}

#[test]
fn options_gate_ignored_commands_but_invalid_bits_fail_before_ignoring() {
    let profile = profile(true);
    let mut settings = Settings::default();
    let state = State {
        power: Some(false),
        level: Some(50),
        xy: Some((1000, 1000)),
        temperature: Some(200),
        hs: Some(hs::Hs {
            hue: 10.0,
            saturation: 100.0,
        }),
    };
    assert!(matches!(
        planner::move_to_level(
            60,
            false,
            Some(0),
            options(0, 0),
            &input(profile, settings, state)
        )
        .unwrap(),
        Plan::Ignore
    ));
    settings.level_options = 1;
    assert!(matches!(
        planner::move_to_level(
            60,
            false,
            Some(0),
            options(0, 0),
            &input(profile, settings, state)
        )
        .unwrap(),
        Plan::Execute { .. }
    ));
    settings.level_options = 0;
    assert!(matches!(
        planner::move_to_level(
            60,
            false,
            Some(0),
            options(1, 1),
            &input(profile, settings, state)
        )
        .unwrap(),
        Plan::Execute { .. }
    ));
    assert!(matches!(
        planner::move_to_level(
            60,
            false,
            Some(0),
            options(1, 0),
            &input(profile, settings, state)
        )
        .unwrap(),
        Plan::Ignore
    ));
    assert_eq!(
        planner::move_to_level(
            60,
            false,
            Some(0),
            options(2, 0),
            &input(profile, settings, state)
        )
        .unwrap_err(),
        PlanError::Constraint
    );
    assert!(matches!(
        planner::move_to_level(
            60,
            true,
            Some(0),
            options(0, 0),
            &input(profile, settings, state)
        )
        .unwrap(),
        Plan::Execute { .. }
    ));
    assert!(matches!(
        planner::move_to_color(
            1000,
            1000,
            0,
            options(0, 0),
            &input(profile, settings, state)
        )
        .unwrap(),
        Plan::Ignore
    ));
    settings.color_options = 1;
    assert!(matches!(
        planner::move_to_color(
            1000,
            1000,
            0,
            options(0, 0),
            &input(profile, settings, state)
        )
        .unwrap(),
        Plan::Execute { .. }
    ));
    assert_eq!(
        planner::stop_move_step(options(2, 0), &input(profile, settings, state)).unwrap_err(),
        PlanError::Constraint
    );
    assert_eq!(
        planner::move_to_color_temperature(200, 0, options(2, 0), &input(profile, settings, state))
            .unwrap_err(),
        PlanError::Constraint
    );
}

#[test]
fn on_level_presets_and_repeated_power_keep_the_existing_level() {
    let settings = Settings {
        on_level: Some(1),
        ..Settings::default()
    };
    let profile = profile(false);
    let PowerPlan::Begin { task, power } = planner::power(
        crate::device::Command::On,
        &input(profile, settings, feedback(Some(false), Some(60))),
    )
    .unwrap() else {
        panic!("expected an on-level preset")
    };
    assert!(power);
    assert_eq!(task.frame(0), (1, true, None), "the preset is constant");
    assert!(matches!(
        planner::power(
            crate::device::Command::On,
            &input(profile, settings, feedback(Some(true), Some(60)))
        )
        .unwrap(),
        PowerPlan::Execute(crate::device::Command::On)
    ));
    let PowerPlan::Begin { task, power } = planner::power(
        crate::device::Command::Off,
        &input(profile, settings, feedback(Some(true), Some(60))),
    )
    .unwrap() else {
        panic!("expected an off-level preset")
    };
    assert!(!power);
    assert_eq!(task.frame(0), (1, true, None), "the off target is constant");
    assert!(matches!(
        planner::power(
            crate::device::Command::Off,
            &input(profile, Settings::default(), feedback(Some(true), Some(60)))
        )
        .unwrap(),
        PowerPlan::Execute(crate::device::Command::Off)
    ));
    assert_eq!(
        planner::power(
            crate::device::Command::Toggle,
            &input(profile, Settings::default(), feedback(None, Some(60)))
        )
        .unwrap_err(),
        PlanError::InvalidState
    );
}

#[test]
fn ct_targets_clamp_to_published_bounds_and_validate_required_ranges() {
    let profile = profile(true);
    let settings = Settings::default();
    let state = State {
        power: Some(true),
        temperature: Some(200),
        ..State::default()
    };
    let input = input(profile, settings, state);
    let Plan::Execute {
        task:
            Task::Color(color::Task::ColorTemperatureMoveTo {
                start,
                target_mireds,
                transition_ms,
            }),
    } = planner::move_to_color_temperature(100, 0, options(0, 0), &input).unwrap()
    else {
        panic!("expected a temperature execution")
    };
    assert_eq!((start, target_mireds, transition_ms), (200, 154, 0));
    assert_eq!(
        planner::move_to_color_temperature(
            100,
            0,
            options(0, 0),
            &Input {
                profile: LightProfile::Dimmable,
                ..input
            }
        )
        .unwrap_err(),
        PlanError::InvalidState
    );
    assert_eq!(
        planner::move_to_color_temperature(0xffff, 0, options(0, 0), &input).unwrap_err(),
        PlanError::Constraint
    );
    let clipped = Input {
        state: State {
            temperature: Some(368),
            ..state
        },
        ..input
    };
    let Plan::Execute {
        task:
            Task::Color(color::Task::ColorTemperatureMoveTo {
                target_mireds,
                transition_ms,
                ..
            }),
    } = planner::step_color_temperature(
        planner::StepMode::Up,
        10,
        100,
        0,
        0,
        options(0, 0),
        &clipped,
    )
    .unwrap()
    else {
        panic!("expected a temperature execution")
    };
    assert_eq!((target_mireds, transition_ms), (370, 2000));
    assert_eq!(
        planner::move_color_temperature(planner::MoveMode::Up, 1, 300, 200, options(0, 0), &input)
            .unwrap_err(),
        PlanError::InvalidCommand
    );
}

#[test]
fn hs_commands_resume_the_current_trajectory_and_stop_only_hs() {
    let profile = profile(true);
    let settings = Settings::default();
    let state = State {
        power: Some(true),
        hs: Some(hs::Hs {
            hue: 10.0,
            saturation: 100.0,
        }),
        ..State::default()
    };
    let trajectory = Trajectory {
        task: color::Task::Hs(hs::HsTask {
            hue: hs::HueAxis::move_at(0.0, 20.0),
            saturation: hs::SaturationAxis::move_at(0.0, 10.0),
        }),
        elapsed_ms: 500,
        current: true,
    };
    let input = Input {
        profile,
        settings,
        state,
        trajectory: Some(trajectory),
    };
    let Plan::Execute {
        task: Task::Color(color::Task::Hs(task)),
    } = planner::move_hue(planner::MoveMode::Up, 20, options(0, 0), &input).unwrap()
    else {
        panic!("expected an HS execution")
    };
    assert_eq!(task.hue, hs::HueAxis::move_at(10.0, 20.0));
    assert_eq!(
        task.saturation,
        hs::HsTask {
            hue: hs::HueAxis::move_at(0.0, 20.0),
            saturation: hs::SaturationAxis::move_at(0.0, 10.0),
        }
        .resume(500)
        .saturation,
        "the other axis keeps its original timeline"
    );

    assert!(matches!(
        planner::move_hue(planner::MoveMode::Stop, 0, options(0, 0), &input).unwrap(),
        Plan::Stop(LightDimension::Color)
    ));
    let ct_input = Input {
        trajectory: Some(Trajectory {
            task: color::Task::ColorTemperatureMoveTo {
                start: 200,
                target_mireds: 300,
                transition_ms: 1000,
            },
            elapsed_ms: 100,
            current: true,
        }),
        ..input
    };
    assert!(matches!(
        planner::move_hue(planner::MoveMode::Stop, 0, options(0, 0), &ct_input).unwrap(),
        Plan::Ignore
    ));

    // Value errors are reported even when the command would be ignored.
    let off = Input {
        state: State {
            power: Some(false),
            ..state
        },
        ..input
    };
    assert_eq!(
        planner::move_to_hue(255, hs::Direction::Up, 0, options(0, 0), &off).unwrap_err(),
        PlanError::Constraint
    );
    assert_eq!(
        planner::move_to_color(
            1000,
            1000,
            0,
            options(0, 0),
            &Input {
                profile: LightProfile::Dimmable,
                ..off
            }
        )
        .unwrap_err(),
        PlanError::Unsupported
    );
}
