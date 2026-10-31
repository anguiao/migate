use super::*;

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
                    && ac.swing.value == Some(false)
                    && work(&c).function_observer(ID).is_some()
                    && !work(&c).refresh.borrow().contains(ID, MODE)
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
fn retained_mode_keeps_recovery_reads_and_commands_require_the_fresh_mode() {
    block_on(rig().run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(peer(), |_| async move {
            until(|| {
                snapshot(control).mode.value == Some(Mode::Cool)
                    && work(control).function_observer(ID).is_some()
            })
            .await;
        })
        .await;
        until(|| c.context.live.borrow().session.is_none()).await;

        let replacement = peer();
        replacement.value("one", POWER, json!(true));
        replacement.value("one", MODE, json!(1));
        for binding in [POWER, MODE] {
            replacement.state.borrow_mut().read_timeout.insert((
                "one".into(),
                binding.siid,
                binding.piid,
            ));
        }
        let control = &c;
        c.peer(replacement, |peer| async move {
            let held_read = |binding: PropertyBinding| {
                peer.state.borrow().held.iter().any(|(publish, _)| {
                    let payload = decode_mips(&publish.payload).unwrap().payload;
                    payload["siid"] == binding.siid && payload["piid"] == binding.piid
                })
            };
            until(|| held_read(POWER) && work(control).function_observer(ID).is_some()).await;
            let source = work(control).function_observer(ID).unwrap();
            control
                .context
                .property_value_event(&source, "one", MODE, &json!(2), Some(1), true);
            assert_eq!(snapshot(control).mode.value, Some(Mode::Auto));
            assert!(work(control).refresh.borrow().contains(ID, MODE));

            peer.release_property("one", POWER, json!({"value":true,"updateTime":2}))
                .await;
            until(|| held_read(MODE)).await;
            control
                .context
                .property_value_event(&source, "one", MODE, &json!(4), Some(3), true);
            assert_eq!(snapshot(control).mode.value, Some(Mode::Dry));
            let function = function(&control.context.registry, "one", ID);
            for command in [Command::Temperature(26), Command::Speed(Speed::High)] {
                assert_eq!(
                    function
                        .begin_air_conditioner(command)
                        .unwrap()
                        .accepted()
                        .await,
                    Err(CommandError::UnknownState)
                );
            }
            assert!(writes(&peer).is_empty());

            peer.release_property("one", MODE, json!({"value":1,"updateTime":4}))
                .await;
            until(|| !work(control).refresh.borrow().contains(ID, MODE)).await;
            assert_eq!(snapshot(control).mode.value, Some(Mode::Heat));
            for command in [Command::Temperature(26), Command::Speed(Speed::High)] {
                function
                    .begin_air_conditioner(command)
                    .unwrap()
                    .accepted()
                    .await
                    .unwrap();
            }
            assert_eq!(
                writes(&peer),
                vec![
                    (json!(3), json!(4), json!(26.0)),
                    (json!(4), json!(2), json!(3))
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
