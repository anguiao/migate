use super::*;

async fn next_shared_read(c: &Control, before: usize) {
    for _ in 0..4 {
        c.clock.advance(Duration::from_secs(16));
        let done = future::or(
            async {
                async_io::Timer::after(Duration::from_millis(300)).await;
                false
            },
            async {
                until(|| c.cloud.lock().unwrap().rpc_reads.len() > before).await;
                true
            },
        )
        .await;
        if done {
            return;
        }
    }
    panic!("shared feedback read did not complete");
}

async fn next_shared_step(c: &Control, before: usize, elapsed: &mut u64) -> u64 {
    for _ in 0..20 {
        c.clock.advance(Duration::from_secs(1));
        *elapsed += 1;
        async_io::Timer::after(Duration::from_millis(80)).await;
        if c.cloud.lock().unwrap().rpc_reads.len() > before {
            return *elapsed;
        }
    }
    panic!("shared feedback poll did not occur within 20 seconds");
}

#[test]
fn cloud_shared_group_is_read_once_and_feeds_both_fans_and_heater_temperature() {
    let (rig, listener, cloud) = rig();
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            let blow = function(&c, BLOW);
            let ventilation = function(&c, VENTILATION);
            let heater = function(&c, HEATER);
            eventually(|| {
                blow.snapshot()
                    .fan()
                    .is_some_and(|fan| fan.power.value == Some(true) && fan.speed.value == Some(2))
                    && ventilation.snapshot().fan().is_some_and(|fan| {
                        fan.power.value == Some(true) && fan.speed.value == Some(1)
                    })
                    && heater.snapshot().heater().is_some_and(|heater| {
                        heater.current_temperature.value == Some(26.5)
                            && matches!(
                                heater.temperature_source,
                                TemperatureSourceState::Available(_)
                            )
                    })
            })
            .await;

            let reads = c.cloud.lock().unwrap().rpc_reads.clone();
            assert_eq!(reads.len(), 1, "{reads:?}");
            assert_eq!(reads[0].0, DID);
            assert_eq!(reads[0].1, "get_prop");
            assert_eq!(
                reads[0].2,
                json!(["bh_mode", "fan_speed_idx", "temperature"])
            );
        })
        .await;
    }));
}

#[test]
fn shared_feedback_keeps_last_values_on_communication_failure_and_isolates_invalid_fields() {
    let (rig, listener, cloud) = rig();
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            let blow = function(&c, BLOW);
            let ventilation = function(&c, VENTILATION);
            let heater = function(&c, HEATER);
            eventually(|| {
                blow.snapshot()
                    .fan()
                    .is_some_and(|fan| fan.speed.value == Some(2))
                    && ventilation
                        .snapshot()
                        .fan()
                        .is_some_and(|fan| fan.speed.value == Some(1))
                    && heater
                        .snapshot()
                        .heater()
                        .is_some_and(|heater| heater.current_temperature.value == Some(26.5))
            })
            .await;
            let before = c.cloud.lock().unwrap().rpc_reads.len();

            {
                let mut state = c.cloud.lock().unwrap();
                state.rpc_status = 503;
            }
            next_shared_read(&c, before).await;
            assert_eq!(
                blow.snapshot().fan().unwrap().speed.value,
                Some(2),
                "a communication failure must retain the last valid speed"
            );
            assert_eq!(
                heater
                    .snapshot()
                    .heater()
                    .unwrap()
                    .current_temperature
                    .value,
                Some(26.5),
                "a communication failure must retain the last valid temperature"
            );

            let before = c.cloud.lock().unwrap().rpc_reads.len();
            {
                let mut state = c.cloud.lock().unwrap();
                state.rpc_status = 200;
                state
                    .rpc_results
                    .insert("get_prop".into(), json!(["heat", 32, "not-a-temperature"]));
            }
            next_shared_read(&c, before).await;
            eventually(|| {
                blow.snapshot()
                    .fan()
                    .is_some_and(|fan| fan.speed.value == Some(2))
                    && ventilation
                        .snapshot()
                        .fan()
                        .is_some_and(|fan| fan.speed.value.is_none())
                    && heater.snapshot().heater().is_some_and(|heater| {
                        heater.current_temperature.value.is_none()
                            && heater.temperature_source == TemperatureSourceState::AwaitingReading
                    })
            })
            .await;

            let before = c.cloud.lock().unwrap().rpc_reads.len();
            c.cloud
                .lock()
                .unwrap()
                .rpc_results
                .insert("get_prop".into(), json!(["dry", 13, 27.25]));
            next_shared_read(&c, before).await;
            eventually(|| {
                blow.snapshot()
                    .fan()
                    .is_some_and(|fan| fan.speed.value == Some(1))
                    && ventilation
                        .snapshot()
                        .fan()
                        .is_some_and(|fan| fan.speed.value == Some(2))
                    && heater.snapshot().heater().is_some_and(|heater| {
                        heater.current_temperature.value == Some(27.25)
                            && matches!(
                                heater.temperature_source,
                                TemperatureSourceState::Available(_)
                            )
                    })
            })
            .await;
        })
        .await;
    }));
}

#[test]
fn same_value_heater_and_native_mode_notifications_refresh_related_inputs() {
    let (rig, listener, cloud) = rig();
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            c.announce();
            let control = &c;
            c.peer(standard_peer(), |peer| async move {
                let heater = function(control, HEATER);
                eventually(|| {
                    work(control).function_observer(HEATER).is_some()
                        && heater
                            .snapshot()
                            .heater()
                            .is_some_and(|heater| heater.power.value == Some(false))
                        && !control.cloud.lock().unwrap().rpc_reads.is_empty()
                })
                .await;
                eventually(|| {
                    let device = work(control);
                    device.refresh.borrow().snapshot().inflight.is_empty()
                })
                .await;

                let reads = control.cloud.lock().unwrap().rpc_reads.len();
                // Value is identical to the already observed false value.
                peer.notify_value_at(DID, HEATING, json!(false), Some(810), false);
                eventually(|| control.cloud.lock().unwrap().rpc_reads.len() > reads).await;

                let reads = control.cloud.lock().unwrap().rpc_reads.len();
                // 3.1 is its own diagnostic input; even an unchanged mode value
                // triggers the merged related read.
                peer.notify_value_at(DID, MODE, json!(0), Some(811), false);
                eventually(|| control.cloud.lock().unwrap().rpc_reads.len() > reads).await;
                assert_eq!(heater.snapshot().heater().unwrap().power.value, Some(false));
            })
            .await;
        })
        .await;
    }));
}

#[test]
fn a_pending_readback_keeps_the_last_valid_speed_time_and_drops_the_stale_group_result() {
    let (rig, listener, cloud) = rig();
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            c.announce();
            let control = &c;
            c.peer(standard_peer(), |peer| async move {
                let blow = function(control, BLOW);
                let ventilation = function(control, VENTILATION);
                eventually(|| {
                    work(control).function_observer(HEATER).is_some()
                        && blow
                            .snapshot()
                            .fan()
                            .is_some_and(|fan| fan.speed.value == Some(2))
                        && ventilation
                            .snapshot()
                            .fan()
                            .is_some_and(|fan| fan.speed.value == Some(1))
                })
                .await;
                let before = blow.snapshot().fan().unwrap().speed;
                assert!(before.received_at.is_some());

                let (release, gate) = flume::bounded(1);
                control.cloud.lock().unwrap().rpc_gate = Some(gate);
                control
                    .cloud
                    .lock()
                    .unwrap()
                    .rpc_results
                    .insert("get_prop".into(), json!(["heat", 32, 26.5]));
                let reads = control.cloud.lock().unwrap().rpc_reads.len();
                control.clock.advance(Duration::from_secs(16));
                eventually(|| control.cloud.lock().unwrap().rpc_reads.len() > reads).await;

                // A new notification makes the already in-flight group read
                // stale, but cannot erase the last confirmed speed or its time.
                peer.notify_value_at(DID, HEATING, json!(false), Some(900), false);
                eventually(|| work(control).shared_feedback().pending).await;
                assert_eq!(blow.snapshot().fan().unwrap().speed, before);
                assert_eq!(ventilation.snapshot().fan().unwrap().speed.value, Some(1));

                control
                    .cloud
                    .lock()
                    .unwrap()
                    .rpc_results
                    .insert("get_prop".into(), json!(["dry", 13, 27.0]));
                let (release_fresh, fresh_gate) = flume::bounded(1);
                control.cloud.lock().unwrap().rpc_gate = Some(fresh_gate);
                release.send(()).unwrap();
                async_io::Timer::after(Duration::from_millis(100)).await;
                // The stale 32 (invalid ventilation) must not replace the
                // confirmed low level before the replacement read arrives.
                assert_eq!(ventilation.snapshot().fan().unwrap().speed.value, Some(1));

                control.cloud.lock().unwrap().rpc_gate = None;
                release_fresh.send(()).unwrap();
                control.clock.advance(Duration::from_secs(16));
                eventually(|| {
                    blow.snapshot()
                        .fan()
                        .is_some_and(|fan| fan.speed.value == Some(1))
                        && ventilation
                            .snapshot()
                            .fan()
                            .is_some_and(|fan| fan.speed.value == Some(2))
                })
                .await;
            })
            .await;
        })
        .await;
    }));
}

#[test]
fn standard_31_mode_and_legacy_bh_mode_stay_separate_diagnostic_inputs() {
    let (rig, listener, cloud) = rig();
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            c.announce();
            let control = &c;
            c.peer(standard_peer(), |peer| async move {
                let blow = function(control, BLOW);
                let heater = function(control, HEATER);
                eventually(|| {
                    work(control).function_observer(HEATER).is_some()
                        && work(control)
                            .standard_mode_value()
                            .is_some_and(|value| value.value == json!(0))
                        && work(control)
                            .shared_feedback()
                            .native_mode
                            .as_ref()
                            .is_some_and(|value| value.value == json!("heat"))
                        && blow
                            .snapshot()
                            .fan()
                            .is_some_and(|fan| fan.power.value == Some(true))
                        && heater
                            .snapshot()
                            .heater()
                            .is_some_and(|heater| heater.power.value == Some(false))
                })
                .await;

                peer.notify_value_at(DID, MODE, json!(4), Some(920), false);
                eventually(|| {
                    work(control).standard_mode_value().is_some_and(|value| {
                        value.value == json!(4) && value.source_time == Some(920)
                    })
                })
                .await;
                assert_eq!(
                    work(control)
                        .shared_feedback()
                        .native_mode
                        .as_ref()
                        .unwrap()
                        .value,
                    json!("heat")
                );
                assert_eq!(blow.snapshot().fan().unwrap().power.value, Some(true));
                assert_eq!(heater.snapshot().heater().unwrap().power.value, Some(false));

                let reads = control.cloud.lock().unwrap().rpc_reads.len();
                control
                    .cloud
                    .lock()
                    .unwrap()
                    .rpc_results
                    .insert("get_prop".into(), json!(["quick-heat", 31, 26.5]));
                control.clock.advance(Duration::from_secs(16));
                eventually(|| control.cloud.lock().unwrap().rpc_reads.len() > reads).await;
                eventually(|| {
                    work(control)
                        .shared_feedback()
                        .native_mode
                        .as_ref()
                        .is_some_and(|value| value.value == json!("quick-heat"))
                })
                .await;
                assert!(
                    work(control)
                        .standard_mode_value()
                        .is_some_and(|value| value.value == json!(4)),
                    "legacy bh_mode must not overwrite the standard 3.1 mode"
                );
                assert_eq!(blow.snapshot().fan().unwrap().power.value, Some(true));
                assert_eq!(heater.snapshot().heater().unwrap().power.value, Some(false));
            })
            .await;
        })
        .await;
    }));
}

#[test]
fn built_in_temperature_drops_from_matter_only_when_all_group_paths_are_unavailable() {
    let (rig, listener, cloud) = rig();
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            let heater = function(&c, HEATER);
            eventually(|| {
                heater.snapshot().heater().is_some_and(|heater| {
                    heater.current_temperature.value == Some(26.5)
                        && matches!(
                            heater.temperature_source,
                            TemperatureSourceState::Available(_)
                        )
                })
            })
            .await;

            let device = work(&c);
            c.context.credentials.borrow_mut().pause_http();
            device.reconcile_state_reads(&c.context.device_routes(&device.key.id));
            eventually(|| {
                heater.snapshot().heater().unwrap().temperature_source
                    == TemperatureSourceState::Unavailable
            })
            .await;
            let stale = heater.snapshot().heater().unwrap();
            assert_eq!(stale.current_temperature.value, Some(26.5));
            assert_eq!(
                stale.temperature_source,
                TemperatureSourceState::Unavailable
            );

            c.context.credentials.borrow_mut().resume_http();
            device.reconcile_state_reads(&c.context.device_routes(&device.key.id));
            eventually(|| {
                heater.snapshot().heater().unwrap().temperature_source
                    == TemperatureSourceState::AwaitingReading
            })
            .await;
            assert_eq!(
                heater
                    .snapshot()
                    .heater()
                    .unwrap()
                    .current_temperature
                    .value,
                Some(26.5),
                "the old reading remains diagnostic only until a new one arrives"
            );

            c.cloud
                .lock()
                .unwrap()
                .rpc_results
                .insert("get_prop".into(), json!(["heat", 31, 28.0]));
            c.clock.advance(Duration::from_secs(16));
            eventually(|| {
                heater.snapshot().heater().is_some_and(|heater| {
                    heater.current_temperature.value == Some(28.0)
                        && matches!(
                            heater.temperature_source,
                            TemperatureSourceState::Available(_)
                        )
                })
            })
            .await;
        })
        .await;
    }));
}

#[test]
fn all_v13_inputs_join_the_existing_sixteen_second_poll_without_a_new_loop() {
    let (rig, listener, cloud) = rig();
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            eventually(|| {
                !c.cloud.lock().unwrap().rpc_reads.is_empty()
                    && function(&c, HEATER)
                        .snapshot()
                        .heater()
                        .is_some_and(|heater| heater.power.value == Some(false))
            })
            .await;
            quiesce(&c).await;
            flush_all_polls(&c).await;

            let mut elapsed = 0_u64;
            let first = c.cloud.lock().unwrap().rpc_reads.len();
            let first_at = next_shared_step(&c, first, &mut elapsed).await;
            let second = c.cloud.lock().unwrap().rpc_reads.len();
            let batches = c.cloud.lock().unwrap().read_batches.len();
            let second_at = next_shared_step(&c, second, &mut elapsed).await;
            assert_eq!(
                second_at - first_at,
                16,
                "v13 inputs must use the existing 16-second interval"
            );
            let tail = c.cloud.lock().unwrap().read_batches[batches..].to_vec();
            for binding in [
                PropertyBinding { siid: 2, piid: 1 },
                PropertyBinding { siid: 2, piid: 3 },
                HEATING,
                TARGET,
                BLOW_POWER,
                VENTILATION_POWER,
                MODE,
            ] {
                assert!(
                    tail.iter().any(|batch| batch
                        .iter()
                        .any(|read| { read.1 == binding.siid && read.2 == binding.piid })),
                    "missing 16-second poll for {binding:?}: {tail:?}"
                );
            }
        })
        .await;
    }));
}

#[test]
fn diagnostics_keep_31_mode_legacy_fields_and_per_fan_unknown_reasons_separate() {
    let (rig, listener, cloud) = rig();
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            c.announce();
            let control = &c;
            c.peer(standard_peer(), |_peer| async move {
                eventually(|| {
                    work(control).function_observer(HEATER).is_some()
                        && work(control)
                            .standard_mode_value()
                            .is_some_and(|value| value.value == json!(0))
                })
                .await;
                let reads = control.cloud.lock().unwrap().rpc_reads.len();
                control
                    .cloud
                    .lock()
                    .unwrap()
                    .rpc_results
                    .insert("get_prop".into(), json!(["heat", 32, "bad-temperature"]));
                control.clock.advance(Duration::from_secs(16));
                eventually(|| control.cloud.lock().unwrap().rpc_reads.len() > reads).await;
                eventually(|| {
                    let status = control.context.diagnostics();
                    let device = status
                        .devices
                        .iter()
                        .find(|device| device.did == DID)
                        .unwrap();
                    device.shared_feedback.as_ref().is_some_and(|shared| {
                        shared.fans.iter().any(|fan| {
                            fan.function == BLOW && fan.level == Some(2) && fan.reason.is_none()
                        }) && shared.fans.iter().any(|fan| {
                            fan.function == VENTILATION
                                && fan.level.is_none()
                                && fan
                                    .reason
                                    .as_deref()
                                    .is_some_and(|reason| reason.contains('2'))
                        }) && shared.temperature.as_ref().is_some_and(|temperature| {
                            temperature.value == json!("bad-temperature")
                                && temperature.reason.is_some()
                        })
                    })
                })
                .await;

                let status = control.context.diagnostics();
                let device = status
                    .devices
                    .iter()
                    .find(|device| device.did == DID)
                    .unwrap();
                let mode = device.standard_mode.as_ref().unwrap();
                assert_eq!(mode.value, json!(0));
                assert_eq!(mode.source, Some(PathKind::Local));
                let shared = device.shared_feedback.as_ref().unwrap();
                assert_eq!(shared.source, Some(PathKind::Cloud));
                assert_eq!(shared.native_mode.as_ref().unwrap().value, json!("heat"));
                assert_eq!(shared.fan_speed_index.as_ref().unwrap().value, json!(32));
                assert!(!shared.pending);
                let heater = device
                    .functions
                    .iter()
                    .find(|function| function.function == HEATER)
                    .unwrap();
                assert!(
                    heater
                        .properties
                        .iter()
                        .any(|property| property.name == "Power")
                );
                assert!(
                    heater
                        .properties
                        .iter()
                        .any(|property| property.name == "Temperature")
                );
            })
            .await;
        })
        .await;
    }));
}

#[test]
fn external_heat_level_notifications_read_the_actual_heater_power() {
    let (rig, listener, cloud) = rig();
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            c.announce();
            let control = &c;
            c.peer(standard_peer(), |peer| async move {
                let heater = function(control, HEATER);
                eventually(|| {
                    work(control)
                        .function_observer(HEATER)
                        .is_some_and(|source| source.kind() == PathKind::Local)
                        && heater.snapshot().heater().unwrap().power.value == Some(false)
                })
                .await;
                quiesce(control).await;
                flush_all_polls(control).await;

                // The notification is a hint. Only the heating property supplies Home's state.
                for (level, heating) in [(1, true), (2, false), (2, true)] {
                    let seen = peer.state.borrow().seen.len();
                    peer.value(DID, HEATING, json!(heating));
                    peer.notify_value(DID, HEAT_LEVEL, json!(level), false);
                    for _ in 0..5 {
                        control.clock.advance(Duration::from_secs(1));
                        async_io::Timer::after(Duration::from_millis(100)).await;
                        if heater.snapshot().heater().unwrap().power.value == Some(heating) {
                            break;
                        }
                    }
                    assert_eq!(
                        heater.snapshot().heater().unwrap().power.value,
                        Some(heating)
                    );
                    assert_eq!(hub_reads(&peer, seen), [related_read(HEATING)]);
                }
            })
            .await;
        })
        .await;
    }));
}

#[test]
fn only_an_accepted_valid_new_notification_starts_related_reads() {
    let (rig, listener, cloud) = rig();
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            c.announce();
            let control = &c;
            c.peer(standard_peer(), |peer| async move {
                eventually(|| {
                    work(control).function_observer(HEATER).is_some()
                        && work(control).function_observer(BLOW).is_some()
                        && !control.cloud.lock().unwrap().rpc_reads.is_empty()
                })
                .await;
                quiesce(control).await;
                flush_all_polls(control).await;
                assert!(!has_shared_demand(control));

                let source = work(control).function_observer(HEATER).unwrap();
                control.context.property_value_event(
                    &source,
                    DID,
                    HEATING,
                    &json!(false),
                    Some(950),
                    true,
                );
                async_io::Timer::after(Duration::from_millis(100)).await;
                assert!(
                    !has_shared_demand(control),
                    "a cached report must not trigger related reads"
                );

                for raw in [json!("bad"), json!(255)] {
                    control.context.property_value_event(
                        &source,
                        DID,
                        MODE,
                        &raw,
                        Some(952),
                        false,
                    );
                    let observed = work(control).standard_mode_value().unwrap();
                    assert_eq!(observed.value, raw);
                    assert_eq!(observed.source_time, Some(952));
                    assert!(observed.reason.unwrap().contains("unrecognized"));
                    assert!(!has_shared_demand(control));

                    control.context.property_value_event(
                        &source,
                        DID,
                        HEAT_LEVEL,
                        &raw,
                        Some(952),
                        false,
                    );
                    assert!(!work(control).refresh.borrow().contains(HEATER, HEATING));
                }

                peer.notify_value_at(DID, MODE, json!(4), Some(953), false);
                eventually(|| has_shared_demand(control)).await;
                let observed = work(control).standard_mode_value().unwrap();
                assert_eq!(observed.value, json!(4));
                assert!(observed.reason.is_none());
            })
            .await;
        })
        .await;
    }));
}

#[test]
fn a_stale_shared_read_failure_cannot_consume_the_new_notification_demand() {
    let (rig, listener, cloud) = rig();
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            c.announce();
            let control = &c;
            c.peer(standard_peer(), |peer| async move {
                let blow = function(control, BLOW);
                let ventilation = function(control, VENTILATION);
                eventually(|| {
                    work(control).function_observer(HEATER).is_some()
                        && blow
                            .snapshot()
                            .fan()
                            .is_some_and(|fan| fan.speed.value == Some(2))
                        && ventilation
                            .snapshot()
                            .fan()
                            .is_some_and(|fan| fan.speed.value == Some(1))
                })
                .await;
                quiesce(control).await;
                flush_all_polls(control).await;

                let (release, gate) = flume::bounded(1);
                {
                    let mut state = control.cloud.lock().unwrap();
                    state.rpc_gate = Some(gate);
                    state.rpc_status = 503;
                }
                let reads = control.cloud.lock().unwrap().rpc_reads.len();
                control.clock.advance(Duration::from_secs(16));
                eventually(|| control.cloud.lock().unwrap().rpc_reads.len() > reads).await;

                peer.notify_value_at(DID, HEATING, json!(false), Some(960), false);
                eventually(|| has_shared_demand(control)).await;

                {
                    let mut state = control.cloud.lock().unwrap();
                    state.rpc_status = 200;
                    state
                        .rpc_results
                        .insert("get_prop".into(), json!(["dry", 13, 27.0]));
                    state.rpc_gate = None;
                }
                release.send(()).unwrap();
                async_io::Timer::after(Duration::from_millis(100)).await;
                assert!(
                    work(control)
                        .refresh
                        .borrow()
                        .input_problem(
                            crate::xiaomi::capability::SHARED_FEEDBACK_CHANNEL,
                            crate::xiaomi::capability::ReadInput::SharedFeedback,
                        )
                        .is_none(),
                    "a stale shared failure must not record a problem for the newer demand"
                );

                control.clock.advance(Duration::from_secs(16));
                eventually(|| {
                    blow.snapshot()
                        .fan()
                        .is_some_and(|fan| fan.speed.value == Some(1))
                        && ventilation
                            .snapshot()
                            .fan()
                            .is_some_and(|fan| fan.speed.value == Some(2))
                })
                .await;
            })
            .await;
        })
        .await;
    }));
}

#[test]
fn a_retired_mijia_session_cannot_apply_a_late_shared_result() {
    let (rig, listener, cloud) = rig();
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            let blow = function(&c, BLOW);
            let ventilation = function(&c, VENTILATION);
            eventually(|| {
                blow.snapshot()
                    .fan()
                    .is_some_and(|fan| fan.speed.value == Some(2))
                    && ventilation
                        .snapshot()
                        .fan()
                        .is_some_and(|fan| fan.speed.value == Some(1))
            })
            .await;
            quiesce(&c).await;
            flush_all_polls(&c).await;

            let (release, gate) = flume::bounded(1);
            c.cloud.lock().unwrap().rpc_gate = Some(gate);
            let reads = c.cloud.lock().unwrap().rpc_reads.len();
            c.clock.advance(Duration::from_secs(16));
            eventually(|| c.cloud.lock().unwrap().rpc_reads.len() > reads).await;

            let record = c.store.xiaomi().load().unwrap().unwrap();
            c.store
                .xiaomi()
                .update_recovery(&record.recovery.credentials, &record.mijia)
                .unwrap();
            c.context.adopt_credentials(record);
            c.cloud
                .lock()
                .unwrap()
                .rpc_results
                .insert("get_prop".into(), json!(["dry", 13, 27.0]));
            c.cloud.lock().unwrap().rpc_gate = None;
            release.send(()).unwrap();
            async_io::Timer::after(Duration::from_millis(150)).await;
            assert_eq!(blow.snapshot().fan().unwrap().speed.value, Some(2));
            assert_eq!(ventilation.snapshot().fan().unwrap().speed.value, Some(1));
        })
        .await;
    }));
}

#[test]
fn a_retired_notification_source_cannot_trigger_related_reads() {
    let (rig, listener, cloud) = rig();
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            c.announce();
            let control = &c;
            c.peer(standard_peer(), |_peer| async move {
                eventually(|| {
                    work(control).function_observer(HEATER).is_some()
                        && !control.cloud.lock().unwrap().rpc_reads.is_empty()
                })
                .await;
                quiesce(control).await;
                flush_all_polls(control).await;
                assert!(!has_shared_demand(control));
                let source = work(control).function_observer(HEATER).unwrap();
                let crate::xiaomi::adapter::notifications::NotificationSource::Local(handle) =
                    &source
                else {
                    panic!("test expects a hub notification source");
                };
                assert!(work(control).detach_local(handle));
                control.context.property_value_event(
                    &source,
                    DID,
                    HEATING,
                    &json!(true),
                    Some(970),
                    false,
                );
                async_io::Timer::after(Duration::from_millis(100)).await;
                assert!(
                    !has_shared_demand(control),
                    "a retired source must not start related reads"
                );
            })
            .await;
        })
        .await;
    }));
}
