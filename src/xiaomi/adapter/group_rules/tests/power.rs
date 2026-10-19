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
    use Power::{Off, On, Unknown};
    for (powers, levels, expected_power, expected_level) in [
        ([On, On], [Some(20.0), Some(80.0)], On, Some(50.0)),
        ([On, Off], [Some(20.0), Some(80.0)], On, Some(20.0)),
        ([Off, Off], [Some(20.0), Some(80.0)], Off, Some(50.0)),
        ([Off, Unknown], [Some(20.0), Some(80.0)], Unknown, None),
        ([On, Unknown], [Some(20.0), Some(80.0)], On, Some(20.0)),
        ([On, On], [None, Some(80.0)], On, Some(80.0)),
        ([Off, Off], [None, Some(80.0)], Off, Some(80.0)),
        ([On, Off], [None, Some(80.0)], On, None),
        ([On, On], [None, None], On, None),
    ] {
        let members = [0, 1].map(|index| {
            with_light(
                member(if index == 0 { "a" } else { "b" }, powers[index]),
                light(levels[index], None, None, None),
            )
        });
        let (power, state) = aggregate(&description, &members, 2);
        assert_eq!(power, expected_power, "{powers:?}");
        assert_eq!(state.brightness_percent, expected_level, "{powers:?}");
    }
    let members = vec![
        with_light(member("a", On), light(Some(20.0), None, None, None)),
        with_light(member("b", On), light(Some(80.0), None, None, None)),
        with_light(member("c", Off), light(Some(10.0), None, None, None)),
    ];
    assert_eq!(
        aggregate(&description, &members, 3).1.brightness_percent,
        Some(50.0)
    );
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
