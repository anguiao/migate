use super::*;

mod fan_power;

fn fan_speed_snapshot(
    function: &crate::device::Function,
) -> (Option<u8>, Option<i64>, Option<i64>) {
    let speed = function.snapshot().fan().unwrap().speed;
    (speed.value, speed.source_time, speed.received_at)
}

#[test]
fn cloud_low_speed_uses_the_real_action_while_high_and_off_write_only_the_boolean() {
    let (rig, listener, cloud) = rig();
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            let blow = function(&c, BLOW);
            eventually(|| {
                blow.snapshot()
                    .fan()
                    .is_some_and(|fan| fan.power.value == Some(true))
            })
            .await;

            blow.begin_fan(FanCommand::Speed(1))
                .unwrap()
                .accepted()
                .await
                .unwrap();
            eventually(|| c.cloud.lock().unwrap().action_requests.len() == 1).await;
            assert_eq!(
                c.cloud.lock().unwrap().action_requests[0],
                (DID.to_owned(), 4, 4, json!([]))
            );
            assert!(
                c.cloud
                    .lock()
                    .unwrap()
                    .numeric_writes
                    .iter()
                    .all(|(items, _)| items
                        .iter()
                        .all(|item| item["siid"] != json!(3) || item["piid"] != json!(2))),
                "low speed must not pre-power the fan or write the power property"
            );
            let status = c.context.diagnostics();
            let device = status
                .devices
                .iter()
                .find(|device| device.did == DID)
                .unwrap();
            let command = device
                .commands
                .iter()
                .find(|channel| channel.channel == BLOW)
                .unwrap();
            let text = command.last.as_ref().unwrap().properties_text();
            assert!(text.contains("low-speed cloud action 4.4"), "{text}");
            assert!(!text.contains("3.2=null"), "{text}");

            let ventilation = function(&c, VENTILATION);
            ventilation
                .begin_fan(FanCommand::Speed(1))
                .unwrap()
                .accepted()
                .await
                .unwrap();
            eventually(|| c.cloud.lock().unwrap().action_requests.len() == 2).await;
            assert_eq!(
                c.cloud.lock().unwrap().action_requests[1],
                (DID.to_owned(), 4, 5, json!([]))
            );

            // The boolean is already true, but a high command is still a real
            // change request and must not be deduplicated by the cached value.
            blow.begin_fan(FanCommand::Speed(2))
                .unwrap()
                .accepted()
                .await
                .unwrap();
            eventually(|| {
                c.cloud
                    .lock()
                    .unwrap()
                    .numeric_writes
                    .iter()
                    .any(|(items, _)| {
                        items.iter().any(|item| {
                            item["siid"] == json!(3)
                                && item["piid"] == json!(2)
                                && item["value"] == json!(true)
                        })
                    })
            })
            .await;
            assert_eq!(c.cloud.lock().unwrap().action_requests.len(), 2);

            blow.begin_fan(FanCommand::Power(false))
                .unwrap()
                .accepted()
                .await
                .unwrap();
            eventually(|| {
                c.cloud
                    .lock()
                    .unwrap()
                    .numeric_writes
                    .iter()
                    .any(|(items, _)| {
                        items.iter().any(|item| {
                            item["siid"] == json!(3)
                                && item["piid"] == json!(2)
                                && item["value"] == json!(false)
                        })
                    })
            })
            .await;
            assert_eq!(c.cloud.lock().unwrap().action_requests.len(), 2);
        })
        .await;
    }));
}

#[test]
fn accepted_heater_write_refreshes_related_standard_inputs_and_the_shared_group() {
    let (rig, listener, cloud) = rig();
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            let heater = function(&c, HEATER);
            eventually(|| {
                heater
                    .snapshot()
                    .heater()
                    .is_some_and(|heater| heater.power.value == Some(false))
            })
            .await;

            eventually(|| !c.cloud.lock().unwrap().rpc_reads.is_empty()).await;
            c.clock
                .automatic
                .store(false, std::sync::atomic::Ordering::Relaxed);
            eventually(|| {
                let device = work(&c);
                let refresh = device.refresh.borrow();
                refresh.snapshot().inflight.is_empty()
            })
            .await;
            let shared_before = c.cloud.lock().unwrap().rpc_reads.len();
            let batches_before = c.cloud.lock().unwrap().read_batches.len();
            heater
                .begin_heater(HeaterCommand::Mode(HeaterMode::Heat))
                .unwrap()
                .accepted()
                .await
                .unwrap();
            async_io::Timer::after(Duration::from_millis(100)).await;
            assert!(
                has_shared_demand(&c),
                "an accepted related write must queue the shared group"
            );
            for _ in 0..10 {
                c.clock.advance(Duration::from_secs(1));
                let done = future::or(
                    async {
                        async_io::Timer::after(Duration::from_millis(250)).await;
                        false
                    },
                    async {
                        until(|| c.cloud.lock().unwrap().rpc_reads.len() > shared_before).await;
                        true
                    },
                )
                .await;
                if done {
                    break;
                }
            }
            assert!(
                c.cloud.lock().unwrap().rpc_reads.len() > shared_before,
                "related shared feedback read was not requested"
            );
            let related = [BLOW_POWER, VENTILATION_POWER, MODE];
            for _ in 0..10 {
                let ready = {
                    let cloud = c.cloud.lock().unwrap();
                    let batches = &cloud.read_batches[batches_before..];
                    related.iter().all(|binding| {
                        batches.iter().any(|batch| {
                            batch
                                .iter()
                                .any(|read| read.1 == binding.siid && read.2 == binding.piid)
                        })
                    })
                };
                if ready {
                    break;
                }
                c.clock.advance(Duration::from_secs(1));
                async_io::Timer::after(Duration::from_millis(200)).await;
            }
            let batches = c.cloud.lock().unwrap().read_batches[batches_before..].to_vec();
            for binding in related {
                assert!(
                    batches.iter().any(|batch| batch
                        .iter()
                        .any(|read| { read.1 == binding.siid && read.2 == binding.piid })),
                    "missing related read {binding:?}: {batches:?}"
                );
            }
        })
        .await;
    }));
}

#[test]
fn an_uncertain_low_action_is_verified_read_only_and_never_replayed() {
    let (rig, listener, cloud) = rig();
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            let blow = function(&c, BLOW);
            eventually(|| {
                blow.snapshot()
                    .fan()
                    .is_some_and(|fan| fan.power.value == Some(true))
            })
            .await;
            quiesce(&c).await;
            flush_all_polls(&c).await;
            assert!(!has_shared_demand(&c));
            let (release, gate) = flume::bounded(1);
            c.cloud.lock().unwrap().rpc_gate = Some(gate);
            c.cloud.lock().unwrap().action_status = 400;

            let result = blow
                .begin_fan(FanCommand::Speed(1))
                .unwrap()
                .accepted()
                .await;
            assert!(result.is_err(), "a sent HTTP failure stays uncertain");
            async_io::Timer::after(Duration::from_millis(100)).await;
            assert_eq!(
                c.cloud.lock().unwrap().action_requests.len(),
                1,
                "an uncertain action must not be replayed"
            );
            assert!(
                has_shared_demand(&c),
                "an uncertain sent action still schedules a read-only verification"
            );
            c.cloud.lock().unwrap().rpc_gate = None;
            drop(release);
        })
        .await;
    }));
}

#[test]
fn a_rejected_action_without_an_accepted_step_does_not_start_related_reads() {
    let (rig, listener, cloud) = rig();
    let key = (DID.to_owned(), BLOW_POWER.siid, BLOW_POWER.piid);
    rig.cloud
        .lock()
        .unwrap()
        .raw_values
        .insert(key.clone(), json!(false));
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            let blow = function(&c, BLOW);
            eventually(|| {
                blow.snapshot()
                    .fan()
                    .is_some_and(|fan| fan.power.value == Some(false))
            })
            .await;
            quiesce(&c).await;
            flush_all_polls(&c).await;
            assert!(!has_shared_demand(&c));

            let (release, gate) = flume::bounded(1);
            c.cloud.lock().unwrap().rpc_gate = Some(gate);
            c.cloud.lock().unwrap().action_code = -704;
            let low = blow.begin_fan(FanCommand::Speed(1)).unwrap();
            let on = blow.begin_fan(FanCommand::Power(true)).unwrap();
            let failure = low.accepted().await.unwrap_err();
            assert_eq!(on.accepted().await.unwrap_err(), failure);
            assert!(c.cloud.lock().unwrap().numeric_writes.is_empty());
            async_io::Timer::after(Duration::from_millis(100)).await;
            assert!(
                !has_shared_demand(&c),
                "a rejected action must not request write-after verification"
            );
            c.cloud.lock().unwrap().rpc_gate = None;
            drop(release);

            // A fresh read of the same off value ends the failed start.
            let power_reads = || {
                c.cloud
                    .lock()
                    .unwrap()
                    .property_reads
                    .iter()
                    .filter(|item| *item == &key)
                    .count()
            };
            let before = power_reads();
            let device = work(&c);
            device.request_dirty_read(BLOW, BLOW_POWER);
            c.clock
                .automatic
                .store(true, std::sync::atomic::Ordering::Relaxed);
            eventually(|| {
                power_reads() > before && !device.refresh.borrow().contains(BLOW, BLOW_POWER)
            })
            .await;
            assert_eq!(blow.snapshot().fan().unwrap().power.value, Some(false));
            blow.execute(crate::device::Command::On).await.unwrap();
            let writes = &c.cloud.lock().unwrap().numeric_writes;
            assert_eq!(writes.len(), 1);
            assert_eq!(
                writes[0].0,
                [json!({"did":DID,"siid":3,"piid":2,"value":true})]
            );
        })
        .await;
    }));
}

#[test]
fn a_rejected_property_write_without_an_accepted_step_does_not_start_related_reads() {
    let (rig, listener, cloud) = rig();
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            let heater = function(&c, HEATER);
            eventually(|| {
                heater
                    .snapshot()
                    .heater()
                    .is_some_and(|heater| heater.power.value == Some(false))
            })
            .await;
            quiesce(&c).await;
            flush_all_polls(&c).await;
            assert!(!has_shared_demand(&c));
            let (release, gate) = flume::bounded(1);
            c.cloud.lock().unwrap().rpc_gate = Some(gate);
            c.cloud
                .lock()
                .unwrap()
                .property_codes
                .insert((DID.into(), 3, 3), -704);
            assert!(
                heater
                    .begin_heater(HeaterCommand::Power(true))
                    .unwrap()
                    .accepted()
                    .await
                    .is_err()
            );
            async_io::Timer::after(Duration::from_millis(100)).await;
            assert!(
                !has_shared_demand(&c),
                "a rejected-only property write must not verify the shared group"
            );
            c.cloud.lock().unwrap().rpc_gate = None;
            drop(release);
        })
        .await;
    }));
}

#[test]
fn low_speed_never_uses_the_hub_even_when_its_standard_properties_are_ready() {
    let (rig, listener, cloud) = rig();
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            c.announce();
            let control = &c;
            c.peer(standard_peer(), |peer| async move {
                let blow = function(control, BLOW);
                eventually(|| {
                    work(control).function_observer(BLOW).is_some()
                        && blow
                            .snapshot()
                            .fan()
                            .is_some_and(|fan| fan.power.value == Some(true))
                })
                .await;
                blow.begin_fan(FanCommand::Speed(1))
                    .unwrap()
                    .accepted()
                    .await
                    .unwrap();
                eventually(|| control.cloud.lock().unwrap().action_requests.len() == 1).await;
                assert_eq!(
                    control.cloud.lock().unwrap().action_requests[0],
                    (DID.to_owned(), 4, 4, json!([]))
                );
                assert!(
                    peer.state.borrow().seen.iter().all(|(operation, value)| {
                        operation != "rpcReq"
                            || value["rpc"]["params"][0]["siid"] != json!(3)
                            || value["rpc"]["params"][0]["piid"] != json!(2)
                    }),
                    "the hub must not receive a low-speed power write"
                );
            })
            .await;
        })
        .await;
    }));
}

#[test]
fn temperature_write_refreshes_only_target_and_current_temperature() {
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
                quiesce(control).await;
                flush_all_polls(control).await;
                let seen = peer.state.borrow().seen.len();
                let (release, gate) = flume::bounded(1);
                control.cloud.lock().unwrap().rpc_gate = Some(gate);

                heater
                    .begin_heater(HeaterCommand::Temperature(30))
                    .unwrap()
                    .accepted()
                    .await
                    .unwrap();
                let mut reads = Vec::new();
                for _ in 0..5 {
                    control.clock.advance(Duration::from_secs(1));
                    async_io::Timer::after(Duration::from_millis(150)).await;
                    reads = hub_reads(&peer, seen);
                    if reads.contains(&related_read(TARGET)) {
                        break;
                    }
                }
                assert!(reads.contains(&related_read(TARGET)), "{reads:?}");
                for binding in [HEATING, BLOW_POWER, VENTILATION_POWER, MODE, HEAT_LEVEL] {
                    assert!(
                        !reads.contains(&related_read(binding)),
                        "temperature write refreshed unrelated {binding:?}: {reads:?}"
                    );
                }
                assert!(has_shared_demand(control));
                control.cloud.lock().unwrap().rpc_gate = None;
                drop(release);
            })
            .await;
        })
        .await;
    }));
}

#[test]
fn heater_write_refreshes_the_full_related_set() {
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
                let (release, gate) = flume::bounded(1);
                control.cloud.lock().unwrap().rpc_gate = Some(gate);

                let seen = peer.state.borrow().seen.len();
                function(control, HEATER)
                    .begin_heater(HeaterCommand::Power(true))
                    .unwrap()
                    .accepted()
                    .await
                    .unwrap();
                let expected = [HEATING, TARGET, BLOW_POWER, VENTILATION_POWER, MODE];
                let mut reads = Vec::new();
                for _ in 0..8 {
                    control.clock.advance(Duration::from_secs(1));
                    async_io::Timer::after(Duration::from_millis(150)).await;
                    reads = hub_reads(&peer, seen);
                    if expected
                        .into_iter()
                        .all(|binding| reads.contains(&related_read(binding)))
                    {
                        break;
                    }
                }
                for binding in expected {
                    assert!(
                        reads.contains(&related_read(binding)),
                        "heater write missed {binding:?}: {reads:?}"
                    );
                }
                assert!(has_shared_demand(control));
                control.cloud.lock().unwrap().rpc_gate = None;
                drop(release);
            })
            .await;
        })
        .await;
    }));
}

#[test]
fn light_write_does_not_start_related_reads() {
    let (rig, listener, cloud) = rig();
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            c.announce();
            let control = &c;
            c.peer(standard_peer(), |peer| async move {
                eventually(|| {
                    work(control).function_observer("light:2").is_some()
                        && !control.cloud.lock().unwrap().rpc_reads.is_empty()
                })
                .await;
                quiesce(control).await;
                flush_all_polls(control).await;
                assert!(!has_shared_demand(control));
                let seen = peer.state.borrow().seen.len();
                let shared = control.cloud.lock().unwrap().rpc_reads.len();
                function(control, "light:2")
                    .execute(crate::device::Command::On)
                    .await
                    .unwrap();
                for _ in 0..4 {
                    control.clock.advance(Duration::from_secs(1));
                    async_io::Timer::after(Duration::from_millis(150)).await;
                }
                let reads = hub_reads(&peer, seen);
                for binding in [HEATING, TARGET, BLOW_POWER, VENTILATION_POWER, MODE] {
                    assert!(
                        !reads.contains(&related_read(binding)),
                        "light write triggered unrelated {binding:?}: {reads:?}"
                    );
                }
                assert!(
                    !has_shared_demand(control),
                    "light write must not request the shared legacy group"
                );
                assert_eq!(control.cloud.lock().unwrap().rpc_reads.len(), shared);
            })
            .await;
        })
        .await;
    }));
}

#[test]
fn an_uncertain_action_refreshes_even_when_notifications_are_ready() {
    let (rig, listener, cloud) = rig();
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            c.announce();
            let control = &c;
            let hub = standard_peer();
            hub.value(DID, BLOW_POWER, json!(false));
            c.peer(hub, |_peer| async move {
                let blow = function(control, BLOW);
                eventually(|| {
                    work(control).function_observer(BLOW).is_some()
                        && blow
                            .snapshot()
                            .fan()
                            .is_some_and(|fan| fan.power.value == Some(false))
                        && !control.cloud.lock().unwrap().rpc_reads.is_empty()
                })
                .await;
                quiesce(control).await;
                flush_all_polls(control).await;
                assert!(!has_shared_demand(control));
                let (release, gate) = flume::bounded(1);
                control.cloud.lock().unwrap().rpc_gate = Some(gate);
                control.cloud.lock().unwrap().action_status = 400;

                let low = blow.begin_fan(FanCommand::Speed(1)).unwrap();
                let on = blow.begin_fan(FanCommand::Power(true)).unwrap();
                let failure = low.accepted().await.unwrap_err();
                assert_eq!(on.accepted().await.unwrap_err(), failure);
                assert!(control.cloud.lock().unwrap().numeric_writes.is_empty());
                async_io::Timer::after(Duration::from_millis(100)).await;
                assert!(
                    has_shared_demand(control),
                    "uncertain sent control still needs read-only verification with ready notifications"
                );
                control.cloud.lock().unwrap().rpc_gate = None;
                drop(release);
            })
            .await;
        })
        .await;
    }));
}

#[test]
fn a_sent_low_action_invalidates_an_earlier_shared_read_before_its_receipt() {
    let (rig, listener, cloud) = rig();
    let rig = rig.with_stall_timeout(Duration::from_secs(15));
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
                    && !c.cloud.lock().unwrap().rpc_reads.is_empty()
            })
            .await;
            quiesce(&c).await;
            flush_all_polls(&c).await;
            let before_blow = fan_speed_snapshot(&blow);
            let before_ventilation = fan_speed_snapshot(&ventilation);

            let (release_read, read_gate) = flume::bounded(1);
            {
                let mut state = c.cloud.lock().unwrap();
                state.rpc_gate = Some(read_gate);
                state
                    .rpc_results
                    .insert("get_prop".into(), json!(["heat", 32, 26.5]));
            }
            let reads = c.cloud.lock().unwrap().rpc_reads.len();
            c.clock.advance(Duration::from_secs(16));
            eventually(|| c.cloud.lock().unwrap().rpc_reads.len() > reads).await;

            let (release_action, action_gate) = flume::bounded(1);
            c.cloud.lock().unwrap().action_gate = Some(action_gate);
            let action = blow.begin_fan(FanCommand::Speed(1)).unwrap();
            let mut action = Box::pin(action.accepted());
            assert!(
                futures_lite::future::poll_once(action.as_mut())
                    .await
                    .is_none()
            );
            eventually(|| c.cloud.lock().unwrap().action_requests.len() == 1).await;

            c.cloud
                .lock()
                .unwrap()
                .rpc_results
                .insert("get_prop".into(), json!(["dry", 13, 27.0]));
            c.cloud.lock().unwrap().rpc_gate = None;
            release_read.send(()).unwrap();
            // Barrier: the old flight must finish before comparing complete
            // value/source-time/observation-time tuples for both fans.
            eventually(|| {
                let device = work(&c);
                !device
                    .refresh
                    .borrow()
                    .snapshot()
                    .inflight
                    .iter()
                    .any(|(_, input)| {
                        *input == crate::xiaomi::capability::ReadInput::SharedFeedback
                    })
            })
            .await;
            assert_eq!(
                fan_speed_snapshot(&blow),
                before_blow,
                "an earlier shared response must not change the blow tuple after send"
            );
            assert_eq!(
                fan_speed_snapshot(&ventilation),
                before_ventilation,
                "an earlier shared response must not change the ventilation tuple after send"
            );

            c.cloud.lock().unwrap().action_gate = None;
            release_action.send(()).unwrap();
            action.await.unwrap();
            for _ in 0..10 {
                c.clock.advance(Duration::from_secs(1));
                async_io::Timer::after(Duration::from_millis(150)).await;
                if blow
                    .snapshot()
                    .fan()
                    .is_some_and(|fan| fan.speed.value == Some(1))
                    && ventilation
                        .snapshot()
                        .fan()
                        .is_some_and(|fan| fan.speed.value == Some(2))
                {
                    break;
                }
            }
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
    }));
}

#[test]
fn a_sent_heater_write_invalidates_an_earlier_shared_read_before_its_receipt() {
    let (rig, listener, cloud) = rig();
    let rig = rig.with_stall_timeout(Duration::from_secs(15));
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            let heater = function(&c, HEATER);
            let blow = function(&c, BLOW);
            let ventilation = function(&c, VENTILATION);
            eventually(|| {
                heater
                    .snapshot()
                    .heater()
                    .is_some_and(|heater| heater.power.value == Some(false))
                    && blow
                        .snapshot()
                        .fan()
                        .is_some_and(|fan| fan.speed.value == Some(2))
                    && ventilation
                        .snapshot()
                        .fan()
                        .is_some_and(|fan| fan.speed.value == Some(1))
                    && !c.cloud.lock().unwrap().rpc_reads.is_empty()
            })
            .await;
            quiesce(&c).await;
            flush_all_polls(&c).await;
            let before_blow = fan_speed_snapshot(&blow);
            let before_ventilation = fan_speed_snapshot(&ventilation);

            let (release_read, read_gate) = flume::bounded(1);
            {
                let mut state = c.cloud.lock().unwrap();
                state.rpc_gate = Some(read_gate);
                state
                    .rpc_results
                    .insert("get_prop".into(), json!(["heat", 32, 26.5]));
            }
            let reads = c.cloud.lock().unwrap().rpc_reads.len();
            c.clock.advance(Duration::from_secs(16));
            eventually(|| c.cloud.lock().unwrap().rpc_reads.len() > reads).await;

            let (release_write, write_gate) = flume::bounded(1);
            c.cloud.lock().unwrap().property_gate = Some(write_gate);
            let command = heater.begin_heater(HeaterCommand::Power(true)).unwrap();
            let mut write = Box::pin(command.accepted());
            assert!(
                futures_lite::future::poll_once(write.as_mut())
                    .await
                    .is_none()
            );
            eventually(|| !c.cloud.lock().unwrap().numeric_writes.is_empty()).await;

            c.cloud
                .lock()
                .unwrap()
                .rpc_results
                .insert("get_prop".into(), json!(["dry", 13, 27.0]));
            c.cloud.lock().unwrap().rpc_gate = None;
            release_read.send(()).unwrap();
            // Barrier: the old flight must finish before comparing complete
            // value/source-time/observation-time tuples for both fans.
            eventually(|| {
                let device = work(&c);
                !device
                    .refresh
                    .borrow()
                    .snapshot()
                    .inflight
                    .iter()
                    .any(|(_, input)| {
                        *input == crate::xiaomi::capability::ReadInput::SharedFeedback
                    })
            })
            .await;
            assert_eq!(
                fan_speed_snapshot(&blow),
                before_blow,
                "an earlier shared response must not change the blow tuple after send"
            );
            assert_eq!(
                fan_speed_snapshot(&ventilation),
                before_ventilation,
                "an earlier shared response must not change the ventilation tuple after send"
            );

            c.cloud.lock().unwrap().property_gate = None;
            release_write.send(()).unwrap();
            write.await.unwrap();
            for _ in 0..10 {
                c.clock.advance(Duration::from_secs(1));
                async_io::Timer::after(Duration::from_millis(150)).await;
                if blow
                    .snapshot()
                    .fan()
                    .is_some_and(|fan| fan.speed.value == Some(1))
                    && ventilation
                        .snapshot()
                        .fan()
                        .is_some_and(|fan| fan.speed.value == Some(2))
                {
                    break;
                }
            }
            assert!(
                blow.snapshot()
                    .fan()
                    .is_some_and(|fan| fan.speed.value == Some(1))
                    && ventilation
                        .snapshot()
                        .fan()
                        .is_some_and(|fan| fan.speed.value == Some(2)),
                "the new readback must apply after the blocked control settles"
            );
        })
        .await;
    }));
}
