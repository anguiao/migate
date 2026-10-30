use super::*;

#[test]
fn mcn04_state_report_updates_valid_fields_atomically_and_preserves_invalid_fields() {
    block_on(mcn04_rig().run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        peer.value("one", MCN_STATE, json!("P0_M0_T24_S1_D1"));
        peer.value(
            "one",
            MCN_RANGE,
            json!(
                "{\"models\":[{\"mode\":0,\"temps\":[24,26],\"speeds\":[0,1,2,3],\"directs\":[1]}]}"
            ),
        );
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| snapshot(control).power.value == Some(true)).await;
            let function = function(&control.context.registry, "one", ID);
            let before = function.snapshot().revision;
            peer.notify_value_at(
                "one",
                MCN_STATE,
                json!("P1_M1_T999_S3_D0"),
                Some(777),
                false,
            );
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
                    .mcn04_problem(ID, MCN_STATE)
                    .unwrap()
                    .contains("T")
            );
        })
        .await;
    }));
}

#[test]
fn mcn04_range_is_unknown_until_a_source_reports_it() {
    block_on(mcn04_rig().run(|c| async move {
        let function = function(&c.context.registry, "one", ID);
        assert!(function.air_conditioner_constraints().is_none());
    }));
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
