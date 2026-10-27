use super::*;

#[test]
fn observed_zero_is_known_minimum_without_changing_power_and_relative_start() {
    use l::ClusterAsyncHandler as _;
    setup!(false, function, receiver, light, clock, ctx);
    function
        .observe_light(LightPatch {
            power: Some(true),
            brightness_percent: Some(0.0),
            ..Default::default()
        })
        .unwrap();
    block_on(async {
        assert_eq!(
            LevelHandler(&light)
                .current_level(&ctx)
                .await
                .unwrap()
                .into_option(),
            Some(1)
        );
        assert_eq!(
            function
                .snapshot()
                .power()
                .and_then(crate::device::Power::value),
            Some(true)
        );
        let input = light.plan_input();
        let lighting_plan::Plan::Execute {
            task: LightingTask::Level(task),
        } = lighting_plan::move_level(
            false,
            Direction::Up,
            Some(1),
            lighting_plan::Options::new(0, 0),
            &input,
        )
        .unwrap()
        else {
            panic!("expected a level execution")
        };
        assert_eq!(task.frame(0).0, 1);
        let action = light.begin(LightingTask::Level(task), None).unwrap();
        let request = frame(&mut receiver).await;
        assert_eq!(request.target.brightness_percent(), Some(1.0));
        accept(&function, request);
        action.accepted().await.unwrap();
        clock.set(1000);
        light.advance();
        let request = frame(&mut receiver).await;
        assert!(request.target.brightness_percent().unwrap() > 1.0);
    });
}

#[test]
fn color_quiet_reports_ignore_unrelated_unknown_caches_and_remaining_time_changes_version() {
    for rgb in [false, true] {
        let (light, function, mut receiver, clock) = light_fixture(rgb);
        let patch = if rgb {
            LightPatch {
                color: Some(Rgb {
                    red: 255,
                    green: 0,
                    blue: 0,
                }),
                mode: Some(LightMode::Rgb),
                ..Default::default()
            }
        } else {
            LightPatch {
                temperature_kelvin: Some(5000),
                mode: Some(LightMode::Temperature),
                ..Default::default()
            }
        };
        function
            .observe_light(LightPatch {
                power: Some(true),
                ..patch
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
            let mut reported = Vec::new();
            for now in [100, 200, 300, 900] {
                clock.set(now);
                light.advance();
                accept(&function, frame(&mut receiver).await);
                reported.extend(collect_reports(&light));
            }
            assert_eq!(
                reported
                    .iter()
                    .filter(|(cluster, attribute)| {
                        *cluster == 0x300 && matches!(*attribute, 0 | 1 | 3 | 4 | 7)
                    })
                    .count(),
                0
            );
            clock.set(1000);
            light.advance();
            accept(&function, frame(&mut receiver).await);
            let mut attributes: Vec<_> = collect_reports(&light)
                .into_iter()
                .filter_map(|(cluster, attribute)| {
                    (cluster == 0x300 && matches!(attribute, 0 | 1 | 3 | 4 | 7))
                        .then_some(attribute)
                })
                .collect();
            attributes.sort_unstable();
            assert_eq!(attributes, if rgb { vec![0, 1, 3, 4] } else { vec![7] });
        });
        // A long CT trajectory whose first tick still rounds to the current
        // value advances RemainingTime's DataVersion without a quiet report.
        let action = light
            .begin(
                LightingTask::Color(ColorTask::ColorTemperatureMoveTo {
                    start: 200,
                    target_mireds: 201,
                    transition_ms: 10000,
                }),
                None,
            )
            .unwrap();
        block_on(async {
            accept(&function, frame(&mut receiver).await);
            action.accepted().await.unwrap();
            collect_reports(&light);
            let old = light.report.color_version.get();
            assert_eq!(light.remaining_time(LightDimension::Color), 100);

            clock.set(1100);
            light.advance();
            assert!(poll_once(receiver.next_operation()).await.is_none());
            assert_eq!(light.remaining_time(LightDimension::Color), 99);
            light.sync_versions();
            assert_ne!(light.report.color_version.get(), old);
            assert!(collect_reports(&light).is_empty());
        });
    }
}

#[test]
fn version_reads_preserve_quiet_final_and_unknown_reports() {
    for dimension in [LightDimension::Brightness, LightDimension::Color] {
        let (light, function, mut receiver, clock) = light_fixture(false);
        function
            .observe_light(LightPatch {
                power: Some(true),
                brightness_percent: Some(1.0),
                temperature_kelvin: Some(5000),
                ..Default::default()
            })
            .unwrap();
        collect_reports(&light);
        let task = if dimension == LightDimension::Brightness {
            LightingTask::Level(Task::MoveToLevel {
                with_on_off: false,
                start: 1,
                target: 254,
                transition_ms: 1500,
            })
        } else {
            LightingTask::Color(ColorTask::ColorTemperatureMoveTo {
                start: 200,
                target_mireds: 300,
                transition_ms: 1500,
            })
        };
        let action = light.begin(task, None).unwrap();
        block_on(async {
            accept(&function, frame(&mut receiver).await);
            action.accepted().await.unwrap();
            collect_reports(&light);
            let (cluster, attribute) = if dimension == LightDimension::Brightness {
                (8, 0)
            } else {
                (0x300, 7)
            };
            for now in [100, 900, 1000, 1100, 1500] {
                clock.set(now);
                light.advance();
                accept(&function, frame(&mut receiver).await);
                light.sync_versions();
                let changes = collect_reports(&light);
                assert_eq!(
                    changes.contains(&(cluster, attribute)),
                    matches!(now, 1000 | 1500)
                );
            }
            action.ended().await.unwrap();
        });
    }

    let (light, function, mut receiver, _clock) = light_fixture(true);
    function
        .observe_light(LightPatch {
            color: Some(Rgb {
                red: 255,
                green: 0,
                blue: 0,
            }),
            mode: Some(LightMode::Rgb),
            ..Default::default()
        })
        .unwrap();
    collect_reports(&light);
    function
        .observe_light(LightPatch {
            color: Some(Rgb {
                red: 0,
                green: 0,
                blue: 0,
            }),
            ..Default::default()
        })
        .unwrap();
    light.sync_versions();
    let changes = collect_reports(&light);
    assert!(changes.contains(&(0x300, 3)));
    assert!(changes.contains(&(0x300, 4)));
    assert!(block_on(poll_once(receiver.next_operation())).is_none());
}
