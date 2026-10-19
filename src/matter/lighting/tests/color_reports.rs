use super::*;
use crate::lighting::hs;

fn reported(light: &Lighting) -> Vec<u32> {
    let mut attributes: Vec<_> = collect_reports(light)
        .into_iter()
        .filter_map(|(cluster, attribute)| (cluster == 0x300).then_some(attribute))
        .collect();
    attributes.sort_unstable();
    attributes
}

fn hue_sat(light: &Lighting) -> Option<(u8, u8)> {
    light.refresh().hs.map(|value| {
        (
            value.hue.round().clamp(0.0, 254.0) as u8,
            value.saturation.round().clamp(0.0, 254.0) as u8,
        )
    })
}

fn cached_colors(mode: LightMode) -> LightPatch {
    LightPatch {
        power: Some(true),
        temperature_kelvin: Some(2700),
        color: Some(Rgb {
            red: 255,
            green: 0,
            blue: 0,
        }),
        mode: Some(mode),
        ..Default::default()
    }
}

#[test]
fn external_cold_white_reports_one_consistent_color_without_changing_the_rgb_cache() {
    let (light, function, mut receiver, clock) = light_fixture(true);
    function
        .observe_light(cached_colors(LightMode::Rgb))
        .unwrap();
    collect_reports(&light);

    function
        .observe_light(LightPatch {
            temperature_kelvin: Some(6500),
            mode: Some(LightMode::Temperature),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(reported(&light), [0, 1, 3, 4, 7, 8, 0x4001]);
    assert_eq!(light.refresh().temperature, Some(154));
    let (_, saturation) = hue_sat(&light).unwrap();
    assert!(saturation <= 10);
    let (x, y) = light.refresh().xy.unwrap();
    assert!((20000..22000).contains(&x));
    assert!((20000..22000).contains(&y));
    assert_eq!(light.refresh().color_mode, Some(2));
    assert_eq!(
        function.snapshot().lighting.unwrap().color,
        cached_colors(LightMode::Rgb).color
    );

    function
        .observe_light(LightPatch {
            temperature_kelvin: Some(4000),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(reported(&light), [0, 1, 3, 4, 7]);
    assert_eq!(light.refresh().temperature, Some(250));
    let (hue, saturation) = hue_sat(&light).unwrap();
    assert!((15..35).contains(&hue));
    assert!((70..110).contains(&saturation));
    let (x, y) = light.refresh().xy.unwrap();
    assert!((24000..26000).contains(&x));
    assert!((24000..26000).contains(&y));
    assert_eq!(clock.now(), 0);
    assert!(block_on(poll_once(receiver.next_operation())).is_none());
}

#[test]
fn mode_only_changes_report_the_new_group_even_when_temperature_encoding_is_unchanged() {
    let (light, function, mut receiver, clock) = light_fixture(true);
    function
        .observe_light(LightPatch {
            temperature_kelvin: Some(6494),
            ..cached_colors(LightMode::Rgb)
        })
        .unwrap();
    collect_reports(&light);
    assert_eq!(light.refresh().temperature, Some(154));

    function
        .observe_light(LightPatch {
            temperature_kelvin: Some(6500),
            ..Default::default()
        })
        .unwrap();
    assert!(reported(&light).is_empty());
    assert_eq!(light.refresh().temperature, Some(154));

    function
        .observe_light(LightPatch {
            mode: Some(LightMode::Temperature),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(reported(&light), [0, 1, 3, 4, 7, 8, 0x4001]);
    assert!(light.refresh().encoded_hs().unwrap().1 <= 10);

    function
        .observe_light(LightPatch {
            mode: Some(LightMode::Rgb),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(reported(&light), [0, 1, 3, 4, 8, 0x4001]);
    assert_eq!(light.refresh().encoded_hs(), Some((0, 254)));
    assert_eq!(light.refresh().xy, Some((41943, 21627)));
    assert_eq!(clock.now(), 0);
    assert!(block_on(poll_once(receiver.next_operation())).is_none());
}

#[test]
fn inactive_cache_updates_only_version_values_exposed_by_the_current_projection() {
    for mode in [LightMode::Temperature, LightMode::Rgb] {
        let (light, function, mut receiver, clock) = light_fixture(true);
        function.observe_light(cached_colors(mode)).unwrap();
        collect_reports(&light);
        let old_version = light.report.color_version.get();
        let old = light.refresh();
        let patch = match mode {
            LightMode::Temperature => LightPatch {
                color: Some(Rgb {
                    red: 0,
                    green: 0,
                    blue: 255,
                }),
                ..Default::default()
            },
            LightMode::Rgb => LightPatch {
                temperature_kelvin: Some(5000),
                ..Default::default()
            },
        };
        function.observe_light(patch).unwrap();
        light.sync_versions();
        assert!(reported(&light).is_empty());
        match mode {
            LightMode::Temperature => {
                assert_eq!(light.report.color_version.get(), old_version);
                let (hue, saturation) = old.encoded_hs().unwrap();
                let (x, y) = old.xy.unwrap();
                assert_eq!(hue_sat(&light), Some((hue, saturation)));
                assert_eq!(light.refresh().xy, Some((x, y)));
                assert_eq!(function.snapshot().lighting.unwrap().color, patch.color);
            }
            LightMode::Rgb => {
                assert_ne!(light.report.color_version.get(), old_version);
                assert_eq!(light.refresh().temperature, Some(200));
                assert_eq!(light.refresh().encoded_hs(), old.encoded_hs());
                assert_eq!(light.refresh().xy, old.xy);
            }
        }
        clock.set(1000);
        assert!(reported(&light).is_empty());
        assert!(block_on(poll_once(receiver.next_operation())).is_none());
    }
}

#[test]
fn unknown_active_color_reports_its_own_group_without_using_the_other_cache() {
    for mode in [LightMode::Temperature, LightMode::Rgb] {
        let (light, function, mut receiver, clock) = light_fixture(true);
        let patch = match mode {
            LightMode::Temperature => LightPatch {
                color: Some(Rgb {
                    red: 255,
                    green: 0,
                    blue: 0,
                }),
                ..Default::default()
            },
            LightMode::Rgb => LightPatch {
                temperature_kelvin: Some(6500),
                ..Default::default()
            },
        };
        function.observe_light(patch).unwrap();
        collect_reports(&light);
        assert!(reported(&light).is_empty());
        assert!(light.refresh().hs.is_none());
        assert!(light.refresh().xy.is_none());
        function
            .observe_light(LightPatch {
                mode: Some(mode),
                ..Default::default()
            })
            .unwrap();
        match mode {
            LightMode::Temperature => {
                assert_eq!(reported(&light), [0, 1, 3, 4, 7, 8, 0x4001]);
                assert!(hue_sat(&light).is_none());
                assert!(light.refresh().xy.is_none());
                assert!(light.refresh().temperature.is_none());
            }
            LightMode::Rgb => {
                assert_eq!(reported(&light), [0, 1, 3, 4, 8, 0x4001]);
                assert!(hue_sat(&light).is_none());
                assert!(light.refresh().xy.is_none());
                assert_eq!(light.refresh().temperature, Some(154));
            }
        }
        assert_eq!(clock.now(), 0);
        assert!(block_on(poll_once(receiver.next_operation())).is_none());
    }
}

#[test]
fn mode_boundary_flushes_the_new_group_and_discards_the_old_quiet_report() {
    for rgb in [false, true] {
        let (light, function, mut receiver, clock) = light_fixture(true);
        function
            .observe_light(LightPatch {
                temperature_kelvin: Some(5000),
                ..cached_colors(if rgb {
                    LightMode::Rgb
                } else {
                    LightMode::Temperature
                })
            })
            .unwrap();
        collect_reports(&light);
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
        let action = light.begin(LightingTask::Color(task), None).unwrap();
        block_on(async {
            accept(&function, frame(&mut receiver).await);
            action.accepted().await.unwrap();
            collect_reports(&light);
            clock.set(100);
            light.advance();
            accept(&function, frame(&mut receiver).await);
            assert!(reported(&light).is_empty());
            assert!(light.report.is_pending());

            function.take_over_light(LightDimension::Color).unwrap();
            function
                .observe_light(LightPatch {
                    mode: Some(if rgb {
                        LightMode::Temperature
                    } else {
                        LightMode::Rgb
                    }),
                    ..Default::default()
                })
                .unwrap();
            assert_eq!(
                reported(&light),
                if rgb {
                    vec![0, 1, 3, 4, 7, 8, 0x4001]
                } else {
                    vec![0, 1, 3, 4, 8, 0x4001]
                }
            );
            assert!(!light.report.is_pending());
            clock.set(1000);
            assert!(reported(&light).is_empty());
        });
    }
}

#[test]
fn short_color_transitions_report_the_active_group_at_the_end() {
    for rgb in [false, true] {
        let (light, function, mut receiver, clock) = light_fixture(true);
        function
            .observe_light(LightPatch {
                temperature_kelvin: Some(5000),
                ..cached_colors(if rgb {
                    LightMode::Rgb
                } else {
                    LightMode::Temperature
                })
            })
            .unwrap();
        collect_reports(&light);
        let task = if rgb {
            ColorTask::XyMoveTo {
                start_x: 41943,
                start_y: 21627,
                target_x: 19661,
                target_y: 39322,
                transition_ms: 100,
            }
        } else {
            ColorTask::ColorTemperatureMoveTo {
                start: 200,
                target_mireds: 300,
                transition_ms: 100,
            }
        };
        let action = light.begin(LightingTask::Color(task), None).unwrap();
        block_on(async {
            accept(&function, frame(&mut receiver).await);
            action.accepted().await.unwrap();
            collect_reports(&light);
            clock.set(50);
            light.advance();
            accept(&function, frame(&mut receiver).await);
            assert!(reported(&light).is_empty());

            clock.set(100);
            light.advance();
            let request = frame(&mut receiver).await;
            assert!(request.final_frame);
            accept(&function, request);
            assert_eq!(
                reported(&light),
                if rgb {
                    vec![0, 1, 3, 4]
                } else {
                    vec![0, 1, 3, 4, 7]
                }
            );
            action.ended().await.unwrap();
        });
    }
}

#[test]
fn gray_rgb_hue_selection_survives_a_temperature_mode_round_trip() {
    let (light, function, mut receiver, clock) = light_fixture(true);
    function
        .observe_light(LightPatch {
            temperature_kelvin: Some(6500),
            ..cached_colors(LightMode::Rgb)
        })
        .unwrap();
    let action = light
        .begin(
            LightingTask::Color(ColorTask::Hs(hs::HsTask::stationary(hs::Hs {
                hue: 170.0,
                saturation: 0.0,
            }))),
            None,
        )
        .unwrap();
    block_on(async {
        accept(&function, frame(&mut receiver).await);
        action.accepted().await.unwrap();
        collect_reports(&light);
        assert_eq!(hue_sat(&light), Some((170, 0)));
        light.advance();
        assert!(light.scheduler.observe(LightDimension::Color).is_none());
        let rgb_cache = function.snapshot().lighting.unwrap().color;

        function
            .observe_light(LightPatch {
                mode: Some(LightMode::Temperature),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(reported(&light), [0, 1, 3, 4, 7, 8, 0x4001]);
        let (hue, saturation) = hue_sat(&light).unwrap();
        assert_ne!(hue, 170);
        assert!(saturation <= 10);
        assert_eq!(function.snapshot().lighting.unwrap().color, rgb_cache);

        function
            .observe_light(LightPatch {
                mode: Some(LightMode::Rgb),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(reported(&light), [0, 1, 3, 4, 8, 0x4001]);
        assert_eq!(hue_sat(&light), Some((170, 0)));
        assert_eq!(function.snapshot().lighting.unwrap().color, rgb_cache);
    });
    assert_eq!(clock.now(), 0);
}
