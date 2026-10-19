use super::{super::aggregate, support::*};
use crate::device::Power;

#[test]
fn valid_on_wins_and_off_requires_every_confirmed_member() {
    let description = dimmable();
    for (members, confirmed, expected) in [
        (
            members(&[("a", Power::On), ("b", Power::Off)]),
            2,
            Power::On,
        ),
        (
            members(&[("a", Power::Off), ("b", Power::Off)]),
            2,
            Power::Off,
        ),
        (
            members(&[("a", Power::Off), ("b", Power::Unknown)]),
            2,
            Power::Unknown,
        ),
        (
            members(&[("a", Power::On), ("b", Power::Unknown)]),
            2,
            Power::On,
        ),
        (members(&[("a", Power::On)]), 2, Power::On),
        (members(&[("a", Power::Off)]), 2, Power::Unknown),
        (members(&[("a", Power::Off)]), 1, Power::Off),
    ] {
        let (power, _) = aggregate(&description, &members, confirmed);
        assert_eq!(power, expected, "{members:?} with {confirmed} confirmed");
    }
}

#[test]
fn brightness_averages_only_available_values_for_the_derived_power() {
    let description = dimmable();
    let members = vec![
        with_light(member("a", Power::On), light(Some(20.0), None, None, None)),
        with_light(member("b", Power::On), light(Some(80.0), None, None, None)),
        with_light(member("c", Power::Off), light(Some(10.0), None, None, None)),
    ];
    let (power, state) = aggregate(&description, &members, 3);
    assert_eq!(power, Power::On);
    assert_eq!(state.brightness_percent, Some(50.0));

    let members = vec![
        with_light(member("a", Power::Off), light(Some(20.0), None, None, None)),
        with_light(member("b", Power::Off), light(Some(80.0), None, None, None)),
    ];
    let (power, state) = aggregate(&description, &members, 2);
    assert_eq!(power, Power::Off);
    assert_eq!(state.brightness_percent, Some(50.0));

    let members = vec![
        with_light(member("a", Power::On), light(None, None, None, None)),
        with_light(member("b", Power::On), light(Some(80.0), None, None, None)),
    ];
    let (power, state) = aggregate(&description, &members, 2);
    assert_eq!(power, Power::On);
    assert_eq!(state.brightness_percent, Some(80.0));

    let members = vec![
        with_light(member("a", Power::Off), light(Some(20.0), None, None, None)),
        with_light(
            member("b", Power::Unknown),
            light(Some(80.0), None, None, None),
        ),
    ];
    let (power, state) = aggregate(&description, &members, 2);
    assert_eq!(power, Power::Unknown);
    assert_eq!(state.brightness_percent, None);
}

#[test]
fn brightness_stays_unknown_when_the_group_does_not_support_it() {
    let members = vec![
        with_light(member("a", Power::On), light(Some(20.0), None, None, None)),
        with_light(member("b", Power::On), light(Some(80.0), None, None, None)),
    ];
    let (power, state) = aggregate(&on_off(), &members, 2);
    assert_eq!(power, Power::On);
    assert_eq!(state.brightness_percent, None);
}
