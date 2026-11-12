use super::*;
use crate::device::{FanCommand, HeaterCommand, HeaterMode, TemperatureSourceState};
use crate::xiaomi::capability::PropertyBinding;
use serde_json::json;

mod fan_power;

const DID: &str = "one";
const BLOW: &str = "fan:3:2";
const VENTILATION: &str = "fan:3:4";
const HEATER: &str = "heater:3";
const BLOW_POWER: PropertyBinding = PropertyBinding { siid: 3, piid: 2 };
const VENTILATION_POWER: PropertyBinding = PropertyBinding { siid: 3, piid: 4 };
const HEATING: PropertyBinding = PropertyBinding { siid: 3, piid: 3 };
const TARGET: PropertyBinding = PropertyBinding { siid: 3, piid: 8 };
const HEAT_LEVEL: PropertyBinding = PropertyBinding { siid: 3, piid: 5 };
const MODE: PropertyBinding = PropertyBinding { siid: 3, piid: 1 };

fn v13() -> Value {
    serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/yeelink-bhf-light-v13.json"
    ))
    .unwrap()
}

fn rig() -> (Rig, Async<TcpListener>, Rc<CloudPeer>) {
    let rig = Rig::custom_lights(&[(DID, "yeelink.bhf_light.v13", v13())]);
    let (rig, listener, cloud) = with_cloud_rig(rig);
    {
        let mut state = rig.cloud.lock().unwrap();
        state
            .rpc_results
            .insert("get_prop".into(), json!(["heat", 31, 26.5]));
        for (binding, value) in [
            (PropertyBinding { siid: 2, piid: 1 }, json!(false)),
            (PropertyBinding { siid: 2, piid: 2 }, json!(0)),
            (PropertyBinding { siid: 2, piid: 3 }, json!(50)),
            (BLOW_POWER, json!(true)),
            (VENTILATION_POWER, json!(true)),
            (HEATING, json!(false)),
            (TARGET, json!(25)),
            (HEAT_LEVEL, json!(1)),
            (MODE, json!(0)),
        ] {
            state
                .raw_values
                .insert((DID.into(), binding.siid, binding.piid), value);
        }
    }
    (rig, listener, cloud)
}

fn function(c: &Control, id: &str) -> crate::device::Function {
    super::support::function(&c.context.registry, DID, id)
}

async fn eventually(mut predicate: impl FnMut() -> bool) {
    let ok = future::or(
        async {
            async_io::Timer::after(Duration::from_secs(10)).await;
            false
        },
        async {
            until(&mut predicate).await;
            true
        },
    )
    .await;
    assert!(ok, "timed out waiting for device behavior");
}

#[test]
fn a_malformed_fan_switch_is_a_property_read_failure() {
    let (rig, listener, cloud) = rig();
    rig.cloud
        .lock()
        .unwrap()
        .raw_values
        .insert((DID.into(), 3, 2), json!("on"));
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            let device = work(&c);
            eventually(|| {
                device
                    .refresh
                    .borrow()
                    .property_problem(BLOW, BLOW_POWER)
                    .is_some()
            })
            .await;
            assert!(device.refresh.borrow().initial_complete(BLOW, BLOW_POWER));
            assert_eq!(
                function(&c, BLOW).snapshot().fan().unwrap().power.value,
                None
            );
        })
        .await;
    }));
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

fn work(c: &Control) -> Rc<crate::xiaomi::adapter::work::DeviceWork> {
    c.context.live.borrow().devices[DID]
        .admitted
        .as_ref()
        .unwrap()
        .clone()
}

fn has_shared_demand(c: &Control) -> bool {
    let device = work(c);
    let refresh = device.refresh.borrow();
    let snapshot = refresh.snapshot();
    snapshot
        .pending
        .iter()
        .any(|read| read.input == crate::xiaomi::capability::ReadInput::SharedFeedback)
        || snapshot
            .inflight
            .iter()
            .any(|(_, input)| *input == crate::xiaomi::capability::ReadInput::SharedFeedback)
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

async fn quiesce(c: &Control) {
    eventually(|| !c.cloud.lock().unwrap().rpc_reads.is_empty()).await;
    c.clock
        .automatic
        .store(false, std::sync::atomic::Ordering::Relaxed);
    async_io::Timer::after(Duration::from_millis(50)).await;
    let device = work(c);
    eventually(|| device.refresh.borrow().snapshot().inflight.is_empty()).await;
}

async fn flush_all_polls(c: &Control) {
    for _ in 0..3 {
        c.clock.advance(Duration::from_secs(16));
        for _ in 0..12 {
            async_io::Timer::after(Duration::from_millis(60)).await;
            let device = work(c);
            let idle = {
                let refresh = device.refresh.borrow();
                let snapshot = refresh.snapshot();
                snapshot.pending.is_empty() && snapshot.inflight.is_empty()
            };
            if idle {
                return;
            }
            c.clock.advance(Duration::from_secs(1));
        }
    }
    panic!("periodic reads did not settle");
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

fn standard_peer() -> Rc<Peer> {
    let peer = Rc::new(Peer::default());
    for (binding, value) in [
        (PropertyBinding { siid: 2, piid: 1 }, json!(false)),
        (PropertyBinding { siid: 2, piid: 2 }, json!(0)),
        (PropertyBinding { siid: 2, piid: 3 }, json!(50)),
        (BLOW_POWER, json!(true)),
        (VENTILATION_POWER, json!(true)),
        (HEATING, json!(false)),
        (TARGET, json!(25)),
        (HEAT_LEVEL, json!(1)),
        (MODE, json!(0)),
    ] {
        peer.value(DID, binding, value);
    }
    peer
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
            c.context.http_paused.set(true);
            device.reconcile_state_reads(&c.context);
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

            c.context.http_paused.set(false);
            device.reconcile_state_reads(&c.context);
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

fn hub_reads(peer: &Peer, from: usize) -> Vec<(u32, u32)> {
    peer.state.borrow().seen[from..]
        .iter()
        .filter(|(operation, _)| operation == "get")
        .map(|(_, value)| {
            (
                value["siid"].as_u64().unwrap() as u32,
                value["piid"].as_u64().unwrap() as u32,
            )
        })
        .collect()
}

fn related_read(binding: PropertyBinding) -> (u32, u32) {
    (binding.siid, binding.piid)
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
                for (level, heating) in [(1, true), (2, false)] {
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

                peer.notify_value_at(DID, MODE, json!("bad"), Some(952), false);
                eventually(|| {
                    work(control)
                        .standard_mode_value()
                        .is_some_and(|value| value.reason.is_some())
                })
                .await;
                async_io::Timer::after(Duration::from_millis(100)).await;
                assert!(
                    !has_shared_demand(control),
                    "an invalid standard mode must not trigger related reads"
                );

                peer.notify_value_at(DID, HEATING, json!(false), Some(953), false);
                eventually(|| has_shared_demand(control)).await;
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

            *c.context.mijia_rejected.borrow_mut() = Some(u64::MAX);
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
            *c.context.mijia_rejected.borrow_mut() = None;
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

fn fan_speed_snapshot(
    function: &crate::device::Function,
) -> (Option<u8>, Option<i64>, Option<i64>) {
    let speed = function.snapshot().fan().unwrap().speed;
    (speed.value, speed.source_time, speed.received_at)
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
