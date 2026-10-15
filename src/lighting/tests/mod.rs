use super::{
    RemainingTime,
    color::{self, Task as ColorTask},
    level::{Direction, Task},
};
use crate::device::Rgb;

mod lifecycle;

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
        move_mode: Direction::Up,
        rate: 10,
    };
    assert_eq!(task.frame(1000).0, 110);
    assert_eq!(task.frame(2000).0, 120);
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
    assert!(remaining.write(task.duration(), true, 0xfffe));
    assert_eq!(remaining.value, 0xfffe);
    assert!(!remaining.write(1000, false, 0xfffe));
    assert!(remaining.write(0, false, 0xfffe));
}

#[test]
fn short_remaining_time_is_readable_without_subscription_reports() {
    let mut remaining = RemainingTime::default();
    assert!(!remaining.write(100, true, 0xfffe));
    assert_eq!(remaining.value, 1);
    assert!(!remaining.write(0, false, 0xfffe));
    assert!(!remaining.write(1000, true, 0xfffe));
    assert!(remaining.write(0, false, 0xfffe));
    assert!(remaining.write(2000, true, 0xfffe));
    assert!(!remaining.write(100, false, 0xfffe));
    assert!(remaining.write(0, false, 0xfffe));
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
