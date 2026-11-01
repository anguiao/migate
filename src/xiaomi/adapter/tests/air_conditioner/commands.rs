use super::*;

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
            function.set_air_conditioner_constraints(restrictions(vec![24, 26]));
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
                function.set_air_conditioner_constraints(restrictions(vec![24]));
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
            function.set_air_conditioner_constraints(restrictions(vec![24, 26]));
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
            assert!(text.contains("hub"), "{text}");
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
