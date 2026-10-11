use super::{new_device, outlet};
use crate::device::{
    Availability, Capability, Category, Command, CommandError, CommandReceiver, DeviceRequest,
    LightColorTarget, LightDimension, LightMode, LightPatch, LightTarget, NumericRange,
    PhysicalDevice, PhysicalDeviceDescription, Power, Rgb,
};
use futures_lite::future::{block_on, poll_once};

const TEMPERATURE: NumericRange = NumericRange {
    min: 2700,
    max: 6500,
    step: 1,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum K {
    OnOff,
    Brightness,
    Temperature,
    Rgb,
}

const ALL: [K; 4] = [K::OnOff, K::Brightness, K::Temperature, K::Rgb];

fn capability(kind: K) -> Capability {
    match kind {
        K::OnOff => Capability::OnOff,
        K::Brightness => Capability::Brightness,
        K::Temperature => Capability::ColorTemperature(TEMPERATURE),
        K::Rgb => Capability::ColorRgb,
    }
}

/// All 15 nonempty capability subsets in a stable order.
fn subsets() -> Vec<Vec<K>> {
    (1u8..16)
        .map(|mask| {
            ALL.iter()
                .enumerate()
                .filter(|(index, _)| mask & (1 << index) != 0)
                .map(|(_, kind)| *kind)
                .collect()
        })
        .collect()
}

fn has(kinds: &[K], kind: K) -> bool {
    kinds.contains(&kind)
}

fn light(kinds: &[K]) -> PhysicalDeviceDescription {
    let mut description = outlet();
    description.functions[0].category = Category::Light;
    description.functions[0].capabilities = kinds.iter().map(|kind| capability(*kind)).collect();
    description
}

fn device(kinds: &[K]) -> (PhysicalDevice, CommandReceiver) {
    new_device(light(kinds))
}

fn brightness_target() -> LightTarget {
    LightTarget {
        brightness_percent: Some(50.0),
        ..Default::default()
    }
}

fn temperature_target() -> LightTarget {
    LightTarget {
        color: Some(LightColorTarget::Temperature(3100)),
        ..Default::default()
    }
}

fn rgb_target() -> LightTarget {
    LightTarget {
        color: Some(LightColorTarget::Rgb(Rgb {
            red: 10,
            green: 20,
            blue: 30,
        })),
        ..Default::default()
    }
}

fn check_patch(kinds: &[K], supported: bool, patch: LightPatch) {
    let (physical, _receiver) = device(kinds);
    let function = &physical.functions()[0];
    let before = function.snapshot();
    let result = function.observe_light(patch);
    if supported {
        assert!(result.is_ok(), "{kinds:?} should accept {patch:?}");
    } else {
        assert_eq!(
            result,
            Err(CommandError::Unsupported),
            "{kinds:?} should reject {patch:?}"
        );
        assert_eq!(
            function.snapshot(),
            before,
            "{kinds:?} mutated state for {patch:?}"
        );
    }
}

#[test]
fn every_nonempty_capability_subset_is_a_valid_light_in_any_order() {
    let subsets = subsets();
    assert_eq!(subsets.len(), 15);
    for kinds in subsets {
        assert!(light(&kinds).functions[0].valid_capabilities(), "{kinds:?}");
        let mut reversed = kinds.clone();
        reversed.reverse();
        assert!(
            light(&reversed).functions[0].valid_capabilities(),
            "{reversed:?}"
        );
    }
}

#[test]
fn duplicate_or_malformed_capabilities_are_rejected_at_the_description_boundary() {
    for kinds in [
        vec![K::OnOff, K::OnOff],
        vec![K::Brightness, K::Brightness],
        vec![K::Rgb, K::Rgb],
        vec![K::Temperature, K::Temperature],
    ] {
        assert!(
            !light(&kinds).functions[0].valid_capabilities(),
            "{kinds:?}"
        );
    }
    let mut description = light(&[K::Temperature]);
    description.functions[0]
        .capabilities
        .push(Capability::ColorTemperature(NumericRange {
            min: 3000,
            max: 6000,
            step: 1,
        }));
    assert!(!description.functions[0].valid_capabilities());

    for range in [
        NumericRange {
            min: 2700,
            max: 2700,
            step: 1,
        },
        NumericRange {
            min: 2700,
            max: 6500,
            step: 0,
        },
        NumericRange {
            min: 2700,
            max: 6500,
            step: 4000,
        },
        NumericRange {
            min: 0,
            max: 6500,
            step: 1,
        },
        NumericRange {
            min: 2700,
            max: 1_000_001,
            step: 1,
        },
    ] {
        let mut description = light(&[K::Temperature]);
        description.functions[0].capabilities = vec![Capability::ColorTemperature(range)];
        assert!(!description.functions[0].valid_capabilities(), "{range:?}");
    }

    let mut empty = light(&[K::OnOff]);
    empty.functions[0].capabilities.clear();
    assert!(!empty.functions[0].valid_capabilities());

    // The outlet contract stays exactly OnOff.
    assert!(outlet().functions[0].valid_capabilities());
    let mut outlet_with_brightness = outlet();
    outlet_with_brightness.functions[0]
        .capabilities
        .push(Capability::Brightness);
    assert!(!outlet_with_brightness.functions[0].valid_capabilities());
    let mut light_on_off_only = outlet();
    light_on_off_only.functions[0].category = Category::Light;
    assert!(light_on_off_only.functions[0].valid_capabilities());
}

#[test]
fn generic_kelvin_ranges_are_not_excluded_by_matter_mired_representability() {
    for range in [
        NumericRange {
            min: 4500,
            max: 4501,
            step: 1,
        },
        NumericRange {
            min: 10,
            max: 6500,
            step: 1,
        },
        NumericRange {
            min: 1,
            max: 6500,
            step: 1,
        },
    ] {
        assert!(range.mired_bounds().is_err());
        let mut description = light(&[K::Temperature]);
        description.functions[0].capabilities = vec![Capability::ColorTemperature(range)];
        assert!(
            description.functions[0].valid_capabilities(),
            "{range:?} is a generic Kelvin range"
        );
    }
}

#[test]
fn lighting_state_and_controllers_follow_present_capabilities() {
    for kinds in subsets() {
        let (physical, _receiver) = device(&kinds);
        let function = &physical.functions()[0];
        let snapshot = function.snapshot();
        assert_eq!(snapshot.power, Power::Unknown, "{kinds:?}");
        let has_lighting =
            has(&kinds, K::Brightness) || has(&kinds, K::Temperature) || has(&kinds, K::Rgb);
        assert_eq!(snapshot.lighting.is_some(), has_lighting, "{kinds:?}");
        assert_eq!(
            function.light_action_diagnostics().is_some(),
            has_lighting,
            "{kinds:?}"
        );
        let Some(lighting) = snapshot.lighting else {
            continue;
        };
        assert_eq!(lighting.brightness_percent, None);
        assert_eq!(lighting.temperature_kelvin, None);
        assert_eq!(lighting.color, None);
        let expected = match (has(&kinds, K::Temperature), has(&kinds, K::Rgb)) {
            (true, true) => None,
            (true, false) => Some(LightMode::Temperature),
            (false, true) => Some(LightMode::Rgb),
            (false, false) => None,
        };
        assert_eq!(lighting.mode, expected, "{kinds:?}");
    }
}

#[test]
fn reports_only_apply_when_every_present_field_has_its_capability() {
    for kinds in subsets() {
        let has_on_off = has(&kinds, K::OnOff);
        let has_brightness = has(&kinds, K::Brightness);
        let has_temperature = has(&kinds, K::Temperature);
        let has_rgb = has(&kinds, K::Rgb);
        check_patch(
            &kinds,
            has_on_off,
            LightPatch {
                power: Some(true),
                ..Default::default()
            },
        );
        check_patch(
            &kinds,
            has_brightness,
            LightPatch {
                brightness_percent: Some(50.0),
                ..Default::default()
            },
        );
        check_patch(
            &kinds,
            has_temperature,
            LightPatch {
                temperature_kelvin: Some(3100),
                ..Default::default()
            },
        );
        check_patch(
            &kinds,
            has_rgb,
            LightPatch {
                color: Some(Rgb {
                    red: 1,
                    green: 2,
                    blue: 3,
                }),
                ..Default::default()
            },
        );
        check_patch(
            &kinds,
            has_temperature,
            LightPatch {
                mode: Some(LightMode::Temperature),
                ..Default::default()
            },
        );
        check_patch(
            &kinds,
            has_rgb,
            LightPatch {
                mode: Some(LightMode::Rgb),
                ..Default::default()
            },
        );
        check_patch(
            &kinds,
            has_on_off && has_brightness,
            LightPatch {
                power: Some(true),
                brightness_percent: Some(50.0),
                ..Default::default()
            },
        );
        check_patch(
            &kinds,
            has_brightness && has_temperature,
            LightPatch {
                brightness_percent: Some(50.0),
                temperature_kelvin: Some(3100),
                ..Default::default()
            },
        );
    }
}

#[test]
fn unsupported_commands_and_dimensions_cannot_mutate_state() {
    block_on(async {
        for kinds in subsets() {
            let (physical, _receiver) = device(&kinds);
            let function = &physical.functions()[0];
            physical.set_availability(Availability::Available);
            let revision = function.snapshot().revision;
            if !has(&kinds, K::OnOff) {
                assert_eq!(
                    function.execute(Command::On).await,
                    Err(CommandError::Unsupported),
                    "{kinds:?}"
                );
                assert_eq!(function.snapshot().revision, revision, "{kinds:?}");
            }
            for (dimension, target, supported) in [
                (
                    LightDimension::Brightness,
                    brightness_target(),
                    has(&kinds, K::Brightness),
                ),
                (
                    LightDimension::Color,
                    temperature_target(),
                    has(&kinds, K::Temperature),
                ),
                (LightDimension::Color, rgb_target(), has(&kinds, K::Rgb)),
            ] {
                let before = function.light_action_diagnostics();
                let result = function.begin_light_action(dimension, target, false);
                if supported {
                    assert!(result.is_ok(), "{kinds:?} {dimension:?}");
                } else {
                    assert_eq!(
                        result.err(),
                        Some(CommandError::Unsupported),
                        "{kinds:?} {dimension:?}"
                    );
                    assert_eq!(
                        function.light_action_diagnostics(),
                        before,
                        "{kinds:?} {dimension:?}"
                    );
                }
            }
        }
    });
}

#[test]
fn supported_dimensions_are_usable_without_other_capabilities() {
    block_on(async {
        for (kinds, dimension, target) in [
            (
                vec![K::Brightness],
                LightDimension::Brightness,
                brightness_target(),
            ),
            (
                vec![K::Temperature],
                LightDimension::Color,
                temperature_target(),
            ),
            (vec![K::Rgb], LightDimension::Color, rgb_target()),
            (
                vec![K::Brightness, K::Rgb],
                LightDimension::Color,
                rgb_target(),
            ),
        ] {
            let (physical, mut receiver) = device(&kinds);
            let function = &physical.functions()[0];
            physical.set_availability(Availability::Available);
            let action = function
                .begin_light_action(dimension, target, true)
                .unwrap_or_else(|error| panic!("{kinds:?} rejected its dimension: {error}"));
            let request = match receiver.next_operation().await.unwrap() {
                DeviceRequest::Light(request) => request,
                DeviceRequest::Power(_) => panic!("expected a lighting frame"),
            };
            assert_eq!(request.token, action.token());
            assert_eq!(request.target, target);
            request.mark_sent().unwrap();
            request.accept().unwrap();
            action.accepted().await.unwrap();
            action.ended().await.unwrap();
            let snapshot = function.snapshot();
            assert_eq!(snapshot.power, Power::Unknown);
            let lighting = snapshot.lighting.expect("lighting state");
            assert_eq!(lighting.brightness_percent, None);
            assert_eq!(lighting.temperature_kelvin, None);
            assert_eq!(lighting.color, None);
        }
    });
}

#[test]
fn single_color_capability_actions_do_not_require_an_unrelated_mode_observation() {
    let rgb = Rgb {
        red: 10,
        green: 20,
        blue: 30,
    };
    block_on(async {
        let (physical, _receiver) = device(&[K::Rgb]);
        let function = &physical.functions()[0];
        physical.set_availability(Availability::Available);
        function
            .observe_light(LightPatch {
                color: Some(rgb),
                ..Default::default()
            })
            .unwrap();
        let action = function
            .begin_light_action(
                LightDimension::Color,
                LightTarget {
                    color: Some(LightColorTarget::Rgb(rgb)),
                    ..Default::default()
                },
                false,
            )
            .unwrap();
        assert!(
            function
                .accept_light_state_only_startup(action.token())
                .unwrap()
        );
        assert!(action.accepted().await.is_ok());
    });
    block_on(async {
        let (physical, _receiver) = device(&[K::Temperature]);
        let function = &physical.functions()[0];
        physical.set_availability(Availability::Available);
        function
            .observe_light(LightPatch {
                temperature_kelvin: Some(3100),
                ..Default::default()
            })
            .unwrap();
        let action = function
            .begin_light_action(LightDimension::Color, temperature_target(), false)
            .unwrap();
        assert!(
            function
                .accept_light_state_only_startup(action.token())
                .unwrap()
        );
        assert!(action.accepted().await.is_ok());
    });
    block_on(async {
        let (physical, _receiver) = device(&[K::Temperature, K::Rgb]);
        let function = &physical.functions()[0];
        physical.set_availability(Availability::Available);
        function
            .observe_light(LightPatch {
                color: Some(rgb),
                ..Default::default()
            })
            .unwrap();
        let action = function
            .begin_light_action(
                LightDimension::Color,
                LightTarget {
                    color: Some(LightColorTarget::Rgb(rgb)),
                    ..Default::default()
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
        function
            .observe_light(LightPatch {
                mode: Some(LightMode::Rgb),
                ..Default::default()
            })
            .unwrap();
        assert!(
            function
                .accept_light_state_only_startup(action.token())
                .unwrap()
        );
        assert!(action.accepted().await.is_ok());
    });
}

#[test]
fn dual_color_mode_starts_unknown_and_follows_observations() {
    let (physical, _receiver) = device(&[K::Temperature, K::Rgb]);
    let function = &physical.functions()[0];
    assert_eq!(function.snapshot().lighting.unwrap().mode, None);
    function
        .observe_light(LightPatch {
            mode: Some(LightMode::Temperature),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        function.snapshot().lighting.unwrap().mode,
        Some(LightMode::Temperature)
    );
    function
        .observe_light(LightPatch {
            mode: Some(LightMode::Rgb),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        function.snapshot().lighting.unwrap().mode,
        Some(LightMode::Rgb)
    );
}

#[test]
fn unobserved_fields_stay_unknown_and_missing_lighting_state_is_not_manufactured() {
    let (physical, _receiver) = device(&[K::OnOff]);
    let function = &physical.functions()[0];
    function
        .observe_light(LightPatch {
            power: Some(true),
            ..Default::default()
        })
        .unwrap();
    let snapshot = function.snapshot();
    assert_eq!(snapshot.power, Power::On);
    assert!(snapshot.lighting.is_none());

    let (physical, _receiver) = device(&[K::OnOff, K::Brightness, K::Temperature, K::Rgb]);
    let function = &physical.functions()[0];
    function
        .observe_light(LightPatch {
            brightness_percent: Some(42.0),
            ..Default::default()
        })
        .unwrap();
    let lighting = function.snapshot().lighting.unwrap();
    assert_eq!(lighting.brightness_percent, Some(42.0));
    assert_eq!(lighting.temperature_kelvin, None);
    assert_eq!(lighting.color, None);
    assert_eq!(lighting.mode, None);
}

#[test]
fn on_off_only_lights_keep_the_existing_power_queue_and_takeover() {
    block_on(async {
        let (physical, mut receiver) = device(&[K::OnOff]);
        let function = &physical.functions()[0];
        function.observe(false);
        physical.set_availability(Availability::Available);
        let mut command = Box::pin(function.execute(Command::On));
        assert!(poll_once(command.as_mut()).await.is_none());
        let request = receiver.next().await.unwrap();
        assert!(request.target);
        function.take_over_light_all().unwrap();
        assert!(!request.is_current());
        assert_eq!(request.mark_sent(), Err(CommandError::Superseded));
        assert_eq!(command.await, Err(CommandError::Superseded));
        request.reject(CommandError::Cancelled);
        assert!(function.light_action_diagnostics().is_none());
        let snapshot = function.snapshot();
        assert!(snapshot.lighting.is_none());
        assert_eq!(snapshot.power, Power::Off);
    });
}

#[test]
fn take_over_light_all_keeps_the_outlet_contract_and_pending_power() {
    block_on(async {
        let (physical, mut receiver) = new_device(outlet());
        let function = &physical.functions()[0];
        function.observe(false);
        physical.set_availability(Availability::Available);
        let mut command = Box::pin(function.execute(Command::On));
        assert!(poll_once(command.as_mut()).await.is_none());
        assert_eq!(
            function.take_over_light_all(),
            Err(CommandError::Unsupported)
        );
        // The queued outlet command is still owned by its original request.
        let request = receiver.next().await.unwrap();
        assert!(request.target);
        assert!(request.is_current());
        request.accept().unwrap();
        assert_eq!(command.await.unwrap().power, Power::On);
        assert_eq!(function.snapshot().power, Power::On);
    });
}

#[test]
fn cancellation_matches_present_dimensions_and_preserves_the_other_action() {
    block_on(async {
        let (physical, _receiver) = device(&[K::Rgb]);
        let function = &physical.functions()[0];
        physical.set_availability(Availability::Available);
        let action = function
            .begin_light_action(LightDimension::Color, rgb_target(), false)
            .unwrap();
        assert_eq!(
            function.stop_light(LightDimension::Brightness),
            Err(CommandError::Unsupported)
        );
        assert_eq!(
            function.take_over_light(LightDimension::Brightness),
            Err(CommandError::Unsupported)
        );
        assert!(function.light_action_current(action.token()));
        assert!(poll_once(action.ended()).await.is_none());
        function.stop_light(LightDimension::Color).unwrap();
        assert_eq!(action.ended().await, Err(CommandError::Stopped));
        assert!(!function.light_action_current(action.token()));
    });
    block_on(async {
        let (physical, _receiver) = device(&[K::Brightness]);
        let function = &physical.functions()[0];
        physical.set_availability(Availability::Available);
        let action = function
            .begin_light_action(LightDimension::Brightness, brightness_target(), false)
            .unwrap();
        assert_eq!(
            function.stop_light(LightDimension::Color),
            Err(CommandError::Unsupported)
        );
        assert_eq!(
            function.take_over_light(LightDimension::Color),
            Err(CommandError::Unsupported)
        );
        assert!(function.light_action_current(action.token()));
        assert!(poll_once(action.ended()).await.is_none());
        function.stop_light(LightDimension::Brightness).unwrap();
        assert_eq!(action.ended().await, Err(CommandError::Stopped));
        assert!(!function.light_action_current(action.token()));
    });
    block_on(async {
        let (physical, _receiver) = device(&[K::Temperature]);
        let function = &physical.functions()[0];
        physical.set_availability(Availability::Available);
        assert_eq!(
            function.stop_light(LightDimension::Brightness),
            Err(CommandError::Unsupported)
        );
        assert!(function.stop_light(LightDimension::Color).is_ok());
    });
    block_on(async {
        let (physical, _receiver) = device(&[K::OnOff]);
        let function = &physical.functions()[0];
        physical.set_availability(Availability::Available);
        assert_eq!(
            function.stop_light(LightDimension::Brightness),
            Err(CommandError::Unsupported)
        );
        assert_eq!(
            function.stop_light(LightDimension::Color),
            Err(CommandError::Unsupported)
        );
        assert_eq!(
            function.take_over_light(LightDimension::Brightness),
            Err(CommandError::Unsupported)
        );
        assert_eq!(
            function.take_over_light(LightDimension::Color),
            Err(CommandError::Unsupported)
        );
    });
}
