use super::*;

fn close(actual: f64, expected: f64) {
    assert!((actual - expected).abs() < 1e-9, "{actual} != {expected}");
}

#[test]
fn hue_paths_preserve_direction_longest_and_equal_distance_up() {
    close(hue_delta(250.0, 4.0, Direction::Shortest, 254.0), 8.0);
    close(hue_delta(250.0, 4.0, Direction::Longest, 254.0), -246.0);
    close(hue_delta(4.0, 250.0, Direction::Up, 254.0), 246.0);
    close(hue_delta(4.0, 250.0, Direction::Down, 254.0), -8.0);
    for direction in [Direction::Shortest, Direction::Longest] {
        close(hue_delta(200.0, 73.0, direction, 254.0), 127.0);
    }
    close(hue_delta(40.0, 40.0, Direction::Shortest, 254.0), 0.0);
    close(hue_delta(40.0, 40.0, Direction::Longest, 254.0), 254.0);
    let short = HueAxis::move_to(250.0, 4.0, Direction::Shortest, 1000, 254.0);
    let long = HueAxis::move_to(250.0, 4.0, Direction::Longest, 1000, 254.0);
    close(short.0.frame(500), 0.0);
    close(long.0.frame(500), 127.0);
    close(short.0.frame(1000), 4.0);
    close(long.0.frame(1000), 4.0);
    let exact_target = HueAxis::move_to(250.0, 254.0, Direction::Up, 1000, 254.0);
    close(exact_target.0.frame(1000), 254.0);
}

#[test]
fn continuous_and_step_hue_wrap_without_exposing_reserved_255() {
    let up = HueAxis::move_at(250.0, 10.0, 254.0);
    let down = HueAxis::move_at(4.0, -10.0, 254.0);
    close(up.0.frame(1000), 6.0);
    close(down.0.frame(1000), 248.0);
    close(up.0.frame(26_400), 6.0);
    assert_eq!(up.0.duration(), u64::MAX);
    close(HueAxis::step(250.0, 8.0, 1000, 254.0).0.frame(1000), 4.0);
    close(HueAxis::step(4.0, -8.0, 0, 254.0).0.frame(0), 250.0);
    let task = HsTask {
        hue: up,
        saturation: SaturationAxis::hold(254.0),
    };
    assert!(!task.frame(u64::MAX).1);
}

#[test]
fn saturation_stops_at_its_bound_and_has_an_exact_final_frame() {
    let up = SaturationAxis::move_at(250.0, 3.0, 254.0);
    assert_eq!(up.0.duration(), 1334);
    assert!(up.0.frame(1333) < 254.0);
    close(up.0.frame(1334), 254.0);
    let down = SaturationAxis::move_at(4.0, -3.0, 254.0);
    assert_eq!(down.0.duration(), 1334);
    assert!(down.0.frame(1333) > 0.0);
    close(down.0.frame(1334), 0.0);
    let target = SaturationAxis::move_to(250.0, 254.0, 1000);
    close(target.0.frame(500), 252.0);
    close(target.0.frame(1000), 254.0);
    assert_eq!(target.0.duration(), 1000);
    close(SaturationAxis::move_to(10.0, 200.0, 0).0.frame(0), 200.0);
}

#[test]
fn replacing_one_axis_preserves_the_others_original_deadline() {
    let first = HsTask {
        hue: HueAxis::move_to(20.0, 120.0, Direction::Up, 2000, 254.0),
        saturation: SaturationAxis::move_to(50.0, 250.0, 4000),
    };
    let mut next = first.resume(1000);
    close(next.frame(0).0.hue, 70.0);
    close(next.frame(0).0.saturation, 100.0);
    next.hue = HueAxis::move_to(70.0, 90.0, Direction::Up, 500, 254.0);
    assert_eq!(next.duration(), 3000);
    let (middle, ended) = next.frame(1000);
    assert!(!ended);
    close(middle.hue, 90.0);
    close(middle.saturation, 150.0);
    let (last, ended) = next.frame(3000);
    assert!(ended);
    close(last.hue, 90.0);
    close(last.saturation, 250.0);

    let mut saturation_command = first.resume(500).resume(500);
    saturation_command.saturation = SaturationAxis::move_to(100.0, 0.0, 100);
    assert_eq!(saturation_command.duration(), 1000);
    close(saturation_command.frame(500).0.hue, 95.0);
    close(saturation_command.frame(500).0.saturation, 0.0);
    assert!(saturation_command.frame(1000).1);
}

#[test]
fn resuming_continuous_hue_retains_rate_and_phase() {
    let first = HsTask {
        hue: HueAxis::move_at(250.0, 10.0, 254.0),
        saturation: SaturationAxis::hold(254.0),
    };
    let mut next = first.resume(1000);
    next.saturation = SaturationAxis::move_to(254.0, 0.0, 1000);
    let (color, ended) = next.frame(1000);
    close(color.hue, 16.0);
    close(color.saturation, 0.0);
    assert!(!ended);
    assert_eq!(next.duration(), u64::MAX);
}

#[test]
fn only_a_wholly_achromatic_trajectory_keeps_its_physical_rgb_constant() {
    let mut task = HsTask {
        hue: HueAxis::move_at(0.0, 10.0, 254.0),
        saturation: SaturationAxis::hold(0.0),
    };
    assert!(task.is_achromatic());
    task.saturation = SaturationAxis::hold(10.0);
    assert!(!task.is_achromatic());
    task.saturation = SaturationAxis::move_to(0.0, 254.0, 1000);
    assert!(!task.is_achromatic());
    task.saturation = SaturationAxis::move_to(254.0, 0.0, 1000);
    assert!(!task.is_achromatic());
    assert!(!task.resume(999).is_achromatic());
    assert!(task.resume(1000).is_achromatic());
    task.saturation = SaturationAxis::move_at(0.0, 10.0, 254.0);
    assert!(!task.is_achromatic());
    task.saturation = SaturationAxis::move_at(0.0, -10.0, 254.0);
    assert!(task.is_achromatic());
}
