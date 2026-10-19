use super::{super::aggregate, support::*};
use crate::device::{LightMode, Power, Rgb};

#[test]
fn one_representative_supplies_temperature_rgb_and_mode() {
    let members = vec![
        with_light(
            member("a", Power::On),
            light(
                None,
                Some(3000),
                Some(0x0000ff),
                Some(LightMode::Temperature),
            ),
        ),
        with_light(
            member("b", Power::On),
            light(None, Some(4000), Some(0xff0000), Some(LightMode::Rgb)),
        ),
    ];
    let (power, state) = aggregate(&extended(), &members, 2);
    assert_eq!(power, Power::On);
    assert_eq!(state.temperature_kelvin, Some(3000));
    assert_eq!(state.color, Rgb::from_packed(0x0000ff));
    assert_eq!(state.mode, Some(LightMode::Temperature));
}

#[test]
fn valid_on_member_precedes_a_valid_off_member() {
    let members = vec![
        with_light(
            member("a", Power::Off),
            light(
                None,
                Some(3000),
                Some(0x0000ff),
                Some(LightMode::Temperature),
            ),
        ),
        with_light(
            member("b", Power::On),
            light(None, Some(4000), Some(0xff0000), Some(LightMode::Rgb)),
        ),
    ];
    let (_, state) = aggregate(&extended(), &members, 2);
    assert_eq!(state.temperature_kelvin, Some(4000));
    assert_eq!(state.color, Rgb::from_packed(0xff0000));
    assert_eq!(state.mode, Some(LightMode::Rgb));
}

#[test]
fn member_order_does_not_change_the_representative() {
    let forward = vec![
        with_light(
            member("b", Power::On),
            light(None, Some(4000), Some(0xff0000), Some(LightMode::Rgb)),
        ),
        with_light(
            member("a", Power::On),
            light(
                None,
                Some(3000),
                Some(0x0000ff),
                Some(LightMode::Temperature),
            ),
        ),
    ];
    let mut reversed = forward.clone();
    reversed.reverse();
    let first = aggregate(&extended(), &forward, 2);
    let second = aggregate(&extended(), &reversed, 2);
    assert_eq!(first, second);
    assert_eq!(first.1.temperature_kelvin, Some(3000));
    assert_eq!(first.1.color, Rgb::from_packed(0x0000ff));
    assert_eq!(first.1.mode, Some(LightMode::Temperature));
}

#[test]
fn invalid_active_color_falls_back_to_a_valid_on_member() {
    let mut non_color = with_light(
        member("0", Power::On),
        light(None, Some(2800), Some(0x00ff00), Some(LightMode::Rgb)),
    );
    non_color.supports_color = false;
    let members = vec![
        non_color,
        with_light(
            member("a", Power::On),
            light(None, Some(3000), None, Some(LightMode::Rgb)),
        ),
        with_light(
            member("b", Power::On),
            light(None, Some(4000), Some(0xff0000), Some(LightMode::Rgb)),
        ),
    ];
    let (power, state) = aggregate(&extended(), &members, 3);
    assert_eq!(power, Power::On);
    assert_eq!(state.temperature_kelvin, Some(4000));
    assert_eq!(state.color, Rgb::from_packed(0xff0000));
    assert_eq!(state.mode, Some(LightMode::Rgb));
}

#[test]
fn out_of_range_temperature_becomes_unknown_without_clamping() {
    let members = vec![with_light(
        member("a", Power::On),
        light(
            None,
            Some(9000),
            Some(0xff0000),
            Some(LightMode::Temperature),
        ),
    )];
    let (_, state) = aggregate(&extended(), &members, 1);
    assert_eq!(state.temperature_kelvin, None);
    assert_eq!(state.color, Rgb::from_packed(0xff0000));
    assert_eq!(state.mode, Some(LightMode::Temperature));
}

#[test]
fn unsupported_color_feedback_becomes_unknown() {
    let rgb_member = with_light(
        member("a", Power::On),
        light(None, Some(4000), Some(0xff0000), Some(LightMode::Rgb)),
    );
    let (_, state) = aggregate(&color_temperature(), std::slice::from_ref(&rgb_member), 1);
    assert_eq!(state.temperature_kelvin, Some(4000));
    assert_eq!(state.color, None);
    assert_eq!(state.mode, None);

    let temperature_member = with_light(
        member("b", Power::On),
        light(
            None,
            Some(4000),
            Some(0x0000ff),
            Some(LightMode::Temperature),
        ),
    );
    let (_, state) = aggregate(&dimmable(), std::slice::from_ref(&temperature_member), 1);
    assert_eq!(state.temperature_kelvin, None);
    assert_eq!(state.color, None);
    assert_eq!(state.mode, None);
}

#[test]
fn a_new_representative_clears_fields_missing_from_its_snapshot() {
    let mut members = [
        with_light(
            member("a", Power::On),
            light(None, Some(3000), Some(0xff0000), Some(LightMode::Rgb)),
        ),
        with_light(
            member("b", Power::On),
            light(None, Some(6000), None, Some(LightMode::Temperature)),
        ),
    ];
    let (_, before) = aggregate(&extended(), &members, 2);
    assert_eq!(before.color, Rgb::from_packed(0xff0000));
    members[0].power = Power::Off;
    let (_, after) = aggregate(&extended(), &members, 2);
    assert_eq!(after.temperature_kelvin, Some(6000));
    assert_eq!(after.mode, Some(LightMode::Temperature));
    assert_eq!(after.color, None);
}

#[test]
fn a_ct_group_does_not_infer_white_mode_or_clamp_member_feedback() {
    let description = description(crate::device::LightProfile::ColorTemperature(
        crate::device::TemperatureInterval::new(2700, 6400).unwrap(),
    ));
    let mut members = [
        with_light(
            member("a", Power::On),
            light(Some(20.0), Some(6500), Some(0xff0000), Some(LightMode::Rgb)),
        ),
        with_light(
            member("b", Power::Off),
            light(Some(80.0), Some(4000), None, Some(LightMode::Temperature)),
        ),
    ];
    let (power, before) = aggregate(&description, &members, 2);
    assert_eq!(power, Power::On);
    assert_eq!(before.brightness_percent, Some(20.0));
    assert_eq!(before.mode, None);
    assert_eq!(before.color, None);
    assert_eq!(before.temperature_kelvin, None);
    members[1].power = Power::On;
    let (_, after) = aggregate(&description, &members, 2);
    assert_eq!(after.brightness_percent, Some(50.0));
    assert_eq!(after.mode, Some(LightMode::Temperature));
    assert_eq!(after.temperature_kelvin, Some(4000));
}
