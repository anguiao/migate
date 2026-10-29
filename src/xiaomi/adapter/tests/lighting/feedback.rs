use super::*;

#[test]
fn own_echo_cache_same_value_and_invalid_notifications_do_not_take_over_but_external_change_does() {
    block_on(Rig::light("devcea.light.ls2307").run(|c| async move {
        c.announce();
        let peer = peer_values(&c);
        let control = &c;
        c.peer(peer, |peer| async move {
            let light = ready_light(control).await;
            peer.state.borrow_mut().hold_writes = true;
            let action = light.begin_light_action(brightness(60.0), false).unwrap();
            until(|| peer.count("rpcReq", "one", 0) == 1).await;
            peer.notify_value("one", BRIGHTNESS, json!(60), false);
            until(|| light.snapshot().lighting().unwrap().brightness_percent == Some(60.0)).await;
            assert!(light.light_action_current(action.token()));
            peer.notify_value("one", BRIGHTNESS, json!(65), true);
            until(|| light.snapshot().lighting().unwrap().brightness_percent == Some(65.0)).await;
            assert!(light.light_action_current(action.token()));
            peer.notify_value("one", BRIGHTNESS, json!(65), false);
            peer.notify_value("one", BRIGHTNESS, json!(200), false);
            peer.notify_value("one", BRIGHTNESS, json!("67"), false);
            for _ in 0..30 {
                future::yield_now().await;
            }
            assert!(light.light_action_current(action.token()));
            peer.notify_value("one", BRIGHTNESS, json!(67), false);
            until(|| !light.light_action_current(action.token())).await;
            assert_eq!(
                action.ended().await.unwrap_err(),
                crate::device::CommandError::Superseded
            );
            peer.release_property("one", BRIGHTNESS, accepted_items(&[(BRIGHTNESS, 0)]))
                .await;
            action.accepted().await.unwrap();
            assert_eq!(
                light.snapshot().lighting().unwrap().brightness_percent,
                Some(67.0)
            );
        })
        .await;
    }));
}

#[test]
fn removing_and_readding_preserves_feedback_until_accepted_history_expires() {
    let mut rig = Rig::light("xiaomi.light.ceil04");
    // Keep the deliberately held receipt alive across replacement and feedback.
    rig.source.request_timeout = Duration::from_secs(2);
    block_on(rig.run(|c| async move {
        c.announce();
        let peer = peer_values(&c);
        let control = &c;
        c.peer(peer, |peer| async move {
            let old = ready_light(control).await;
            freeze_light_clock(control);
            peer.state.borrow_mut().hold_writes = true;
            let first = old.begin_light_action(brightness(60.0), false).unwrap();
            until(|| peer.count("rpcReq", "one", 0) == 1).await;
            let cached = control.context.live.borrow().devices["one"]
                .admitted
                .as_ref()
                .unwrap()
                .native
                .clone();
            let access = control.context.live.borrow().devices["one"].access.clone();
            let cloud_access = control.context.live.borrow().devices["one"]
                .cloud_access
                .clone();
            control.context.remove("one").unwrap();
            let slot = super::super::super::state::DeviceSlot {
                latest: Some(cached),
                latest_name: None,
                access,
                cloud_access,
                // A genuine fresh confirmation clears the durable tombstone.
                confirmed: true,
                ..Default::default()
            };
            control
                .context
                .live
                .borrow_mut()
                .devices
                .insert("one".into(), slot);
            control.context.reconcile("one").unwrap();
            let new = ready_replacement_light(control).await;
            let next = new.begin_light_action(brightness(55.0), false).unwrap();
            async_io::Timer::after(std::time::Duration::from_millis(30)).await;
            assert_eq!(peer.count("rpcReq", "one", 0), 1);
            assert!(poll_once(next.accepted()).await.is_none());
            assert!(poll_once(first.accepted()).await.is_none());
            peer.notify_value("one", BRIGHTNESS, json!(60), false);
            until(|| new.snapshot().lighting().unwrap().brightness_percent == Some(60.0)).await;
            assert!(new.light_action_current(next.token()));
            assert!(poll_once(next.accepted()).await.is_none());
            assert_eq!(peer.count("rpcReq", "one", 0), 1);
            peer.notify_value("one", BRIGHTNESS, json!(55), true);
            until(|| new.snapshot().lighting().unwrap().brightness_percent == Some(55.0)).await;
            // The old write may have changed this same device after the new
            // instance's cached report, so the queued target must still send.
            peer.release_property("one", BRIGHTNESS, accepted_items(&[(BRIGHTNESS, 0)]))
                .await;
            first.accepted().await.unwrap();
            let accepted_at = control.clock.now();
            until(|| peer.count("rpcReq", "one", 0) == 2).await;
            assert_eq!(
                new.snapshot().lighting().unwrap().brightness_percent,
                Some(55.0)
            );
            assert!(poll_once(next.accepted()).await.is_none());
            peer.release_property("one", BRIGHTNESS, accepted_items(&[(BRIGHTNESS, 0)]))
                .await;
            next.accepted().await.unwrap();
            assert_eq!(peer.count("rpcReq", "one", 0), 2);
            assert_eq!(
                new.snapshot().lighting().unwrap().brightness_percent,
                Some(55.0)
            );
            control
                .clock
                .advance(accepted_at + Duration::from_millis(2999) - control.clock.now());
            peer.notify_value("one", BRIGHTNESS, json!(60), false);
            until(|| new.snapshot().lighting().unwrap().brightness_percent == Some(60.0)).await;
            assert!(new.light_action_current(next.token()));
            peer.notify_value("one", BRIGHTNESS, json!(55), false);
            until(|| new.snapshot().lighting().unwrap().brightness_percent == Some(55.0)).await;
            assert!(new.light_action_current(next.token()));
            control.clock.advance(Duration::from_millis(1));
            peer.notify_value("one", BRIGHTNESS, json!(60), false);
            until(|| !new.light_action_current(next.token())).await;
            assert_eq!(
                next.ended().await.unwrap_err(),
                crate::device::CommandError::Superseded
            );
            assert_eq!(
                new.snapshot().lighting().unwrap().brightness_percent,
                Some(60.0)
            );
            assert_eq!(peer.count("rpcReq", "one", 0), 2);
        })
        .await;
    }));
}

#[test]
fn local_late_echo_after_two_accepted_frames_does_not_cancel_action() {
    for expire_history in [false, true] {
        block_on(Rig::light("xiaomi.light.ceil04").run(|c| async move {
            c.announce();
            let peer = peer_values(&c);
            let control = &c;
            c.peer(peer, |peer| async move {
                let light = ready_light(control).await;
                freeze_light_clock(control);
                let action = light.begin_light_action(brightness(20.0), false).unwrap();
                action.accepted().await.unwrap();
                light
                    .submit_light_frame(action.token(), brightness(40.0), false)
                    .unwrap();
                received_light_frame(control, brightness(40.0)).await;
                let values: Vec<_> = peer
                    .state
                    .borrow()
                    .seen
                    .iter()
                    .filter(|(op, _)| op == "rpcReq")
                    .map(|(_, body)| body["rpc"]["params"][0]["value"].as_i64().unwrap())
                    .collect();
                assert_eq!(values, vec![20, 40]);

                // Fresh reads update snapshots without discarding the history
                // that identifies later echoes of either accepted frame.
                let work = control.context.live.borrow().devices["one"]
                    .admitted
                    .clone()
                    .unwrap();
                for value in [20, 40] {
                    let stamp = work.observations.read_stamp("light:2", BRIGHTNESS);
                    work.observe_read("light:2", BRIGHTNESS, &stamp, &json!(value))
                        .unwrap();
                    assert_eq!(
                        light.snapshot().lighting().unwrap().brightness_percent,
                        Some(f64::from(value))
                    );
                    assert!(light.light_action_current(action.token()));
                }

                // Alternating old and new echoes really change the snapshot;
                // same-value suppression cannot make these assertions pass.
                for value in [20, 40, 20, 40] {
                    peer.notify_value("one", BRIGHTNESS, json!(value), false);
                    until(|| {
                        light.snapshot().lighting().unwrap().brightness_percent
                            == Some(f64::from(value))
                    })
                    .await;
                    assert!(light.light_action_current(action.token()));
                }
                let external_value = if expire_history {
                    control.clock.advance(Duration::from_secs(3));
                    20
                } else {
                    70
                };
                peer.notify_value("one", BRIGHTNESS, json!(external_value), false);
                until(|| !light.light_action_current(action.token())).await;
                assert_eq!(
                    action.ended().await.unwrap_err(),
                    crate::device::CommandError::Superseded
                );
                assert_eq!(
                    light.snapshot().lighting().unwrap().brightness_percent,
                    Some(f64::from(external_value))
                );
            })
            .await;
        }));
    }
}

#[test]
fn cloud_late_echo_after_two_accepted_frames_does_not_cancel_action() {
    for expire_history in [false, true] {
        let rig = Rig::light("xiaomi.light.ceil04");
        {
            let mut cloud = rig.cloud.lock().unwrap();
            cloud.online.insert("one".into());
            for (binding, value) in [
                (POWER, json!(true)),
                (BRIGHTNESS, json!(50)),
                (TEMPERATURE, json!(2700)),
            ] {
                cloud
                    .raw_values
                    .insert(("one".into(), binding.siid, binding.piid), value);
            }
        }
        block_on(rig.run(|c| async move {
            let light = function(&c.context.registry, "one", "light:2");
            until(|| light.snapshot().lighting().unwrap().brightness_percent == Some(50.0)).await;
            freeze_light_clock(&c);
            let action = light.begin_light_action(brightness(20.0), false).unwrap();
            action.accepted().await.unwrap();
            light
                .submit_light_frame(action.token(), brightness(40.0), false)
                .unwrap();
            received_light_frame(&c, brightness(40.0)).await;
            let values: Vec<_> = c
                .cloud
                .lock()
                .unwrap()
                .numeric_writes
                .iter()
                .map(|(items, _)| items[0]["value"].as_i64().unwrap())
                .collect();
            assert_eq!(values, vec![20, 40]);

            // HTTP receipts and notification delivery share the device's
            // feedback ownership; inject at the notification dispatch boundary.
            let work = c.context.live.borrow().devices["one"]
                .admitted
                .clone()
                .unwrap();
            // Cached reports must leave receipt history intact even when they
            // temporarily restore an older accepted frame's value.
            for value in [20, 40] {
                work.notify_value(BRIGHTNESS, &json!(value), true, true);
                assert_eq!(
                    light.snapshot().lighting().unwrap().brightness_percent,
                    Some(f64::from(value))
                );
                assert!(light.light_action_current(action.token()));
            }
            for value in [20, 40, 20, 40] {
                work.notify_value(BRIGHTNESS, &json!(value), false, true);
                assert_eq!(
                    light.snapshot().lighting().unwrap().brightness_percent,
                    Some(f64::from(value))
                );
                assert!(light.light_action_current(action.token()));
            }
            let external_value = if expire_history {
                c.clock.advance(Duration::from_secs(3));
                20
            } else {
                70
            };
            work.notify_value(BRIGHTNESS, &json!(external_value), false, true);
            assert!(!light.light_action_current(action.token()));
            assert_eq!(
                action.ended().await.unwrap_err(),
                crate::device::CommandError::Superseded
            );
            assert_eq!(
                light.snapshot().lighting().unwrap().brightness_percent,
                Some(f64::from(external_value))
            );
        }));
    }
}

#[test]
fn accepted_numeric_echoes_expire_three_seconds_after_ack_without_renewal() {
    block_on(Rig::light("xiaomi.light.ceil04").run(|c| async move {
        c.announce();
        let peer = peer_values(&c);
        let control = &c;
        c.peer(peer, |peer| async move {
            let light = ready_light(control).await;
            freeze_light_clock(control);
            let action = light.begin_light_action(brightness(20.0), false).unwrap();
            action.accepted().await.unwrap();
            let work = control.context.live.borrow().devices["one"]
                .admitted
                .clone()
                .unwrap();
            light
                .submit_light_frame(action.token(), brightness(40.0), false)
                .unwrap();
            received_light_frame(control, brightness(40.0)).await;
            control.clock.advance(Duration::from_millis(2999));
            for value in [40, 20, 40, 20, 40] {
                work.notify_value(BRIGHTNESS, &json!(value), false, true);
                assert_eq!(
                    light.snapshot().lighting().unwrap().brightness_percent,
                    Some(f64::from(value))
                );
                assert!(light.light_action_current(action.token()));
            }
            // Neither changed nor same-value notifications renew the ACK's
            // deadline. At exactly three seconds an old value is external.
            control.clock.advance(Duration::from_millis(1));
            peer.notify_value("one", BRIGHTNESS, json!(20), false);
            until(|| !light.light_action_current(action.token())).await;
            assert_eq!(
                light.snapshot().lighting().unwrap().brightness_percent,
                Some(20.0)
            );
        })
        .await;
    }));
}

#[test]
fn accepted_temperature_and_rgb_history_preserves_repeated_echoes() {
    for rgb in [false, true] {
        block_on(Rig::light("devcea.light.ls2307").run(|c| async move {
            c.announce();
            let peer = peer_values(&c);
            peer.value("one", MODE, json!(u8::from(rgb)));
            let control = &c;
            c.peer(peer, |peer| async move {
                let light = ready_light(control).await;
                freeze_light_clock(control);
                let values: [u32; 3] = if rgb {
                    [0xff0000, 0x00ff00, 0x0000ff]
                } else {
                    [3300, 4200, 5000]
                };
                let target = |value| {
                    color_target(if rgb {
                        LightColorTarget::Rgb(Rgb::from_packed(value).unwrap())
                    } else {
                        LightColorTarget::Temperature(value as f64)
                    })
                };
                let current = |value| {
                    let state = light.snapshot().lighting().unwrap();
                    if rgb {
                        state.color == Rgb::from_packed(value)
                    } else {
                        state.temperature_kelvin == Some(value)
                    }
                };
                let action = light.begin_light_action(target(values[0]), false).unwrap();
                action.accepted().await.unwrap();
                light
                    .submit_light_frame(action.token(), target(values[1]), false)
                    .unwrap();
                received_light_frame(control, target(values[1])).await;
                assert_eq!(peer.count("rpcReq", "one", 0), 2);
                let binding = if rgb { COLOR } else { TEMPERATURE };
                for value in [values[0], values[1], values[0], values[1]] {
                    peer.notify_value("one", binding, json!(value), false);
                    until(|| current(value)).await;
                    assert!(light.light_action_current(action.token()));
                }
                peer.notify_value("one", binding, json!(values[2]), false);
                until(|| !light.light_action_current(action.token())).await;
                assert!(current(values[2]));
                assert_eq!(
                    action.ended().await.unwrap_err(),
                    crate::device::CommandError::Superseded
                );
            })
            .await;
        }));
    }
}

#[test]
fn acknowledged_mode_echo_preserves_action_until_history_expires() {
    for old_mode in [0, 1] {
        block_on(Rig::light("devcea.light.ls2307").run(|c| async move {
            c.announce();
            let peer = peer_values(&c);
            peer.value("one", MODE, json!(1 - old_mode));
            let control = &c;
            c.peer(peer, |peer| async move {
                let light = ready_light(control).await;
                freeze_light_clock(control);
                let target = |mode| {
                    color_target(if mode == 0 {
                        LightColorTarget::Temperature(4200.0)
                    } else {
                        LightColorTarget::Rgb(Rgb::from_packed(0xff0000).unwrap())
                    })
                };
                let first = light.begin_light_action(target(old_mode), true).unwrap();
                first.accepted().await.unwrap();
                first.ended().await.unwrap();
                peer.notify_value("one", MODE, json!(old_mode), false);
                until(|| {
                    light.snapshot().lighting().unwrap().mode
                        == Some(if old_mode == 0 {
                            LightMode::Temperature
                        } else {
                            LightMode::Rgb
                        })
                })
                .await;
                let next = light
                    .begin_light_action(target(1 - old_mode), false)
                    .unwrap();
                next.accepted().await.unwrap();
                assert_eq!(peer.count("rpcReq", "one", 0), 2);
                for mode in [1 - old_mode, old_mode, 1 - old_mode] {
                    peer.notify_value("one", MODE, json!(mode), false);
                    until(|| {
                        light.snapshot().lighting().unwrap().mode
                            == Some(if mode == 0 {
                                LightMode::Temperature
                            } else {
                                LightMode::Rgb
                            })
                    })
                    .await;
                    assert!(light.light_action_current(next.token()));
                }
                control.clock.advance(Duration::from_secs(3));
                peer.notify_value("one", MODE, json!(old_mode), false);
                until(|| !light.light_action_current(next.token())).await;
                assert_eq!(
                    next.ended().await.unwrap_err(),
                    crate::device::CommandError::Superseded
                );
                assert_eq!(
                    light.snapshot().lighting().unwrap().mode,
                    Some(if old_mode == 0 {
                        LightMode::Temperature
                    } else {
                        LightMode::Rgb
                    })
                );
            })
            .await;
        }));
    }
}

#[test]
fn acknowledged_off_echo_preserves_actions_until_history_expires() {
    block_on(Rig::light("devcea.light.ls2307").run(|c| async move {
        c.announce();
        let peer = peer_values(&c);
        let control = &c;
        c.peer(peer, |peer| async move {
            let light = ready_light(control).await;
            freeze_light_clock(control);
            light.execute(Command::Off).await.unwrap();
            peer.notify_value("one", POWER, json!(false), false);
            until(|| light.snapshot().power() == Some(Power::Off)).await;
            light.execute(Command::On).await.unwrap();
            peer.notify_value("one", POWER, json!(true), false);
            until(|| light.snapshot().power() == Some(Power::On)).await;
            assert_eq!(peer.count("rpcReq", "one", 0), 2);
            let level = light.begin_light_action(brightness(60.0), false).unwrap();
            level.accepted().await.unwrap();
            let color = light
                .begin_light_action(
                    color_target(LightColorTarget::Rgb(Rgb::from_packed(0xff0000).unwrap())),
                    false,
                )
                .unwrap();
            color.accepted().await.unwrap();
            assert_eq!(light.snapshot().power(), Some(Power::On));
            peer.notify_value("one", POWER, json!(false), false);
            until(|| light.snapshot().power() == Some(Power::Off)).await;
            assert!(light.light_action_current(level.token()));
            assert!(light.light_action_current(color.token()));
            peer.notify_value("one", POWER, json!(true), false);
            until(|| light.snapshot().power() == Some(Power::On)).await;
            control.clock.advance(Duration::from_secs(3));
            peer.notify_value("one", POWER, json!(false), false);
            until(|| {
                !light.light_action_current(level.token())
                    && !light.light_action_current(color.token())
            })
            .await;
            assert_eq!(light.snapshot().power(), Some(Power::Off));
            assert_eq!(
                level.ended().await.unwrap_err(),
                crate::device::CommandError::Superseded
            );
            assert_eq!(
                color.ended().await.unwrap_err(),
                crate::device::CommandError::Superseded
            );
        })
        .await;
    }));
}

#[test]
fn in_flight_mode_echo_preserves_the_action_before_ack() {
    block_on(Rig::light("devcea.light.ls2307").run(|c| async move {
        c.announce();
        let peer = peer_values(&c);
        let control = &c;
        c.peer(peer, |peer| async move {
            let light = ready_light(control).await;
            freeze_light_clock(control);
            peer.state.borrow_mut().hold_writes = true;
            let action = light
                .begin_light_action(
                    color_target(LightColorTarget::Rgb(Rgb::from_packed(0xff0000).unwrap())),
                    false,
                )
                .unwrap();
            until(|| peer.count("rpcReq", "one", 0) == 1).await;
            peer.notify_value("one", MODE, json!(1), false);
            until(|| light.snapshot().lighting().unwrap().mode == Some(LightMode::Rgb)).await;
            assert!(light.light_action_current(action.token()));
            assert!(poll_once(action.accepted()).await.is_none());
            peer.release_property("one", COLOR, accepted_items(&[(COLOR, 0), (MODE, 0)]))
                .await;
            action.accepted().await.unwrap();
            assert!(light.light_action_current(action.token()));
        })
        .await;
    }));
}

#[test]
fn in_flight_feedback_survives_valid_source_epoch_change() {
    block_on(Rig::light("xiaomi.light.ceil04").run(|c| async move {
        c.announce();
        let peer = peer_values(&c);
        let control = &c;
        c.peer(peer, |peer| async move {
            let light = ready_light(control).await;
            peer.state.borrow_mut().hold_writes = true;
            let action = light.begin_light_action(brightness(60.0), false).unwrap();
            until(|| peer.count("rpcReq", "one", 0) == 1).await;
            let work = control.context.live.borrow().devices["one"]
                .admitted
                .clone()
                .unwrap();
            work.test_source_changed();
            peer.notify_value("one", BRIGHTNESS, json!(60), false);
            until(|| light.snapshot().lighting().unwrap().brightness_percent == Some(60.0)).await;
            assert!(light.light_action_current(action.token()));
            peer.release_property("one", BRIGHTNESS, accepted_items(&[(BRIGHTNESS, 0)]))
                .await;
            action.accepted().await.unwrap();
        })
        .await;
    }));
}
