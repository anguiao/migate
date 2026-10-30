use super::*;
use crate::device::{
    AirConditionerCommand as Command, AirConditionerFanSpeed as Speed, AirConditionerMode as Mode,
    AirConditionerSnapshot, AirConditionerTarget, CommandError, TemperatureGrid,
    air_conditioner::AirConditionerConstraints,
};
use crate::xiaomi::capability::PropertyBinding;
use futures_lite::future::poll_once;

const POWER: PropertyBinding = PropertyBinding { siid: 3, piid: 1 };
const MODE: PropertyBinding = PropertyBinding { siid: 3, piid: 2 };
const TEMP: PropertyBinding = PropertyBinding { siid: 3, piid: 4 };
const SPEED: PropertyBinding = PropertyBinding { siid: 4, piid: 2 };
const SWING: PropertyBinding = PropertyBinding { siid: 4, piid: 4 };
const ID: &str = "air-conditioner:3";

fn rig() -> Rig {
    let instance = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/lumi-mcn04.json"
    ))
    .unwrap();
    Rig::custom_lights(&[("one", "generic.ac.model", instance)])
}
fn work(c: &Control) -> Rc<super::work::DeviceWork> {
    c.context.live.borrow().devices["one"]
        .admitted
        .as_ref()
        .unwrap()
        .clone()
}
fn snapshot(c: &Control) -> AirConditionerSnapshot {
    function(&c.context.registry, "one", ID)
        .snapshot()
        .air_conditioner()
        .unwrap()
}
fn peer() -> Rc<Peer> {
    let peer = Rc::new(Peer::default());
    for (binding, value) in [
        (POWER, json!(false)),
        (MODE, json!(0)),
        (TEMP, json!(24.0)),
        (SPEED, json!(0)),
        (SWING, json!(false)),
    ] {
        peer.value("one", binding, value);
    }
    peer
}
fn writes(peer: &Peer) -> Vec<(Value, Value, Value)> {
    peer.state
        .borrow()
        .seen
        .iter()
        .filter(|(op, value)| op == "rpcReq" && value["did"] == "one")
        .map(|(_, value)| {
            let item = &value["rpc"]["params"][0];
            (
                item["siid"].clone(),
                item["piid"].clone(),
                item["value"].clone(),
            )
        })
        .collect()
}
async fn accept(peer: &Peer, binding: PropertyBinding) {
    peer.release_property(
        "one",
        binding,
        json!({"result":[{"did":"one","siid":binding.siid,"piid":binding.piid,"code":0}]}),
    )
    .await;
}

fn restrictions(cool_temperatures: Vec<i16>) -> AirConditionerConstraints {
    AirConditionerConstraints::new(
        TemperatureGrid::new(16, 30, 1).unwrap(),
        [
            (
                Mode::Cool,
                cool_temperatures,
                vec![Speed::Auto, Speed::Low, Speed::High],
            ),
            (Mode::Heat, vec![24, 26], vec![Speed::Low, Speed::High]),
            (Mode::Auto, vec![], vec![Speed::Auto, Speed::Low]),
            (Mode::Dry, vec![], vec![Speed::Low]),
        ],
    )
    .unwrap()
}

fn sent_temperature_settles_after_feedback_change(change_mode: bool) {
    block_on(rig().run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(peer(), |peer| async move {
            until(|| {
                snapshot(control).mode.value == Some(Mode::Cool)
                    && snapshot(control).temperature.value == Some(24)
                    && work(control).function_observer(ID).is_some()
            })
            .await;
            let function = function(&control.context.registry, "one", ID);
            function.set_air_conditioner_constraints(Some(restrictions(vec![24, 26])));
            peer.state.borrow_mut().hold_writes = true;
            let action = function
                .begin_air_conditioner(Command::Temperature(26))
                .unwrap();
            until(|| writes(&peer).len() == 1).await;
            assert_eq!(writes(&peer)[0], (json!(3), json!(4), json!(26.0)));
            if change_mode {
                peer.notify_value_at("one", MODE, json!(2), Some(901), false);
                until(|| snapshot(control).mode.value == Some(Mode::Auto)).await;
            } else {
                function.set_air_conditioner_constraints(Some(restrictions(vec![24])));
            }
            accept(&peer, TEMP).await;
            action.accepted().await.unwrap();
            assert_eq!(snapshot(control).temperature.value, Some(24));
            assert_eq!(writes(&peer).len(), 1);
        })
        .await;
    }));
}

#[test]
fn sent_temperature_keeps_successful_receipt_after_mode_feedback() {
    sent_temperature_settles_after_feedback_change(true);
}

#[test]
fn sent_temperature_keeps_successful_receipt_after_range_update() {
    sent_temperature_settles_after_feedback_change(false);
}

#[test]
fn feedback_before_mode_receipt_restricts_the_unsent_combination_step() {
    block_on(rig().run(|c| async move {
        c.announce();
        let control = &c;
        let peer = peer();
        peer.value("one", POWER, json!(true));
        c.peer(peer, |peer| async move {
            until(|| {
                snapshot(control).power.value == Some(true)
                    && snapshot(control).mode.value == Some(Mode::Cool)
                    && work(control).function_observer(ID).is_some()
            })
            .await;
            let function = function(&control.context.registry, "one", ID);
            function.set_air_conditioner_constraints(Some(restrictions(vec![24, 26])));
            peer.state.borrow_mut().hold_writes = true;
            let action = function
                .begin_air_conditioner(Command::Combined(AirConditionerTarget {
                    mode: Some(Mode::Heat),
                    temperature: Some(24),
                    speed: Some(Speed::High),
                    swing: None,
                }))
                .unwrap();
            until(|| writes(&peer).len() == 1).await;
            assert_eq!(writes(&peer)[0], (json!(3), json!(2), json!(1)));
            peer.notify_value_at("one", MODE, json!(2), Some(902), false);
            until(|| snapshot(control).mode.value == Some(Mode::Auto)).await;
            accept(&peer, MODE).await;
            let result = future::or(action.accepted(), async {
                until(|| writes(&peer).len() > 1).await;
                Err(CommandError::SendFailed("unexpected next step".into()))
            })
            .await;
            assert_eq!(result, Err(CommandError::InvalidInState));
            assert_eq!(writes(&peer).len(), 1);
        })
        .await;
    }));
}

#[test]
fn off_still_interrupts_an_in_flight_temperature_and_waits_for_its_receipt() {
    block_on(rig().run(|c| async move {
        c.announce();
        let control = &c;
        let peer = peer();
        peer.value("one", POWER, json!(true));
        c.peer(peer, |peer| async move {
            until(|| {
                snapshot(control).power.value == Some(true)
                    && snapshot(control).mode.value == Some(Mode::Cool)
                    && work(control).function_observer(ID).is_some()
            })
            .await;
            let function = function(&control.context.registry, "one", ID);
            let original = function.snapshot();
            peer.state.borrow_mut().hold_writes = true;
            let temperature = function
                .begin_air_conditioner(Command::Temperature(26))
                .unwrap();
            until(|| writes(&peer).len() == 1).await;
            let off = function
                .begin_air_conditioner(Command::Power(false))
                .unwrap();
            assert_eq!(temperature.accepted().await, Err(CommandError::Superseded));
            for _ in 0..30 {
                future::yield_now().await;
            }
            assert_eq!(writes(&peer).len(), 1);
            accept(&peer, TEMP).await;
            until(|| writes(&peer).len() == 2).await;
            assert_eq!(writes(&peer)[1], (json!(3), json!(1), json!(false)));
            accept(&peer, POWER).await;
            off.accepted().await.unwrap();
            assert_eq!(function.snapshot(), original);
        })
        .await;
    }));
}

#[test]
fn local_standard_reads_notifications_and_ordered_writes_keep_receipts_separate() {
    block_on(rig().run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(peer(), |peer| async move {
            until(|| {
                let ac = snapshot(control);
                ac.power.value == Some(false)
                    && ac.mode.value == Some(Mode::Cool)
                    && ac.temperature.value == Some(24)
                    && ac.speed.value == Some(Speed::Auto)
                    && ac.swing.value == Some(false)
                    && work(control).function_observer(ID).is_some()
            })
            .await;
            for binding in [POWER, MODE, TEMP, SPEED, SWING] {
                assert!(
                    work(control)
                        .subscription_status(ID)
                        .iter()
                        .any(|status| status.source == PathKind::Local
                            && status.binding
                                == super::model::SubscriptionBinding::Property(binding)
                            && status.ready)
                );
            }
            let function = function(&control.context.registry, "one", ID);
            let before = function.snapshot();
            peer.state.borrow_mut().hold_writes = true;
            let action = function
                .begin_air_conditioner(Command::Combined(AirConditionerTarget {
                    mode: Some(Mode::Heat),
                    temperature: Some(26),
                    speed: Some(Speed::Low),
                    swing: Some(true),
                }))
                .unwrap();
            let expected = [
                (POWER, json!(true)),
                (MODE, json!(1)),
                (TEMP, json!(26.0)),
                (SPEED, json!(1)),
                (SWING, json!(true)),
            ];
            for (index, (binding, value)) in expected.into_iter().enumerate() {
                until(|| writes(&peer).len() == index + 1).await;
                assert_eq!(
                    writes(&peer)[index],
                    (json!(binding.siid), json!(binding.piid), value)
                );
                assert!(poll_once(action.accepted()).await.is_none());
                accept(&peer, binding).await;
            }
            action.accepted().await.unwrap();
            assert_eq!(function.snapshot(), before);
            peer.notify_value_at("one", MODE, json!(2), Some(700), false);
            peer.notify_value_at("one", SPEED, json!(3), Some(701), false);
            until(|| {
                snapshot(control).mode.value == Some(Mode::Auto)
                    && snapshot(control).speed.value == Some(Speed::High)
            })
            .await;
            assert_eq!(snapshot(control).mode.source_time, Some(700));
            assert_eq!(snapshot(control).speed.source_time, Some(701));
            let status = crate::terminal::AuthStatus::new(
                control.context.status.borrow().authentication.clone(),
                "/bin/migate".into(),
                "/data".into(),
            );
            status.update_adapter(control.context.diagnostics());
            let terminal =
                crate::terminal::bridge::Context::committed(control.publication.clone(), &status);
            let text = terminal
                .handle_line("status one", 0)
                .await
                .unwrap()
                .unwrap();
            assert!(
                text.contains("Mode")
                    && text.contains("Auto")
                    && text.contains("Temperature")
                    && text.contains("Speed"),
                "{text}"
            );
            assert!(text.contains("local"), "{text}");
            peer.notify_value_at("one", POWER, json!(true), Some(702), false);
            peer.notify_value_at("one", SPEED, json!(0), Some(703), false);
            until(|| {
                snapshot(control).power.value == Some(true)
                    && snapshot(control).speed.value == Some(Speed::Auto)
            })
            .await;
            let text = terminal
                .handle_line("status one", 0)
                .await
                .unwrap()
                .unwrap();
            assert!(
                text.contains("Current fan level: unknown (automatic speed)"),
                "{text}"
            );
            peer.state.borrow_mut().hold_writes = false;
            assert_eq!(
                terminal
                    .handle_line("off one air-conditioner:3", 0)
                    .await
                    .unwrap()
                    .unwrap(),
                "one: command accepted"
            );
            assert_eq!(
                terminal
                    .handle_line("on one air-conditioner:3", 0)
                    .await
                    .unwrap()
                    .unwrap(),
                "one: command accepted"
            );
            assert_eq!(
                writes(&peer)[5..],
                [
                    (json!(3), json!(1), json!(false)),
                    (json!(3), json!(1), json!(true))
                ]
            );
        })
        .await;
    }));
}

#[test]
fn cloud_reads_writes_and_notifications_use_the_same_standard_bindings() {
    let rig = rig();
    {
        let mut cloud = rig.cloud.lock().unwrap();
        for (binding, value) in [
            (POWER, json!(false)),
            (MODE, json!(0)),
            (TEMP, json!(24.0)),
            (SPEED, json!(0)),
            (SWING, json!(false)),
        ] {
            cloud
                .raw_values
                .insert(("one".into(), binding.siid, binding.piid), value);
        }
    }
    let (rig, listener, cloud) = with_cloud_rig(rig);
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            until(|| {
                let ac = snapshot(&c);
                ac.power.value == Some(false)
                    && ac.mode.value == Some(Mode::Cool)
                    && ac.temperature.value == Some(24)
                    && ac.speed.value == Some(Speed::Auto)
                    && work(&c).function_observer(ID).is_some()
            })
            .await;
            let function = function(&c.context.registry, "one", ID);
            let before = function.snapshot();
            let action = function
                .begin_air_conditioner(Command::Speed(Speed::Auto))
                .unwrap();
            action.accepted().await.unwrap();
            let actual = c
                .cloud
                .lock()
                .unwrap()
                .numeric_writes
                .iter()
                .map(|(items, _)| {
                    let item = &items[0];
                    (
                        item["siid"].clone(),
                        item["piid"].clone(),
                        item["value"].clone(),
                    )
                })
                .collect::<Vec<_>>();
            assert_eq!(
                actual,
                [
                    (json!(3), json!(1), json!(true)),
                    (json!(4), json!(2), json!(0))
                ]
            );
            assert_eq!(function.snapshot(), before);
            cloud.property_at("one", 3, 4, json!(28.0));
            until(|| snapshot(&c).temperature.value == Some(28)).await;
        })
        .await;
    }));
}

#[test]
fn replacement_source_rebuilds_baseline_and_rejects_retired_notifications() {
    block_on(rig().run(|c| async move {
        c.announce();
        let old = Rc::new(RefCell::new(None));
        let captured = old.clone();
        let control = &c;
        c.peer(peer(), |_| async move {
            until(|| {
                snapshot(control).mode.value == Some(Mode::Cool)
                    && work(control).function_observer(ID).is_some()
            })
            .await;
            *captured.borrow_mut() = work(control).function_observer(ID);
        })
        .await;
        until(|| c.context.live.borrow().session.is_none()).await;
        let replacement = peer();
        replacement.value("one", MODE, json!(1));
        replacement.value("one", TEMP, json!(26.0));
        replacement.value("one", SPEED, json!(1));
        let control = &c;
        c.peer(replacement, |peer| async move {
            until(|| {
                snapshot(control).mode.value == Some(Mode::Heat)
                    && snapshot(control).temperature.value == Some(26)
                    && snapshot(control).speed.value == Some(Speed::Low)
                    && work(control).function_observer(ID).is_some()
            })
            .await;
            let observed = snapshot(control);
            control.context.property_value_event(
                old.borrow().as_ref().unwrap(),
                "one",
                MODE,
                &json!(4),
                Some(999),
                false,
            );
            assert_eq!(snapshot(control), observed);
            work(control).retire();
            peer.notify_value_at("one", TEMP, json!(30.0), Some(1000), false);
            for _ in 0..50 {
                future::yield_now().await;
            }
            assert_eq!(snapshot(control), observed);
        })
        .await;
    }));
}

#[test]
fn mcn04_state_report_updates_valid_fields_atomically_and_preserves_invalid_fields() {
    let instance = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/lumi-mcn04.json"
    ))
    .unwrap();
    let rig = Rig::custom_lights(&[("one", "lumi.acpartner.mcn04", instance)]);
    let state = PropertyBinding { siid: 8, piid: 7 };
    let range = PropertyBinding { siid: 8, piid: 6 };
    block_on(rig.run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        peer.value("one", state, json!("P0_M0_T24_S1_D1"));
        peer.value(
            "one",
            range,
            json!(
                "{\"models\":[{\"mode\":0,\"temps\":[24,26],\"speeds\":[0,1,2,3],\"directs\":[1]}]}"
            ),
        );
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| snapshot(control).power.value == Some(true)).await;
            let function = function(&control.context.registry, "one", ID);
            let before = function.snapshot().revision;
            peer.notify_value_at("one", state, json!("P1_M1_T999_S3_D0"), Some(777), false);
            until(|| snapshot(control).mode.value == Some(Mode::Heat)).await;
            let after = function.snapshot();
            assert_eq!(after.revision, before + 1);
            let observed = after.air_conditioner().unwrap();
            assert_eq!(observed.power.value, Some(false));
            assert_eq!(observed.mode.value, Some(Mode::Heat));
            assert_eq!(observed.temperature.value, Some(24));
            assert_eq!(observed.speed.value, Some(Speed::High));
            assert_eq!(observed.swing.value, Some(true));
            assert_eq!(observed.power.source_time, Some(777));
            assert_eq!(observed.speed.source_time, Some(777));
            assert!(
                work(control)
                    .mcn04_problem(ID, state)
                    .unwrap()
                    .contains("T")
            );
        })
        .await;
    }));
}

#[test]
fn mcn04_range_is_unknown_until_a_source_reports_it() {
    let instance = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/lumi-mcn04.json"
    ))
    .unwrap();
    let rig = Rig::custom_lights(&[("one", "lumi.acpartner.mcn04", instance)]);
    block_on(rig.run(|c| async move {
        let function = function(&c.context.registry, "one", ID);
        assert!(function.air_conditioner_constraints().is_none());
    }));
}

const MCN_STATE: PropertyBinding = PropertyBinding { siid: 8, piid: 7 };
const MCN_RANGE: PropertyBinding = PropertyBinding { siid: 8, piid: 6 };

fn mcn04_rig() -> Rig {
    let instance = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/lumi-mcn04.json"
    ))
    .unwrap();
    Rig::custom_lights(&[("one", "lumi.acpartner.mcn04", instance)])
}
fn mcn04_range() -> Value {
    json!(
        json!({"models":[
            {"mode":0,"speeds":[0,1,2,3,99],"temps":[16,24,26,30,99],"directs":[1],"type":2},
            {"mode":1,"speeds":[0,1,2,3],"temps":[24,26],"directs":[1],"type":2},
            {"mode":2,"speeds":[0,1,2,3],"temps":[],"directs":[1],"type":2},
            {"mode":3,"speeds":[0,1,2,3],"temps":[24,26],"directs":[1],"type":2},
            {"mode":4,"speeds":[1],"temps":[24,26],"directs":[1],"type":2}
        ]})
        .to_string()
    )
}
fn mcn04_peer() -> Rc<Peer> {
    let peer = Rc::new(Peer::default());
    peer.value("one", MCN_STATE, json!("P0_M0_T24_S1_D1"));
    peer.value("one", MCN_RANGE, mcn04_range());
    peer
}

#[test]
fn mcn04_range_controls_commands_and_invalid_reports_preserve_last_valid_range() {
    block_on(mcn04_rig().run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(mcn04_peer(), |peer| async move {
            let function = function(&control.context.registry, "one", ID);
            until(|| {
                function.air_conditioner_constraints_ready()
                    && snapshot(control).mode.value == Some(Mode::Cool)
            })
            .await;
            let constraints = function.air_conditioner_constraints().unwrap();
            assert_eq!(
                constraints.temperatures(Mode::Cool),
                Some([16, 24, 26, 30].as_slice())
            );
            assert_eq!(
                constraints.speeds(Mode::Cool),
                Some([Speed::Auto, Speed::Low, Speed::Medium, Speed::High].as_slice())
            );
            assert_eq!(constraints.temperatures(Mode::Auto), Some([].as_slice()));
            assert_eq!(constraints.speeds(Mode::Dry), Some([Speed::Low].as_slice()));
            let statuses = work(control).subscription_status(ID);
            for binding in [MCN_STATE, MCN_RANGE] {
                assert!(
                    statuses
                        .iter()
                        .any(|status| status.source == PathKind::Local
                            && status.binding
                                == super::model::SubscriptionBinding::Property(binding)
                            && status.ready)
                );
            }
            for binding in [POWER, MODE, TEMP, SPEED, SWING] {
                assert!(
                    !statuses.iter().any(|status| status.binding
                        == super::model::SubscriptionBinding::Property(binding))
                );
            }
            let before = function.snapshot();
            function
                .begin_air_conditioner(Command::Temperature(26))
                .unwrap()
                .accepted()
                .await
                .unwrap();
            assert_eq!(writes(&peer)[0], (json!(3), json!(4), json!(26)));
            assert_eq!(function.snapshot(), before);
            peer.notify_value_at("one", MCN_RANGE, json!("not json"), Some(900), false);
            until(|| work(control).mcn04_problem(ID, MCN_RANGE).is_some()).await;
            assert_eq!(function.air_conditioner_constraints().unwrap(), constraints);
            assert!(function.air_conditioner_constraints_ready());
            assert!(work(control).mcn04_range(ID).unwrap().contains("directs"));
            let status = crate::terminal::AuthStatus::new(
                control.context.status.borrow().authentication.clone(),
                "/bin/migate".into(),
                "/data".into(),
            );
            status.update_adapter(control.context.diagnostics());
            let terminal =
                crate::terminal::bridge::Context::committed(control.publication.clone(), &status);
            let output = terminal
                .handle_line("status one", 0)
                .await
                .unwrap()
                .unwrap();
            assert!(output.contains("property 8.7"), "{output}");
            assert!(
                output.contains("Control range source: property 8.6"),
                "{output}"
            );
            assert!(
                output.contains("ac-ctrl-range") && output.contains("directs"),
                "{output}"
            );
            peer.notify_value_at("one", MCN_STATE, json!("P0_M2_T24_S1_D1"), Some(901), false);
            until(|| snapshot(control).mode.value == Some(Mode::Auto)).await;
            assert_eq!(
                function
                    .begin_air_conditioner(Command::Temperature(24))
                    .unwrap()
                    .accepted()
                    .await,
                Err(CommandError::InvalidInState)
            );
            peer.notify_value_at("one", MCN_STATE, json!("P0_M4_T24_S1_D1"), Some(902), false);
            until(|| snapshot(control).mode.value == Some(Mode::Dry)).await;
            assert_eq!(
                function
                    .begin_air_conditioner(Command::Speed(Speed::High))
                    .unwrap()
                    .accepted()
                    .await,
                Err(CommandError::InvalidInState)
            );
            function
                .begin_air_conditioner(Command::Speed(Speed::Low))
                .unwrap()
                .accepted()
                .await
                .unwrap();
        })
        .await;
    }));
}

#[test]
fn mcn04_without_range_allows_power_and_swing_but_not_dependent_commands() {
    block_on(mcn04_rig().run(|c| async move {
        c.announce();
        let peer = mcn04_peer();
        peer.state
            .borrow_mut()
            .read_fail
            .insert(("one".into(), MCN_RANGE.siid, MCN_RANGE.piid));
        let control = &c;
        c.peer(peer, |peer| async move {
            let function = function(&control.context.registry, "one", ID);
            until(|| {
                snapshot(control).mode.value == Some(Mode::Cool)
                    && work(control).function_observer(ID).is_some()
            })
            .await;
            assert!(!function.air_conditioner_constraints_ready());
            assert_eq!(
                function
                    .begin_air_conditioner(Command::Mode(Mode::Heat))
                    .unwrap()
                    .accepted()
                    .await,
                Err(CommandError::UnknownState)
            );
            function
                .begin_air_conditioner(Command::Power(false))
                .unwrap()
                .accepted()
                .await
                .unwrap();
            function
                .begin_air_conditioner(Command::Swing(true))
                .unwrap()
                .accepted()
                .await
                .unwrap();
            assert_eq!(
                writes(&peer),
                vec![
                    (json!(3), json!(1), json!(false)),
                    (json!(4), json!(4), json!(true))
                ]
            );
        })
        .await;
    }));
}

#[test]
fn mcn04_old_read_and_retained_on_do_not_release_off_barrier() {
    block_on(mcn04_rig().run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(mcn04_peer(), |peer| async move {
            let function = function(&control.context.registry, "one", ID);
            until(|| {
                function.air_conditioner_constraints_ready()
                    && snapshot(control).power.value == Some(true)
            })
            .await;
            peer.state.borrow_mut().read_timeout.insert((
                "one".into(),
                MCN_STATE.siid,
                MCN_STATE.piid,
            ));
            work(control).refresh_property(ID, MCN_STATE);
            until(|| {
                peer.state.borrow().held.iter().any(|(publish, _)| {
                    let payload = decode_mips(&publish.payload).unwrap().payload;
                    payload["siid"] == 8 && payload["piid"] == 7
                })
            })
            .await;
            function
                .begin_air_conditioner(Command::Power(false))
                .unwrap()
                .accepted()
                .await
                .unwrap();
            peer.release_property(
                "one",
                MCN_STATE,
                json!({"value":"P0_M0_T30_S3_D0","updateTime":1}),
            )
            .await;
            until(|| !work(control).refresh.borrow().contains(ID, MCN_STATE)).await;
            assert_eq!(snapshot(control).temperature.value, Some(24));
            peer.notify_value_at("one", MCN_STATE, json!("P0_M0_T24_S1_D1"), Some(2), true);
            for _ in 0..100 {
                future::yield_now().await;
            }
            function
                .begin_air_conditioner(Command::Speed(Speed::High))
                .unwrap()
                .accepted()
                .await
                .unwrap();
            assert_eq!(
                writes(&peer),
                vec![
                    (json!(3), json!(1), json!(false)),
                    (json!(3), json!(1), json!(true)),
                    (json!(4), json!(2), json!(3))
                ]
            );
        })
        .await;
    }));
}

#[test]
fn mcn04_cloud_read_notification_and_standard_writes() {
    let rig = mcn04_rig();
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud
            .raw_values
            .insert(("one".into(), 8, 7), json!("P1_M0_T24_S1_D1"));
        cloud.raw_values.insert(("one".into(), 8, 6), mcn04_range());
    }
    let (rig, listener, cloud) = with_cloud_rig(rig);
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            let function = function(&c.context.registry, "one", ID);
            until(|| {
                function.air_conditioner_constraints_ready()
                    && snapshot(&c).power.value == Some(false)
                    && work(&c).function_observer(ID).is_some()
            })
            .await;
            let before = function.snapshot();
            function
                .begin_air_conditioner(Command::Temperature(26))
                .unwrap()
                .accepted()
                .await
                .unwrap();
            assert_eq!(function.snapshot(), before);
            let writes = c
                .cloud
                .lock()
                .unwrap()
                .numeric_writes
                .iter()
                .map(|(items, _)| items[0].clone())
                .collect::<Vec<_>>();
            assert_eq!(writes[0]["siid"], 3);
            assert_eq!(writes[0]["piid"], 4);
            assert_eq!(writes[0]["value"], json!(26));
            cloud.property_at("one", 8, 7, json!("P0_M1_T26_S3_D0"));
            until(|| snapshot(&c).mode.value == Some(Mode::Heat)).await;
            assert_eq!(snapshot(&c).temperature.value, Some(26));
            assert_eq!(snapshot(&c).swing.value, Some(true));
        })
        .await;
    }));
}

#[test]
fn mcn04_replacement_source_reloads_state_and_range_and_rejects_old_notifications() {
    block_on(mcn04_rig().run(|c| async move {
        c.announce();
        let old = Rc::new(RefCell::new(None));
        let captured = old.clone();
        let control = &c;
        c.peer(mcn04_peer(), |_| async move {
            let function = function(&control.context.registry, "one", ID);
            until(|| {
                function.air_conditioner_constraints_ready()
                    && snapshot(control).mode.value == Some(Mode::Cool)
                    && work(control).function_observer(ID).is_some()
            })
            .await;
            *captured.borrow_mut() = work(control).function_observer(ID);
        })
        .await;
        until(|| c.context.live.borrow().session.is_none()).await;
        let initial = function(&c.context.registry, "one", ID);
        assert!(!initial.air_conditioner_constraints_ready());
        let replacement = mcn04_peer();
        replacement.value("one", MCN_STATE, json!("P0_M1_T26_S3_D0"));
        let control = &c;
        c.peer(replacement, |_| async move {
            let function = function(&control.context.registry, "one", ID);
            until(|| {
                function.air_conditioner_constraints_ready()
                    && snapshot(control).mode.value == Some(Mode::Heat)
                    && work(control).function_observer(ID).is_some()
            })
            .await;
            let before = snapshot(control);
            let constraints = function.air_conditioner_constraints();
            for (binding, value) in [
                (MCN_STATE, json!("P1_M4_T30_S0_D1")),
                (MCN_RANGE, json!("{\"models\":[]}")),
            ] {
                control.context.property_value_event(
                    old.borrow().as_ref().unwrap(),
                    "one",
                    binding,
                    &value,
                    Some(999),
                    false,
                );
            }
            assert_eq!(snapshot(control), before);
            assert_eq!(function.air_conditioner_constraints(), constraints);
        })
        .await;
    }));
}

#[test]
fn mcn04_retained_report_cannot_replace_fresh_state_or_range_diagnostic() {
    block_on(mcn04_rig().run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(mcn04_peer(), |_| async move {
            let function = function(&control.context.registry, "one", ID);
            until(|| {
                function.air_conditioner_constraints_ready()
                    && snapshot(control).mode.value == Some(Mode::Cool)
                    && work(control).function_observer(ID).is_some()
            })
            .await;
            let source = work(control).function_observer(ID).unwrap();
            let before = snapshot(control);
            control.context.property_value_event(
                &source,
                "one",
                MCN_RANGE,
                &json!("invalid"),
                Some(1),
                false,
            );
            assert!(work(control).mcn04_problem(ID, MCN_RANGE).is_some());
            control.context.property_value_event(
                &source,
                "one",
                MCN_STATE,
                &json!("P1_M1_T30_S3_D0"),
                Some(2),
                true,
            );
            control.context.property_value_event(
                &source,
                "one",
                MCN_RANGE,
                &mcn04_range(),
                Some(2),
                true,
            );
            assert_eq!(snapshot(control), before);
            assert!(work(control).mcn04_problem(ID, MCN_RANGE).is_some());
        })
        .await;
    }));
}

#[test]
fn mcn04_new_range_notification_wins_over_old_inflight_read() {
    block_on(mcn04_rig().run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(mcn04_peer(), |peer| async move {
            let function = function(&control.context.registry, "one", ID);
            until(|| {
                function.air_conditioner_constraints_ready()
                    && work(control).function_observer(ID).is_some()
            })
            .await;
            peer.state.borrow_mut().read_timeout.insert((
                "one".into(),
                MCN_RANGE.siid,
                MCN_RANGE.piid,
            ));
            work(control).refresh_property(ID, MCN_RANGE);
            until(|| {
                peer.state.borrow().held.iter().any(|(publish, _)| {
                    let payload = decode_mips(&publish.payload).unwrap().payload;
                    payload["siid"] == 8 && payload["piid"] == 6
                })
            })
            .await;
            let reordered = json!(
                json!({"models":[
                    {"mode":4,"speeds":[1],"temps":[26],"directs":[9]},
                    {"mode":0,"speeds":[3],"temps":[24],"directs":[8]}
                ]})
                .to_string()
            );
            peer.notify_value_at("one", MCN_RANGE, reordered, Some(50), false);
            until(|| {
                function
                    .air_conditioner_constraints()
                    .unwrap()
                    .speeds(Mode::Cool)
                    == Some([Speed::High].as_slice())
            })
            .await;
            peer.release_property(
                "one",
                MCN_RANGE,
                json!({"value":mcn04_range(),"updateTime":1}),
            )
            .await;
            until(|| !work(control).refresh.borrow().contains(ID, MCN_RANGE)).await;
            let current = function.air_conditioner_constraints().unwrap();
            assert_eq!(current.speeds(Mode::Cool), Some([Speed::High].as_slice()));
            assert_eq!(current.temperatures(Mode::Dry), Some([26].as_slice()));
            assert_eq!(current.temperatures(Mode::Heat), None);
        })
        .await;
    }));
}

#[test]
fn mcn04_combined_steps_recheck_new_feedback_after_mode_send() {
    block_on(mcn04_rig().run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(mcn04_peer(), |peer| async move {
            let function = function(&control.context.registry, "one", ID);
            until(|| {
                function.air_conditioner_constraints_ready()
                    && snapshot(control).mode.value == Some(Mode::Cool)
            })
            .await;
            peer.state.borrow_mut().hold_writes = true;
            let action = function
                .begin_air_conditioner(Command::Combined(AirConditionerTarget {
                    mode: Some(Mode::Heat),
                    temperature: Some(24),
                    speed: Some(Speed::High),
                    swing: None,
                }))
                .unwrap();
            until(|| writes(&peer).len() == 1).await;
            assert_eq!(writes(&peer)[0], (json!(3), json!(2), json!(1)));
            peer.notify_value_at("one", MCN_STATE, json!("P0_M2_T24_S1_D1"), Some(60), false);
            until(|| snapshot(control).mode.value == Some(Mode::Auto)).await;
            accept(&peer, MODE).await;
            assert_eq!(action.accepted().await, Err(CommandError::InvalidInState));
            assert_eq!(writes(&peer).len(), 1);
        })
        .await;
    }));
}
