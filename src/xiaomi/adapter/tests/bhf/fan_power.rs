use super::*;
use crate::device::{Command, CommandError, FanAction};
use std::sync::atomic::Ordering;

async fn assert_waiting_for_power(on: &FanAction) {
    let pending = future::or(async { Some(on.accepted().await) }, async {
        async_io::Timer::after(Duration::from_millis(60)).await;
        None
    })
    .await;
    assert_eq!(pending, None);
}

#[test]
fn a_rejected_off_preserves_the_running_low_speed() {
    let (rig, listener, cloud) = rig();
    rig.cloud
        .lock()
        .unwrap()
        .rpc_results
        .insert("get_prop".into(), json!(["coolwind", 13, 22]));
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            let blow = function(&c, BLOW);
            eventually(|| {
                blow.snapshot()
                    .fan()
                    .is_some_and(|fan| fan.power.value == Some(true) && fan.speed.value == Some(1))
            })
            .await;
            quiesce(&c).await;
            flush_all_polls(&c).await;
            let binding = (DID.into(), 3, 2);
            c.cloud
                .lock()
                .unwrap()
                .property_codes
                .insert(binding.clone(), -704);
            assert!(matches!(
                blow.execute(Command::Off).await,
                Err(CommandError::Rejected(_))
            ));
            c.cloud.lock().unwrap().property_codes.remove(&binding);
            blow.execute(Command::On).await.unwrap();
            let state = c.cloud.lock().unwrap();
            assert_eq!(state.numeric_writes.len(), 1);
            assert_eq!(state.numeric_writes[0].0[0]["value"], json!(false));
        })
        .await;
    }));
}

#[test]
fn a_rejected_speed_change_does_not_fail_an_existing_power_on() {
    let (rig, listener, cloud) = rig();
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            let blow = function(&c, BLOW);
            eventually(|| {
                blow.snapshot()
                    .fan()
                    .is_some_and(|fan| fan.power.value == Some(true) && fan.speed.value == Some(2))
            })
            .await;
            quiesce(&c).await;
            flush_all_polls(&c).await;
            c.cloud.lock().unwrap().action_code = -704;
            let low = blow.begin_fan(FanCommand::Speed(1)).unwrap();
            assert!(matches!(
                low.accepted().await,
                Err(CommandError::Rejected(_))
            ));
            blow.execute(Command::On).await.unwrap();
            assert!(c.cloud.lock().unwrap().numeric_writes.is_empty());
        })
        .await;
    }));
}

#[test]
fn a_cancelled_queued_low_only_satisfies_on_if_the_fan_is_still_running() {
    for (running, interlocked) in [(false, false), (true, false), (true, true)] {
        let (rig, listener, cloud) = rig();
        let binding = (DID.into(), 3, 2);
        rig.cloud
            .lock()
            .unwrap()
            .raw_values
            .insert(binding.clone(), json!(running));
        block_on(rig.run(|c| async move {
            future::or(cloud.run(&listener), async {
                let blow = function(&c, BLOW);
                eventually(|| blow.snapshot().fan().unwrap().power.value == Some(running)).await;
                quiesce(&c).await;
                flush_all_polls(&c).await;
                let (release, gate) = flume::bounded(1);
                if interlocked {
                    {
                        let mut state = c.cloud.lock().unwrap();
                        state.raw_values.insert(binding.clone(), json!(false));
                        state.read_gates.insert(binding.clone(), gate);
                    }
                    function(&c, HEATER).execute(Command::On).await.unwrap();
                }
                let low = blow.begin_fan(FanCommand::Speed(1)).unwrap();
                let on = blow.begin_fan(FanCommand::Power(true)).unwrap();
                let off = blow.begin_fan(FanCommand::Power(false)).unwrap();
                assert_eq!(low.accepted().await, Err(CommandError::Superseded));
                if running && !interlocked {
                    on.accepted().await.unwrap();
                } else {
                    assert_eq!(on.accepted().await, Err(CommandError::Superseded));
                }
                off.accepted().await.unwrap();
                let state = c.cloud.lock().unwrap();
                assert!(state.action_requests.is_empty());
                let fan_writes: Vec<_> = state
                    .numeric_writes
                    .iter()
                    .flat_map(|(batch, _)| batch)
                    .filter(|write| write["piid"] == 2)
                    .collect();
                assert_eq!(
                    fan_writes,
                    [&json!({"did":DID,"siid":3,"piid":2,"value":false})]
                );
                drop(release);
            })
            .await;
        }));
    }
}

#[test]
fn on_uses_new_power_feedback_after_the_heater_interlock_or_an_uncertain_off() {
    for heater_interlock in [true, false] {
        let (rig, listener, cloud) = rig();
        rig.cloud
            .lock()
            .unwrap()
            .rpc_results
            .insert("get_prop".into(), json!(["coolwind", 13, 22]));
        block_on(rig.run(|c| async move {
            future::or(cloud.run(&listener), async {
                let blow = function(&c, BLOW);
                eventually(|| {
                    blow.snapshot().fan().is_some_and(|fan| {
                        fan.power.value == Some(true) && fan.speed.value == Some(1)
                    })
                })
                .await;
                quiesce(&c).await;
                flush_all_polls(&c).await;
                let (release, gate) = flume::bounded(1);
                let binding = (DID.into(), 3, 2);
                {
                    let mut state = c.cloud.lock().unwrap();
                    state.read_gates.insert(binding.clone(), gate);
                    state
                        .raw_values
                        .insert(binding.clone(), json!(!heater_interlock));
                }
                if heater_interlock {
                    function(&c, HEATER).execute(Command::On).await.unwrap();
                } else {
                    c.cloud.lock().unwrap().property_status = 400;
                    assert!(blow.execute(Command::Off).await.is_err());
                    c.cloud.lock().unwrap().property_status = 200;
                }
                let on = blow.begin_fan(FanCommand::Power(true)).unwrap();
                assert_waiting_for_power(&on).await;
                assert_eq!(c.cloud.lock().unwrap().numeric_writes.len(), 1);
                c.cloud.lock().unwrap().read_gates.remove(&binding);
                drop(release);
                c.clock.automatic.store(true, Ordering::Relaxed);
                on.accepted().await.unwrap();
                let writes = &c.cloud.lock().unwrap().numeric_writes;
                if heater_interlock {
                    assert_eq!(writes.len(), 2);
                    assert_eq!(
                        writes[1].0,
                        [json!({"did":DID,"siid":3,"piid":2,"value":true})]
                    );
                } else {
                    assert_eq!(writes.len(), 1);
                }
            })
            .await;
        }));
    }
}

#[test]
fn plain_on_waits_for_the_new_source_power_baseline() {
    for running in [true, false] {
        let (rig, listener, cloud) = rig();
        rig.cloud
            .lock()
            .unwrap()
            .rpc_results
            .insert("get_prop".into(), json!(["coolwind", 13, 22]));
        block_on(rig.run(|c| async move {
            future::or(cloud.run(&listener), async {
                c.announce();
                let control = &c;
                c.peer(standard_peer(), |hub| async move {
                    let blow = function(control, BLOW);
                    eventually(|| {
                        work(control).property_source(BLOW, BLOW_POWER) == Some(PathKind::Local)
                            && blow.snapshot().fan().is_some_and(|fan| {
                                fan.power.value == Some(true) && fan.speed.value == Some(1)
                            })
                    })
                    .await;
                    let binding = (DID.into(), 3, 2);
                    let (release, gate) = flume::bounded(1);
                    let before = {
                        let mut state = control.cloud.lock().unwrap();
                        state.raw_values.insert(binding.clone(), json!(running));
                        state.read_gates.insert(binding.clone(), gate);
                        state
                            .property_reads
                            .iter()
                            .filter(|read| **read == binding)
                            .count()
                    };
                    hub.state.borrow_mut().online.insert(DID.into(), false);
                    hub.list_changed(&[DID]);
                    eventually(|| {
                        control
                            .cloud
                            .lock()
                            .unwrap()
                            .property_reads
                            .iter()
                            .filter(|read| **read == binding)
                            .count()
                            > before
                    })
                    .await;
                    let on = blow.begin_fan(FanCommand::Power(true)).unwrap();
                    assert_waiting_for_power(&on).await;
                    assert!(control.cloud.lock().unwrap().numeric_writes.is_empty());
                    control.cloud.lock().unwrap().read_gates.remove(&binding);
                    release.send(()).unwrap();
                    on.accepted().await.unwrap();
                    let state = control.cloud.lock().unwrap();
                    if running {
                        assert!(state.numeric_writes.is_empty());
                    } else {
                        assert_eq!(state.numeric_writes.len(), 1);
                        assert_eq!(
                            state.numeric_writes[0].0,
                            [json!({"did":DID,"siid":3,"piid":2,"value":true})]
                        );
                    }
                })
                .await;
            })
            .await;
        }));
    }
}

#[test]
fn repeated_external_heat_on_invalidates_blow_power_before_plain_on() {
    let (rig, listener, cloud) = rig();
    {
        let mut state = rig.cloud.lock().unwrap();
        state.raw_values.insert((DID.into(), 3, 3), json!(true));
        state.raw_values.insert((DID.into(), 3, 2), json!(false));
    }
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            let hub = standard_peer();
            hub.value(DID, HEATING, json!(true));
            hub.value(DID, BLOW_POWER, json!(false));
            c.announce();
            let control = &c;
            c.peer(hub, |hub| async move {
                let blow = function(control, BLOW);
                let heater = function(control, HEATER);
                eventually(|| {
                    work(control).property_source(HEATER, HEATING) == Some(PathKind::Local)
                        && work(control)
                            .function_observer(BLOW)
                            .is_some_and(|source| source.kind() == PathKind::Local)
                        && heater.snapshot().heater().unwrap().power.value == Some(true)
                        && blow.snapshot().fan().unwrap().power.value == Some(false)
                })
                .await;
                quiesce(control).await;
                flush_all_polls(control).await;
                hub.state.borrow_mut().hold_reads = true;
                // Only the function switched on sends a notification after each interlock.
                hub.value(DID, HEATING, json!(false));
                hub.value(DID, BLOW_POWER, json!(true));
                hub.notify_value_at(DID, BLOW_POWER, json!(true), Some(1001), false);
                eventually(|| blow.snapshot().fan().unwrap().power.value == Some(true)).await;
                assert_eq!(heater.snapshot().heater().unwrap().power.value, Some(true));
                hub.value(DID, BLOW_POWER, json!(false));
                hub.value(DID, HEATING, json!(true));
                hub.notify_value_at(DID, HEATING, json!(true), Some(1002), false);
                eventually(|| heater.snapshot().heater().unwrap().power.source_time == Some(1002))
                    .await;
                let on = blow.begin_fan(FanCommand::Power(true)).unwrap();
                assert_waiting_for_power(&on).await;
                hub.notify_value_at(DID, BLOW_POWER, json!(false), Some(1003), false);
                on.accepted().await.unwrap();
                let peer = hub.state.borrow();
                let writes = peer
                    .seen
                    .iter()
                    .filter(|(op, _)| op == "rpcReq")
                    .map(|(_, request)| &request["rpc"]["params"])
                    .collect::<Vec<_>>();
                assert_eq!(
                    writes,
                    [&json!([{"did":DID,"siid":3,"piid":2,"value":true}])]
                );
            })
            .await;
        })
        .await;
    }));
}
