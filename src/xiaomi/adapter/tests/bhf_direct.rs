use super::*;
use crate::{
    device::{Command, CommandError, FanCommand, TemperatureSourceState},
    storage::LanRecord,
    xiaomi::{
        adapter::model::{LanState, PathKind},
        capability::PropertyBinding,
        lan::{Token, tests::support::LegacyDevice},
    },
};
use std::sync::atomic::Ordering;

const DID: &str = "743765151";
const MODEL: &str = "yeelink.bhf_light.v13";
const BLOW: &str = "fan:3:2";
const VENTILATION: &str = "fan:3:4";
const HEATER: &str = "heater:3";
const BLOW_POWER: PropertyBinding = PropertyBinding { siid: 3, piid: 2 };
const HEATING: PropertyBinding = PropertyBinding { siid: 3, piid: 3 };
const VENTILATION_POWER: PropertyBinding = PropertyBinding { siid: 3, piid: 4 };

fn v13() -> Value {
    serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/yeelink-bhf-light-v13.json"
    ))
    .unwrap()
}

fn setup() -> (Rig, LegacyDevice) {
    let rig =
        Rig::custom_lights(&[(DID, MODEL, v13())]).with_stall_timeout(Duration::from_secs(20));
    let token = Token::parse("00112233445566778899aabbccddeeff").unwrap();
    let peer = LegacyDevice::bhf_v13(token.clone(), DID.parse().unwrap());
    rig.store
        .lan()
        .save(&LanRecord {
            scope: rig
                .store
                .devices()
                .load_configuration()
                .unwrap()
                .scope
                .unwrap(),
            did: DID.into(),
            model: MODEL.into(),
            address: "127.0.0.1".parse().unwrap(),
            token,
        })
        .unwrap();
    rig.network.lan_port.set(peer.address.port());
    rig.clock.automatic.store(true, Ordering::Relaxed);
    (rig, peer)
}

fn function(c: &Control, id: &str) -> crate::device::Function {
    super::support::function(&c.context.registry, DID, id)
}

fn work(c: &Control) -> Rc<crate::xiaomi::adapter::work::DeviceWork> {
    c.context.directory.borrow().devices[DID]
        .admitted
        .as_ref()
        .unwrap()
        .clone()
}

async fn eventually(mut predicate: impl FnMut() -> bool) {
    let ok = future::or(
        async {
            async_io::Timer::after(Duration::from_secs(8)).await;
            false
        },
        async {
            until(&mut predicate).await;
            true
        },
    )
    .await;
    assert!(ok, "timed out waiting for direct bath-heater behavior");
}

#[test]
fn v13_direct_admission_confirms_a_real_boolean_and_reads_standard_baseline() {
    let (rig, peer) = setup();
    block_on(rig.run(|c| async move {
        let blow = function(&c, BLOW);
        let heater = function(&c, HEATER);
        eventually(|| {
            c.context.direct_candidate(DID).is_ok()
                && blow
                    .snapshot()
                    .fan()
                    .is_some_and(|fan| fan.power.value == Some(true))
                && heater
                    .snapshot()
                    .heater()
                    .is_some_and(|heater| heater.power.value == Some(false))
                && c.context.diagnostics().devices.iter().any(|device| {
                    device.did == DID
                        && device.selected_control == Some(PathKind::Direct)
                        && device.selected_read == Some(PathKind::Direct)
                })
        })
        .await;

        let requests = peer.requests.lock().unwrap().clone();
        let info = requests
            .iter()
            .position(|request| request["method"] == "miIO.info")
            .expect("direct admission must confirm the device model");
        let probe = requests
            .get(info + 1)
            .expect("a read-only business confirmation must follow miIO.info");
        assert_eq!(probe["method"], "get_properties", "{requests:?}");
        let item = &probe["params"][0];
        assert_eq!(item["did"], DID);
        let binding = (
            item["siid"].as_u64().unwrap() as u32,
            item["piid"].as_u64().unwrap() as u32,
        );
        assert!(
            matches!(binding, (2, 1) | (3, 2) | (3, 3) | (3, 4)),
            "admission must confirm a real boolean property from the source plan: {requests:?}"
        );

        assert_eq!(
            work(&c).property_source(BLOW, BLOW_POWER),
            Some(PathKind::Direct),
            "standard properties use the authenticated LAN connection"
        );
        assert_eq!(peer.properties.lock().unwrap()[&(3, 2)], json!(true));
        assert_eq!(
            work(&c).property_source(HEATER, HEATING),
            Some(PathKind::Direct),
            "the heater switch baseline is independent of the model probe"
        );
        let status = c
            .context
            .diagnostics()
            .devices
            .into_iter()
            .find(|device| device.did == DID)
            .unwrap();
        assert_eq!(status.lan.unwrap().state, LanState::Ready);
    }));
}

#[test]
fn v13_direct_low_speed_sends_only_set_bh_mode_without_a_high_speed_write() {
    let (rig, peer) = setup();
    block_on(rig.run(|c| async move {
        let blow = function(&c, BLOW);
        eventually(|| {
            c.context.direct_candidate(DID).is_ok()
                && blow
                    .snapshot()
                    .fan()
                    .is_some_and(|fan| fan.power.value == Some(true) && fan.speed.value == Some(2))
        })
        .await;

        for (id, mode) in [(BLOW, "coolwind"), (VENTILATION, "venting")] {
            let before = peer.requests.lock().unwrap().len();
            function(&c, id)
                .begin_fan(FanCommand::Speed(1))
                .unwrap()
                .accepted()
                .await
                .unwrap();
            let requests = peer.requests.lock().unwrap();
            let writes = requests[before..]
                .iter()
                .filter(|request| {
                    matches!(
                        request["method"].as_str(),
                        Some("set_bh_mode" | "set_properties")
                    )
                })
                .map(|request| (&request["method"], &request["params"]))
                .collect::<Vec<_>>();
            assert_eq!(writes, [(&json!("set_bh_mode"), &json!([mode, 1]))], "{id}");
        }
        assert!(
            c.cloud.lock().unwrap().action_requests.is_empty(),
            "a selected direct legacy command must not also invoke the cloud action"
        );
    }));
}

#[test]
fn plain_on_preserves_low_speed_before_and_after_power_feedback() {
    let (rig, direct) = setup();
    rig.network.lan_timeout.set(Duration::from_millis(500));
    let (rig, listener, cloud) = with_cloud_rig(rig);
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            let hub = standard_peer();
            for binding in [BLOW_POWER, VENTILATION_POWER] {
                hub.value(DID, binding, json!(false));
            }
            c.announce();
            let control = &c;
            c.peer(hub, |hub| async move {
                eventually(|| {
                    control.context.direct_candidate(DID).is_ok()
                        && work(control).shared_feedback().source == Some(PathKind::Direct)
                        && [BLOW, VENTILATION].iter().all(|id| {
                            work(control).function_observer(id).is_some()
                                && function(control, id).snapshot().fan().unwrap().power.value
                                    == Some(false)
                        })
                })
                .await;
                hub.state.borrow_mut().hold_reads = true;
                for (id, binding, mode) in [
                    (BLOW, BLOW_POWER, "coolwind"),
                    (VENTILATION, VENTILATION_POWER, "venting"),
                ] {
                    let fan = function(control, id);
                    let before = hub_writes(&hub).len();
                    let low_before = direct_methods(&direct, "set_bh_mode").len();
                    fan.begin_fan(FanCommand::Speed(1))
                        .unwrap()
                        .accepted()
                        .await
                        .unwrap();
                    fan.execute(Command::On).await.unwrap();
                    assert_eq!(fan.snapshot().fan().unwrap().power.value, Some(false));
                    assert_eq!(hub_writes(&hub).len(), before);
                    assert_eq!(
                        direct_methods(&direct, "set_bh_mode")[low_before]["params"],
                        json!([mode, 1])
                    );

                    // Some controllers finish the same gesture with another explicit 50%.
                    fan.begin_fan(FanCommand::Speed(1))
                        .unwrap()
                        .accepted()
                        .await
                        .unwrap();
                    assert_eq!(direct_methods(&direct, "set_bh_mode").len(), low_before + 2);
                    hub.notify_value(DID, binding, json!(true), false);
                    eventually(|| fan.snapshot().fan().unwrap().power.value == Some(true)).await;
                    fan.execute(Command::On).await.unwrap();
                    assert_eq!(hub_writes(&hub).len(), before);
                    fan.begin_fan(FanCommand::Speed(2))
                        .unwrap()
                        .accepted()
                        .await
                        .unwrap();
                    hub.notify_value(DID, binding, json!(false), false);
                    eventually(|| fan.snapshot().fan().unwrap().power.value == Some(false)).await;
                    fan.execute(Command::On).await.unwrap();
                    fan.execute(Command::Off).await.unwrap();
                    fan.execute(Command::On).await.unwrap();
                    assert_eq!(
                        hub_writes(&hub)[before..],
                        [
                            (3, binding.piid, json!(true)),
                            (3, binding.piid, json!(true)),
                            (3, binding.piid, json!(false)),
                            (3, binding.piid, json!(true)),
                        ]
                    );
                }
                let before = hub_writes(&hub).len();
                direct.drop_write.store(true, Ordering::Relaxed);
                let blow = function(control, BLOW);
                let low = blow.begin_fan(FanCommand::Speed(1)).unwrap();
                let on = blow.begin_fan(FanCommand::Power(true)).unwrap();
                assert_eq!(low.accepted().await, Err(CommandError::Timeout));
                assert_eq!(on.accepted().await, Err(CommandError::Timeout));
                assert_eq!(hub_writes(&hub).len(), before);
                blow.execute(Command::Off).await.unwrap();
                blow.execute(Command::On).await.unwrap();
                assert_eq!(
                    hub_writes(&hub)[before..],
                    [(3, 2, json!(false)), (3, 2, json!(true))]
                );
            })
            .await;
        })
        .await;
    }));
}

#[test]
fn v13_direct_shared_get_prop_feeds_both_fans_and_the_built_in_temperature() {
    let (rig, peer) = setup();
    block_on(rig.run(|c| async move {
        let blow = function(&c, BLOW);
        let ventilation = function(&c, VENTILATION);
        let heater = function(&c, HEATER);
        eventually(|| {
            c.context.direct_candidate(DID).is_ok()
                && blow
                    .snapshot()
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

        let requests = peer.requests.lock().unwrap().clone();
        let shared = requests
            .iter()
            .filter(|request| request["method"] == "get_prop")
            .collect::<Vec<_>>();
        assert!(
            shared
                .iter()
                .any(|request| request["params"]
                    == json!(["bh_mode", "fan_speed_idx", "temperature"])),
            "the shared legacy group must be read over the LAN connection: {requests:?}"
        );
        assert!(
            requests.iter().any(|request| {
                request["method"] == "get_properties"
                    && request["params"].as_array().unwrap().iter().any(|item| {
                        (item["siid"] == json!(3))
                            && [2, 3, 4, 5, 1, 8].contains(&item["piid"].as_u64().unwrap())
                    })
            }),
            "standard properties must still be read independently: {requests:?}"
        );

        let device = work(&c);
        let shared = device.shared_feedback();
        assert_eq!(shared.source, Some(PathKind::Direct));
        assert!(!shared.pending);
        assert_eq!(
            shared
                .temperature
                .as_ref()
                .and_then(|temperature| temperature.value.as_f64()),
            Some(26.5)
        );
        assert!(
            requests
                .iter()
                .any(|request| request["method"] == "get_properties"),
            "a directly confirmed standard property is still required"
        );
        assert_eq!(
            function(&c, HEATER)
                .snapshot()
                .heater()
                .unwrap()
                .current_temperature
                .value,
            Some(26.5)
        );
        assert!(
            c.cloud.lock().unwrap().rpc_reads.is_empty(),
            "the direct shared group must not also use cloud RPC"
        );
    }));
}

fn standard_peer() -> Rc<Peer> {
    let peer = Rc::new(Peer::default());
    for (binding, value) in [
        (PropertyBinding { siid: 2, piid: 1 }, json!(false)),
        (PropertyBinding { siid: 2, piid: 2 }, json!(0)),
        (PropertyBinding { siid: 2, piid: 3 }, json!(50)),
        (BLOW_POWER, json!(true)),
        (PropertyBinding { siid: 3, piid: 4 }, json!(true)),
        (HEATING, json!(false)),
        (PropertyBinding { siid: 3, piid: 8 }, json!(25)),
        (PropertyBinding { siid: 3, piid: 1 }, json!(0)),
    ] {
        peer.value(DID, binding, value);
    }
    peer
}

fn hub_writes(peer: &Peer) -> Vec<(u32, u32, Value)> {
    peer.state
        .borrow()
        .seen
        .iter()
        .filter(|(operation, _)| operation == "rpcReq")
        .map(|(_, request)| {
            let item = &request["rpc"]["params"][0];
            (
                item["siid"].as_u64().unwrap() as u32,
                item["piid"].as_u64().unwrap() as u32,
                item["value"].clone(),
            )
        })
        .collect()
}

fn direct_methods(peer: &LegacyDevice, method: &str) -> Vec<Value> {
    peer.requests
        .lock()
        .unwrap()
        .iter()
        .filter(|request| request["method"] == method)
        .cloned()
        .collect()
}

#[test]
fn hub_standard_properties_coexist_with_direct_shared_feedback_and_low_speed() {
    let (rig, direct) = setup();
    let (rig, listener, cloud) = with_cloud_rig(rig);
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            c.announce();
            let control = &c;
            c.peer(standard_peer(), |hub| async move {
                let blow = function(control, BLOW);
                let heater = function(control, HEATER);
                eventually(|| {
                    control.context.direct_candidate(DID).is_ok()
                        && work(control).function_observer(HEATER).is_some()
                        && work(control)
                            .shared_feedback()
                            .source
                            .is_some_and(|source| source == PathKind::Direct)
                        && blow
                            .snapshot()
                            .fan()
                            .is_some_and(|fan| fan.power.value == Some(true))
                })
                .await;

                heater
                    .begin_heater(crate::device::HeaterCommand::Power(true))
                    .unwrap()
                    .accepted()
                    .await
                    .unwrap();
                eventually(|| hub_writes(&hub).contains(&(3, 3, json!(true)))).await;
                assert!(
                    direct_methods(&direct, "set_properties")
                        .iter()
                        .all(|request| request["params"][0]["siid"] != json!(3)
                            || request["params"][0]["piid"] != json!(3)),
                    "the heating property stays on the hub path when it is ready"
                );
                assert!(control.cloud.lock().unwrap().numeric_writes.is_empty());

                blow.begin_fan(FanCommand::Speed(1))
                    .unwrap()
                    .accepted()
                    .await
                    .unwrap();
                eventually(|| {
                    direct_methods(&direct, "set_bh_mode")
                        .iter()
                        .any(|request| request["params"] == json!(["coolwind", 1]))
                })
                .await;
                assert!(
                    control.cloud.lock().unwrap().action_requests.is_empty(),
                    "low speed must not also use the cloud action"
                );
                assert!(
                    direct_methods(&direct, "get_prop").iter().any(|request| {
                        request["params"] == json!(["bh_mode", "fan_speed_idx", "temperature"])
                    }),
                    "the shared group keeps its direct path independently of the hub"
                );
                assert!(control.cloud.lock().unwrap().rpc_reads.is_empty());
                let blow_command = control
                    .context
                    .diagnostics()
                    .devices
                    .into_iter()
                    .find(|device| device.did == DID)
                    .unwrap()
                    .commands
                    .into_iter()
                    .find(|command| command.channel == BLOW)
                    .unwrap()
                    .last
                    .unwrap();
                assert_eq!(blow_command.actual_path, Some(PathKind::Direct));
                assert_eq!(
                    heater
                        .snapshot()
                        .heater()
                        .unwrap()
                        .current_temperature
                        .value,
                    Some(26.5)
                );
                let status = control
                    .context
                    .diagnostics()
                    .devices
                    .into_iter()
                    .find(|device| device.did == DID)
                    .unwrap();
                assert_eq!(status.selected_control, Some(PathKind::Local));
                assert_eq!(
                    status
                        .commands
                        .iter()
                        .find(|command| command.channel == HEATER)
                        .and_then(|command| command.last.as_ref())
                        .and_then(|command| command.actual_path),
                    Some(PathKind::Local)
                );
            })
            .await;
        })
        .await;
    }));
}

#[test]
fn direct_business_and_field_failures_do_not_revoke_shared_or_standard_access() {
    let (rig, peer) = setup();
    block_on(rig.run(|c| async move {
        let blow = function(&c, BLOW);
        let ventilation = function(&c, VENTILATION);
        let heater = function(&c, HEATER);
        eventually(|| {
            c.context.direct_candidate(DID).is_ok()
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

        peer.values.lock().unwrap()[1] = json!(32);
        peer.values.lock().unwrap()[2] = json!("not-a-temperature");
        work(&c).refresh_shared_feedback();
        eventually(|| {
            let status = c.context.diagnostics();
            let device = status
                .devices
                .iter()
                .find(|device| device.did == DID)
                .unwrap();
            device.shared_feedback.as_ref().is_some_and(|shared| {
                shared
                    .fans
                    .iter()
                    .any(|fan| fan.function == BLOW && fan.level == Some(2) && fan.reason.is_none())
                    && shared.fans.iter().any(|fan| {
                        fan.function == VENTILATION
                            && fan.level.is_none()
                            && fan
                                .reason
                                .as_deref()
                                .is_some_and(|reason| reason.contains('2'))
                    })
                    && shared.temperature.as_ref().is_some_and(|temperature| {
                        temperature.value == json!("not-a-temperature")
                            && temperature.reason.is_some()
                    })
            })
        })
        .await;
        assert_eq!(
            heater
                .snapshot()
                .heater()
                .unwrap()
                .current_temperature
                .value,
            None
        );
        assert_eq!(
            heater.snapshot().heater().unwrap().temperature_source,
            TemperatureSourceState::AwaitingReading
        );

        peer.values.lock().unwrap()[1] = json!(31);
        peer.values.lock().unwrap()[2] = json!(26.5);
        work(&c).refresh_shared_feedback();
        eventually(|| {
            heater
                .snapshot()
                .heater()
                .is_some_and(|heater| heater.current_temperature.value == Some(26.5))
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

        peer.rejected
            .lock()
            .unwrap()
            .insert("get_prop".into(), -4004);
        work(&c).refresh_shared_feedback();
        eventually(|| {
            work(&c)
                .refresh
                .borrow()
                .input_problem(
                    crate::xiaomi::capability::SHARED_FEEDBACK_CHANNEL,
                    crate::xiaomi::capability::ReadInput::SharedFeedback,
                )
                .is_some()
        })
        .await;
        assert!(c.context.direct_candidate(DID).is_ok());
        assert_eq!(
            work(&c).property_source(BLOW, BLOW_POWER),
            Some(PathKind::Direct)
        );
        let shared = c
            .context
            .diagnostics()
            .devices
            .into_iter()
            .find(|device| device.did == DID)
            .unwrap()
            .shared_feedback
            .unwrap();
        assert_eq!(shared.selected_read, Some(PathKind::Direct));
        assert_eq!(shared.source, Some(PathKind::Direct));
        assert!(shared.read_problem.is_some());
        assert_eq!(
            blow.snapshot().fan().unwrap().speed.value,
            Some(2),
            "a rejected group read retains the last valid fan level"
        );
        assert_eq!(
            heater
                .snapshot()
                .heater()
                .unwrap()
                .current_temperature
                .value,
            Some(26.5),
            "a rejected group read retains the last valid temperature"
        );

        peer.rejected.lock().unwrap().remove("get_prop");
        peer.rejected_properties
            .lock()
            .unwrap()
            .insert((3, 3), -704);
        assert!(
            heater
                .begin_heater(crate::device::HeaterCommand::Power(true))
                .unwrap()
                .accepted()
                .await
                .is_err()
        );
        assert!(c.context.direct_candidate(DID).is_ok());
        assert_eq!(
            work(&c).property_source(BLOW, BLOW_POWER),
            Some(PathKind::Direct)
        );
        peer.rejected_properties.lock().unwrap().remove(&(3, 3));
        blow.begin_fan(FanCommand::Power(false))
            .unwrap()
            .accepted()
            .await
            .unwrap();
        eventually(|| {
            direct_methods(&peer, "set_properties")
                .iter()
                .any(|request| {
                    request["params"][0]["siid"] == json!(3)
                        && request["params"][0]["piid"] == json!(2)
                        && request["params"][0]["value"] == json!(false)
                })
        })
        .await;
        work(&c).refresh_shared_feedback();
        eventually(|| {
            work(&c)
                .refresh
                .borrow()
                .input_problem(
                    crate::xiaomi::capability::SHARED_FEEDBACK_CHANNEL,
                    crate::xiaomi::capability::ReadInput::SharedFeedback,
                )
                .is_none()
                && heater
                    .snapshot()
                    .heater()
                    .is_some_and(|heater| heater.current_temperature.value == Some(26.5))
        })
        .await;
    }));
}

#[test]
fn direct_outage_switches_shared_and_low_to_cloud_and_recovery_rebaselines_directly() {
    let (rig, direct) = setup();
    rig.network.lan_timeout.set(Duration::from_millis(100));
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud
            .rpc_results
            .insert("get_prop".into(), json!(["dry", 13, 27.0]));
        for (binding, value) in [
            (PropertyBinding { siid: 2, piid: 1 }, json!(false)),
            (PropertyBinding { siid: 2, piid: 2 }, json!(0)),
            (PropertyBinding { siid: 2, piid: 3 }, json!(50)),
            (BLOW_POWER, json!(true)),
            (VENTILATION_POWER, json!(true)),
            (HEATING, json!(false)),
            (PropertyBinding { siid: 3, piid: 8 }, json!(25)),
            (PropertyBinding { siid: 3, piid: 1 }, json!(0)),
        ] {
            cloud
                .raw_values
                .insert((DID.into(), binding.siid, binding.piid), value);
        }
    }
    let (rig, listener, cloud) = with_cloud_rig(rig);
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            let blow = function(&c, BLOW);
            let heater = function(&c, HEATER);
            eventually(|| {
                c.context.direct_candidate(DID).is_ok()
                    && blow
                        .snapshot()
                        .fan()
                        .is_some_and(|fan| fan.speed.value == Some(2))
                    && heater
                        .snapshot()
                        .heater()
                        .is_some_and(|heater| heater.current_temperature.value == Some(26.5))
                    && work(&c).shared_feedback().source == Some(PathKind::Direct)
            })
            .await;

            // Hold an earlier direct group read across the route change. Its
            // late response must not overwrite the cloud result.
            direct.hold_read.store(true, Ordering::Relaxed);
            let reads = direct_methods(&direct, "get_prop").len();
            work(&c).refresh_shared_feedback();
            eventually(|| direct_methods(&direct, "get_prop").len() > reads).await;
            c.context.stop_lan(DID);
            direct.hold_read.store(false, Ordering::Relaxed);
            work(&c).refresh_shared_feedback();
            eventually(|| {
                work(&c).shared_feedback().source == Some(PathKind::Cloud)
                    && heater
                        .snapshot()
                        .heater()
                        .is_some_and(|heater| heater.current_temperature.value == Some(27.0))
            })
            .await;
            assert_eq!(
                blow.snapshot().fan().unwrap().speed.value,
                Some(1),
                "the cloud group's ventilation digit supplies both fans"
            );

            let direct_low = direct_methods(&direct, "set_bh_mode").len();
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
            assert_eq!(direct_methods(&direct, "set_bh_mode").len(), direct_low);
            let status = c
                .context
                .diagnostics()
                .devices
                .into_iter()
                .find(|device| device.did == DID)
                .unwrap();
            assert_eq!(status.selected_control, Some(PathKind::Cloud));
            assert_eq!(
                status
                    .commands
                    .iter()
                    .find(|command| command.channel == BLOW)
                    .and_then(|command| command.last.as_ref())
                    .and_then(|command| command.actual_path),
                Some(PathKind::Cloud)
            );

            direct.values.lock().unwrap()[0] = json!("quick-heat");
            direct.values.lock().unwrap()[1] = json!(31);
            direct.values.lock().unwrap()[2] = json!(28.5);
            let actions = c.cloud.lock().unwrap().action_requests.len();
            c.context.reconcile_lan(DID).unwrap();
            eventually(|| c.context.direct_candidate(DID).is_ok()).await;
            work(&c).refresh_shared_feedback();
            eventually(|| {
                work(&c).shared_feedback().source == Some(PathKind::Direct)
                    && heater
                        .snapshot()
                        .heater()
                        .is_some_and(|heater| heater.current_temperature.value == Some(28.5))
            })
            .await;
            assert_eq!(
                blow.snapshot().fan().unwrap().speed.value,
                Some(2),
                "direct recovery reads a fresh baseline"
            );
            assert_eq!(
                c.cloud.lock().unwrap().action_requests.len(),
                actions,
                "a settled cloud command is never replayed on direct recovery"
            );
        })
        .await;
    }));
}

#[test]
fn an_unsent_direct_low_speed_start_can_use_the_cloud() {
    let (rig, direct) = setup();
    rig.network.lan_timeout.set(Duration::from_millis(200));
    let (rig, listener, cloud) = with_cloud_rig(rig);
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            let blow = function(&c, BLOW);
            eventually(|| {
                c.context.direct_candidate(DID).is_ok()
                    && blow.snapshot().fan().unwrap().speed.value == Some(2)
                    && work(&c).refresh.borrow().snapshot().inflight.is_empty()
            })
            .await;
            c.clock.automatic.store(false, Ordering::Relaxed);
            let client = c.context.direct_candidate(DID).unwrap();
            let requests = direct.requests.clone();
            drop(direct);
            // Losing an unrelated reply expires the client's timestamp. The next
            // operation must handshake before it can send its business datagram.
            assert!(
                Box::pin(client.request_with_timeout(
                    "get_prop",
                    json!(["temperature"]),
                    Duration::from_millis(50)
                ))
                .await
                .is_err()
            );
            assert!(c.context.direct_candidate(DID).is_ok());
            blow.begin_fan(FanCommand::Speed(1))
                .unwrap()
                .accepted()
                .await
                .unwrap();
            assert!(c.context.direct_candidate(DID).is_err());
            assert!(
                requests
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|r| r["method"] != "set_bh_mode")
            );
            assert_eq!(
                c.cloud.lock().unwrap().action_requests.as_slice(),
                &[(DID.into(), 4, 4, json!([]))]
            );
        })
        .await;
    }));
}

#[test]
fn sent_direct_low_speed_timeout_stays_uncertain_and_is_not_replayed_on_cloud() {
    let (rig, direct) = setup();
    rig.network.lan_timeout.set(Duration::from_millis(100));
    let (rig, listener, cloud) = with_cloud_rig(rig);
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            let blow = function(&c, BLOW);
            eventually(|| {
                c.context.direct_candidate(DID).is_ok()
                    && blow
                        .snapshot()
                        .fan()
                        .is_some_and(|fan| fan.speed.value == Some(2))
            })
            .await;
            direct.drop_write.store(true, Ordering::Relaxed);
            assert!(
                blow.begin_fan(FanCommand::Speed(1))
                    .unwrap()
                    .accepted()
                    .await
                    .is_err()
            );
            eventually(|| c.context.direct_candidate(DID).is_err()).await;
            assert_eq!(direct_methods(&direct, "set_bh_mode").len(), 1);
            assert!(
                c.cloud.lock().unwrap().action_requests.is_empty(),
                "a sent direct command with an unknown result must not be replayed on the cloud"
            );
            let status = c
                .context
                .diagnostics()
                .devices
                .into_iter()
                .find(|device| device.did == DID)
                .unwrap();
            let command = status
                .commands
                .iter()
                .find(|command| command.channel == BLOW)
                .unwrap();
            let last = command.last.as_ref().unwrap();
            assert_eq!(last.actual_path, Some(PathKind::Direct));
            assert!(matches!(
                last.steps[0].receipt,
                crate::xiaomi::adapter::StepReceipt::Uncertain(_)
            ));
            async_io::Timer::after(Duration::from_millis(200)).await;
            assert!(c.cloud.lock().unwrap().action_requests.is_empty());
        })
        .await;
    }));
}

#[test]
fn v13_credential_and_address_replacement_moves_standard_and_shared_direct_traffic() {
    use crate::xiaomi::{
        lan::Token,
        mijia::{CloudDeviceInfo, LanCandidate, LanCredentials},
    };

    let rig =
        Rig::custom_lights(&[(DID, MODEL, v13())]).with_stall_timeout(Duration::from_secs(20));
    let scope = rig
        .store
        .devices()
        .load_configuration()
        .unwrap()
        .scope
        .unwrap();
    let old_token = Token::parse("00112233445566778899aabbccddeeff").unwrap();
    let new_token = Token::parse("11223344556677889900aabbccddeeff").unwrap();
    let old = LegacyDevice::bhf_v13(old_token.clone(), DID.parse().unwrap());
    let new = LegacyDevice::bhf_v13(new_token.clone(), DID.parse().unwrap());
    let old_address: std::net::Ipv4Addr = "127.0.0.2".parse().unwrap();
    let new_address: std::net::Ipv4Addr = "127.0.0.3".parse().unwrap();
    rig.store
        .lan()
        .save(&LanRecord {
            scope: scope.clone(),
            did: DID.into(),
            model: MODEL.into(),
            address: old_address,
            token: old_token.clone(),
        })
        .unwrap();
    {
        let mut endpoints = rig.network.lan_endpoints.borrow_mut();
        endpoints.insert(old_address, old.address);
        endpoints.insert(new_address, new.address);
    }
    rig.network.lan_timeout.set(Duration::from_millis(100));
    rig.network
        .discover_at(new.address, Duration::from_millis(20));
    rig.clock.automatic.store(true, Ordering::Relaxed);
    block_on(rig.run(|c| async move {
        let blow = function(&c, BLOW);
        eventually(|| {
            c.context.direct_candidate(DID).is_ok()
                && blow
                    .snapshot()
                    .fan()
                    .is_some_and(|fan| fan.speed.value == Some(2))
        })
        .await;

        let info = CloudDeviceInfo {
            did: DID.into(),
            model: MODEL.into(),
            instance_type: v13()["type"].as_str().unwrap().into(),
            online: true,
            lan: LanCredentials::Candidate(LanCandidate {
                address: new_address,
                token: new_token.clone(),
            }),
        };
        c.context
            .directory
            .borrow_mut()
            .devices
            .get_mut(DID)
            .unwrap()
            .cloud_access = Some(info);
        c.context.changed.notify(usize::MAX);
        eventually(|| {
            c.context
                .directory
                .borrow()
                .devices
                .get(DID)
                .and_then(|slot| slot.lan.active.as_ref())
                .is_some_and(|(record, _)| {
                    record.address == new_address && record.token == new_token
                })
                && blow
                    .snapshot()
                    .fan()
                    .is_some_and(|fan| fan.power.value == Some(true))
        })
        .await;
        assert!(c.context.direct_candidate(DID).is_ok());
        assert!(c.context.legacy_mapping(DID).is_none());

        let old_set = direct_methods(&old, "set_properties").len();
        let new_set = direct_methods(&new, "set_properties").len();
        blow.begin_fan(FanCommand::Power(false))
            .unwrap()
            .accepted()
            .await
            .unwrap();
        eventually(|| direct_methods(&new, "set_properties").len() > new_set).await;
        assert_eq!(direct_methods(&old, "set_properties").len(), old_set);

        let old_shared = direct_methods(&old, "get_prop").len();
        let new_shared = direct_methods(&new, "get_prop").len();
        work(&c).refresh_shared_feedback();
        eventually(|| direct_methods(&new, "get_prop").len() > new_shared).await;
        assert_eq!(direct_methods(&old, "get_prop").len(), old_shared);
        assert_eq!(
            c.context
                .directory
                .borrow()
                .devices
                .get(DID)
                .and_then(|slot| slot.lan.active.as_ref())
                .map(|(record, _)| record.address),
            Some(new_address),
            "the old client is replaced by the current credential/address"
        );
    }));
}

#[test]
fn v13_offline_cold_start_keeps_identity_functions_and_reads_a_new_baseline() {
    let (mut rig, peer) = setup();
    let before = rig.store.devices().load_configuration().unwrap();
    {
        let mut record = rig.store.xiaomi().load().unwrap().unwrap();
        record.ha.authorization.tokens.refresh_at = 1;
        record.ha.authorization.tokens.expires_at = 1;
        rig.store.xiaomi().fixture_replace(&record).unwrap();
    }
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.token_status = 503;
        cloud.home_status = 503;
        cloud.cert_status = 503;
    }
    rig.clock.advance(Duration::from_secs(7200));
    let (restored, ready) = rig.reopen_adapter();
    rig.network.lan_port.set(peer.address.port());
    rig.network.lan_timeout.set(Duration::from_millis(100));
    rig.source = restored;
    rig.ready = ready;
    rig.clock.automatic.store(true, Ordering::Relaxed);
    block_on(rig.run(|c| async move {
        eventually(|| {
            c.context.direct_candidate(DID).is_ok()
                && function(&c, BLOW)
                    .snapshot()
                    .fan()
                    .is_some_and(|fan| fan.speed.value == Some(2))
                && function(&c, HEATER)
                    .snapshot()
                    .heater()
                    .is_some_and(|heater| heater.current_temperature.value == Some(26.5))
        })
        .await;
        let ids = c
            .context
            .directory
            .borrow()
            .devices
            .get(DID)
            .and_then(|slot| slot.admitted.as_ref())
            .unwrap()
            .native
            .device()
            .functions()
            .iter()
            .map(|function| function.id().to_owned())
            .collect::<Vec<_>>();
        for id in ["light:2", HEATER, BLOW, VENTILATION] {
            assert!(ids.iter().any(|candidate| candidate == id), "{ids:?}");
        }
        assert_eq!(
            c.store.devices().load_configuration().unwrap(),
            before,
            "offline restart preserves the committed physical identity and functions"
        );
        let status = c
            .context
            .diagnostics()
            .devices
            .into_iter()
            .find(|device| device.did == DID)
            .unwrap();
        assert_eq!(status.lan.unwrap().state, LanState::Ready);
        assert_eq!(status.selected_control, Some(PathKind::Direct));
        assert_eq!(status.selected_read, Some(PathKind::Direct));
    }));
}

fn status_text(c: &Control) -> String {
    let status = crate::terminal::AuthStatus::new(
        crate::xiaomi::auth::AuthReport::for_test(
            crate::xiaomi::auth::AuthenticationState::NotSignedIn,
            None,
            crate::xiaomi::auth::CertificateUpdate::NotNeeded,
            0,
        ),
        "/bin/migate".into(),
        "/data".into(),
    );
    let terminal = crate::terminal::bridge::Context::committed(c.publication.clone(), &status);
    block_on(terminal.handle_line(&format!("status {DID}"), 0))
        .unwrap()
        .unwrap()
}

#[test]
fn diagnostics_show_shared_current_path_apart_from_the_last_observed_source() {
    let (rig, _peer) = setup();
    block_on(rig.run(|c| async move {
        let heater = function(&c, HEATER);
        eventually(|| {
            c.context.direct_candidate(DID).is_ok()
                && heater
                    .snapshot()
                    .heater()
                    .is_some_and(|heater| heater.current_temperature.value == Some(26.5))
        })
        .await;
        let text = status_text(&c);
        assert!(
            text.contains(
                "Legacy feedback group: candidates direct; selected read direct; last observed direct; settled; pending false."
            ),
            "{text}"
        );

        // Remove both eligible group paths without yielding to a background
        // re-authentication: the current availability must still be visible.
        let direct = c.context
            .directory
            .borrow_mut()
            .devices
            .get_mut(DID)
            .unwrap()
            .lan
            .active
            .take();
        c.context.credentials.borrow_mut().pause_http();
        work(&c).reconcile_state_reads(&c.context);
        assert_eq!(
            heater.snapshot().heater().unwrap().temperature_source,
            TemperatureSourceState::Unavailable
        );
        let text = status_text(&c);
        assert!(
            text.contains(
                "Legacy feedback group: candidates none; selected read none; last observed direct; settled; pending false."
            ),
            "{text}"
        );
        assert!(
            text.contains("Current temperature: unknown (built-in source unavailable)"),
            "{text}"
        );
        assert!(text.contains("Last reading (diagnostic only): 26.50 °C"), "{text}");
        assert!(text.contains("temperature: raw 26.5."), "{text}");

        c.context
            .directory
            .borrow_mut()
            .devices
            .get_mut(DID)
            .unwrap()
            .lan
            .active = direct;
        c.context.credentials.borrow_mut().resume_http();
        work(&c).reconcile_state_reads(&c.context);
        let text = status_text(&c);
        assert!(
            text.contains(
                "Legacy feedback group: candidates direct; selected read direct; last observed direct; settled; pending false."
            ),
            "{text}"
        );
        assert!(text.contains("temperature: raw 26.5."), "{text}");
    }));
}

#[test]
fn v13_direct_heater_and_light_write_only_their_own_properties() {
    let (rig, peer) = setup();
    block_on(rig.run(|c| async move {
        let heater = function(&c, HEATER);
        let light = function(&c, "light:2");
        eventually(|| {
            c.context.direct_candidate(DID).is_ok()
                && heater
                    .snapshot()
                    .heater()
                    .is_some_and(|heater| heater.power.value == Some(false))
        })
        .await;

        heater
            .begin_heater(crate::device::HeaterCommand::Power(true))
            .unwrap()
            .accepted()
            .await
            .unwrap();
        heater
            .begin_heater(crate::device::HeaterCommand::Power(false))
            .unwrap()
            .accepted()
            .await
            .unwrap();
        light.execute(crate::device::Command::On).await.unwrap();
        light.execute(crate::device::Command::Off).await.unwrap();
        heater
            .begin_heater(crate::device::HeaterCommand::Temperature(30))
            .unwrap()
            .accepted()
            .await
            .unwrap();

        let writes = peer
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request["method"] == "set_properties")
            .flat_map(|request| request["params"].as_array().unwrap().iter().cloned())
            .map(|item| {
                (
                    item["siid"].as_u64().unwrap() as u32,
                    item["piid"].as_u64().unwrap() as u32,
                    item["value"].clone(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            writes,
            vec![
                (3, 3, json!(true)),
                (3, 3, json!(false)),
                (2, 1, json!(true)),
                (2, 1, json!(false)),
                (3, 8, json!(30)),
            ]
        );
        assert!(
            c.cloud.lock().unwrap().numeric_writes.is_empty(),
            "ready direct standard properties must not also write the cloud"
        );
    }));
}

#[test]
fn v13_scope_revocation_stops_direct_without_deleting_saved_identity_or_functions() {
    let (rig, _peer) = setup();
    let before = rig.store.devices().load_configuration().unwrap();
    block_on(rig.run(|c| async move {
        eventually(|| c.context.direct_candidate(DID).is_ok()).await;
        c.context.invalidate_lan_scope().unwrap();
        eventually(|| {
            c.context.direct_candidate(DID).is_err()
                && c.store
                    .lan()
                    .load(
                        &c.store
                            .devices()
                            .load_configuration()
                            .unwrap()
                            .scope
                            .unwrap(),
                        DID,
                    )
                    .unwrap()
                    .is_none()
        })
        .await;
        assert_eq!(c.store.devices().load_configuration().unwrap(), before);
        let ids = c
            .context
            .directory
            .borrow()
            .devices
            .get(DID)
            .and_then(|slot| slot.admitted.as_ref())
            .unwrap()
            .native
            .device()
            .functions()
            .iter()
            .map(|function| function.id().to_owned())
            .collect::<Vec<_>>();
        for id in ["light:2", HEATER, BLOW, VENTILATION] {
            assert!(ids.iter().any(|candidate| candidate == id), "{ids:?}");
        }
        let status = c
            .context
            .diagnostics()
            .devices
            .into_iter()
            .find(|device| device.did == DID)
            .unwrap();
        assert!(status.direct_unavailable_reason.is_some());
        assert_eq!(status.selected_control, None);
    }));
}
