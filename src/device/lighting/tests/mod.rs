use super::{
    color::{self, Task as ColorTask},
    level::Task,
};

mod lifecycle;

#[test]
fn short_level_transition_uses_deadline_and_move_does_not_follow_slow_ack() {
    let task = Task::MoveTo {
        start: 1,
        target: 254,
        transition_ms: 100,
    };
    assert_eq!(task.frame(0), (1, false));
    assert_eq!(task.frame(20).0, 51);
    assert!(!task.frame(99).1);
    assert_eq!(task.frame(100), (254, true));
    let task = Task::Move {
        start: 100,
        target: 254,
        rate: 10,
    };
    assert_eq!(task.frame(1000).0, 110);
    assert_eq!(task.frame(2000).0, 120);
}

#[test]
fn long_color_move_saturates_at_the_xy_boundary() {
    let task = ColorTask::XyMove {
        start_x: 0,
        start_y: 0,
        rate_x: 1,
        rate_y: 0,
        maximum: 65279,
    };
    assert_eq!(task.duration(), 65279000);
    assert_eq!(task.frame(65279000), (color::Target::Xy(65279, 0), true));
}

#[test]
fn ct_motion_outside_caller_bounds_never_reverses_or_jumps_on_first_frame() {
    let up = ColorTask::TemperatureMove {
        start: 250,
        rate_per_sec: 10,
        min: 150,
        max: 200,
    };
    assert_eq!(up.duration(), 0);
    assert_eq!(up.frame(0), (color::Target::Temperature(250), true));
    let down = ColorTask::TemperatureMove {
        start: 100,
        rate_per_sec: -10,
        min: 150,
        max: 200,
    };
    assert_eq!(down.duration(), 0);
    assert_eq!(down.frame(0), (color::Target::Temperature(100), true));
    let entering = ColorTask::TemperatureMove {
        start: 100,
        rate_per_sec: 10,
        min: 150,
        max: 200,
    };
    assert_eq!(entering.frame(0), (color::Target::Temperature(100), false));
    assert_eq!(
        entering.frame(10000),
        (color::Target::Temperature(200), true)
    );
}
