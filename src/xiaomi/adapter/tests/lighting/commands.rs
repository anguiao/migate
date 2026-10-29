use super::*;

#[test]
fn successful_and_partial_receipts_preserve_newer_feedback() {
    for mode_code in [0, -704] {
        block_on(Rig::light("devcea.light.ls2307").run(|c| async move {
            c.announce();
            let peer = peer_values(&c);
            let control = &c;
            c.peer(peer, |peer| async move {
                let light = ready_light(control).await;
                peer.state.borrow_mut().hold_writes = true;
                let color = light
                    .begin_light_action(
                        color_target(LightColorTarget::Rgb(Rgb::from_packed(0xabcdef).unwrap())),
                        true,
                    )
                    .unwrap();
                until(|| peer.count("rpcReq", "one", 0) == 1).await;
                peer.notify_value("one", COLOR, json!(0x334455), false);
                until(|| light.snapshot().lighting().unwrap().color == Rgb::from_packed(0x334455))
                    .await;
                let observed = light.snapshot();
                peer.release_property(
                    "one",
                    COLOR,
                    accepted_items(&[(COLOR, 0), (MODE, mode_code)]),
                )
                .await;
                assert_eq!(color.accepted().await.is_ok(), mode_code == 0);
                assert_eq!(light.snapshot(), observed);
            })
            .await;
        }));
    }
}

#[test]
fn light3_keeps_raw_precision_and_quantized_duplicate_frames_do_not_write() {
    block_on(Rig::light("yeelink.light.light3").run(|c| async move {
        c.announce();
        let peer = peer_values(&c);
        let control = &c;
        c.peer(peer, |peer| async move {
            let light = ready_light(control).await;
            peer.notify_value("one", BRIGHTNESS, json!(32768), false);
            until(|| {
                light
                    .snapshot()
                    .lighting()
                    .unwrap()
                    .brightness_percent
                    .is_some_and(|value| value > 50.0)
            })
            .await;
            let value = light
                .snapshot()
                .lighting()
                .unwrap()
                .brightness_percent
                .unwrap();
            assert!((value - 50.5).abs() < 0.001);
            let b = light
                .begin_light_action(brightness(value + 0.00001), true)
                .unwrap();
            b.accepted().await.unwrap();
            assert_eq!(peer.count("rpcReq", "one", 0), 0);

            freeze_light_clock(control);
            let work = control.context.live.borrow().devices["one"]
                .admitted
                .clone()
                .unwrap();
            let old_read = work.observations.read_stamp("light:2", BRIGHTNESS);
            light
                .begin_light_action(brightness(80.0), true)
                .unwrap()
                .accepted()
                .await
                .unwrap();
            // Neither a read started before the receipt nor a cached report
            // proves that the displayed value still matches the device.
            work.observe_read_at(
                "light:2",
                BRIGHTNESS,
                &old_read,
                &json!(32768),
                Observation {
                    source: PathKind::Local,
                    source_time: None,
                    received_at: 0,
                },
            )
            .unwrap();
            work.notify_value_at(BRIGHTNESS, &json!(32768), None, true, false, 0);
            light
                .begin_light_action(brightness(value), true)
                .unwrap()
                .accepted()
                .await
                .unwrap();
            assert_eq!(peer.count("rpcReq", "one", 0), 2);

            peer.notify_value("one", BRIGHTNESS, json!(32768), false);
            until(|| {
                !work
                    .observations
                    .has_unconfirmed_write("light:2", BRIGHTNESS)
            })
            .await;
            light
                .begin_light_action(brightness(value + 0.00001), true)
                .unwrap()
                .accepted()
                .await
                .unwrap();
            assert_eq!(peer.count("rpcReq", "one", 0), 2);
        })
        .await;
    }));
}

#[test]
fn reversing_targets_before_feedback_still_sends_power_brightness_and_mode() {
    block_on(Rig::light("devcea.light.ls2307").run(|c| async move {
        c.announce();
        let peer = peer_values(&c);
        peer.value("one", BRIGHTNESS, json!(50));
        let control = &c;
        c.peer(peer, |peer| async move {
            let light = ready_light(control).await;
            freeze_light_clock(control);
            let before = light.snapshot();
            for target in [80.0, 50.0] {
                light
                    .begin_light_action(brightness(target), true)
                    .unwrap()
                    .accepted()
                    .await
                    .unwrap();
            }
            light.execute(Command::Off).await.unwrap();
            light.execute(Command::On).await.unwrap();
            for target in [
                LightColorTarget::Rgb(Rgb::from_packed(0xabcdef).unwrap()),
                LightColorTarget::Temperature(
                    before.lighting().unwrap().temperature_kelvin.unwrap() as f64,
                ),
            ] {
                light
                    .begin_light_action(color_target(target), true)
                    .unwrap()
                    .accepted()
                    .await
                    .unwrap();
            }
            assert_eq!(light.snapshot(), before);
            let writes = peer
                .state
                .borrow()
                .seen
                .iter()
                .filter(|(operation, _)| operation == "rpcReq")
                .flat_map(|(_, request)| request["rpc"]["params"].as_array().unwrap())
                .map(|property| {
                    (
                        property["siid"].as_u64().unwrap(),
                        property["piid"].as_u64().unwrap(),
                        property["value"].clone(),
                    )
                })
                .collect::<Vec<_>>();
            assert_eq!(
                writes,
                vec![
                    (2, 2, json!(80)),
                    (2, 2, json!(50)),
                    (2, 1, json!(false)),
                    (2, 1, json!(true)),
                    (4, 1, json!(0xabcdef)),
                    (4, 2, json!(1)),
                    (4, 2, json!(0)),
                ]
            );
        })
        .await;
    }));
}

#[test]
fn retired_consumer_keeps_sent_http_startup_owned_until_original_receipt() {
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
    let (release, gate) = flume::bounded(1);
    block_on(rig.run(|c| async move {
        let light = function(&c.context.registry, "one", "light:2");
        until(|| light.snapshot().lighting().unwrap().brightness_percent == Some(50.0)).await;
        c.cloud.lock().unwrap().property_gate = Some(gate);
        let action = light.begin_light_action(brightness(60.0), false).unwrap();
        until(|| c.cloud.lock().unwrap().numeric_writes.len() == 1).await;
        let record = c.context.live.borrow().devices["one"]
            .admitted
            .clone()
            .unwrap();
        record.life.close();
        light.stop_light(LightDimension::Brightness).unwrap();
        for _ in 0..30 {
            future::yield_now().await;
        }
        let mut accepted = Box::pin(action.accepted());
        assert!(poll_once(accepted.as_mut()).await.is_none());
        release.send(()).unwrap();
        accepted.await.unwrap();
        assert_eq!(
            light.snapshot().lighting().unwrap().brightness_percent,
            Some(50.0)
        );
        assert_eq!(c.cloud.lock().unwrap().numeric_writes.len(), 1);
    }));
}

#[test]
fn cloud_partial_auth_rejection_clears_runtime_light_state_when_access_is_restricted() {
    let rig = Rig::light("devcea.light.ls2307");
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.online.insert("one".into());
        for (binding, value) in [
            (POWER, json!(true)),
            (BRIGHTNESS, json!(50)),
            (TEMPERATURE, json!(2700)),
            (COLOR, json!(0x123456)),
            (MODE, json!(0)),
        ] {
            cloud
                .raw_values
                .insert(("one".into(), binding.siid, binding.piid), value);
        }
        cloud
            .property_codes
            .insert(("one".into(), 4, 2), -704_012_901);
        cloud.token_status = 401;
    }
    block_on(rig.run(|c| async move {
        until(|| c.context.cloud_scope_valid.get()).await;
        let light = function(&c.context.registry, "one", "light:2");
        until(|| {
            light.snapshot().lighting().unwrap().mode == Some(LightMode::Temperature)
                && light.snapshot().lighting().unwrap().color.is_some()
        })
        .await;
        let action = light
            .begin_light_action(
                LightTarget::rgb(None, Rgb::from_packed(0xabcdef).unwrap()),
                true,
            )
            .unwrap();
        assert!(action.accepted().await.is_err());
        assert_eq!(light.snapshot().lighting().unwrap().color, None);
        assert_eq!(light.snapshot().lighting().unwrap().mode, None);
        assert_eq!(c.cloud.lock().unwrap().numeric_writes.len(), 1);
    }));
}

#[test]
fn local_partial_offline_rejection_clears_runtime_light_state_when_device_becomes_unavailable() {
    block_on(Rig::light("devcea.light.ls2307").run(|c| async move {
        c.announce();
        let peer = peer_values(&c);
        let control = &c;
        c.peer(peer, |peer| async move {
            let light = ready_light(control).await;
            peer.state.borrow_mut().hold_writes = true;
            let action = light
                .begin_light_action(
                    LightTarget::rgb(None, Rgb::from_packed(0xabcdef).unwrap()),
                    true,
                )
                .unwrap();
            peer.release_property(
                "one",
                COLOR,
                accepted_items(&[(COLOR, 0), (MODE, -704_042_011)]),
            )
            .await;
            assert!(action.accepted().await.is_err());
            assert_eq!(light.snapshot().lighting().unwrap().color, None);
            assert_eq!(light.snapshot().lighting().unwrap().mode, None);
            assert!(matches!(
                light.snapshot().availability,
                Availability::Unavailable(_)
            ));
        })
        .await;
    }));
}

#[test]
fn merged_final_frame_uses_one_gate_and_stop_keeps_sent_receipt() {
    block_on(Rig::light("xiaomi.light.ceil04").run(|c| async move {
        c.announce();
        let peer = peer_values(&c);
        let control = &c;
        c.peer(peer, |peer| async move {
            let light = ready_light(control).await;
            peer.state.borrow_mut().hold_writes = true;
            let action = light.begin_light_action(brightness(20.0), false).unwrap();
            until(|| peer.count("rpcReq", "one", 0) == 1).await;
            for (percent, final_frame) in [(30.0, false), (40.0, false), (70.0, true)] {
                light
                    .submit_light_frame(action.token(), brightness(percent), final_frame)
                    .unwrap();
            }
            for _ in 0..30 {
                future::yield_now().await;
            }
            assert_eq!(peer.count("rpcReq", "one", 0), 1);
            peer.release_property("one", BRIGHTNESS, accepted_items(&[(BRIGHTNESS, 0)]))
                .await;
            action.accepted().await.unwrap();
            until(|| peer.count("rpcReq", "one", 0) == 2).await;
            let last = peer
                .state
                .borrow()
                .seen
                .iter()
                .rev()
                .find(|(op, _)| op == "rpcReq")
                .unwrap()
                .1
                .clone();
            assert_eq!(last["rpc"]["params"][0]["value"], 70);
            let mut ended = Box::pin(action.ended());
            assert!(poll_once(ended.as_mut()).await.is_none());
            light.stop_light(LightDimension::Brightness).unwrap();
            assert_eq!(
                ended.await.unwrap_err(),
                crate::device::CommandError::Stopped
            );
            peer.release_property("one", BRIGHTNESS, accepted_items(&[(BRIGHTNESS, 0)]))
                .await;
            received_light_frame(control, brightness(70.0)).await;
            assert_eq!(peer.count("rpcReq", "one", 0), 2);
            assert_eq!(
                control.context.diagnostics().devices[0].commands[0]
                    .light_actions
                    .as_ref()
                    .unwrap()
                    .last
                    .as_ref()
                    .unwrap()
                    .result,
                Err(crate::device::CommandError::Stopped)
            );
        })
        .await;
    }));
}

#[test]
fn dropping_startup_waiter_does_not_cancel_receipt_or_allow_a_second_write() {
    block_on(Rig::light("xiaomi.light.ceil04").run(|c| async move {
        c.announce();
        let peer = peer_values(&c);
        let control = &c;
        c.peer(peer, |peer| async move {
            let light = ready_light(control).await;
            peer.state.borrow_mut().hold_writes = true;
            let old = light.begin_light_action(brightness(60.0), false).unwrap();
            let mut waiter = Box::pin(old.accepted());
            assert!(poll_once(waiter.as_mut()).await.is_none());
            until(|| peer.count("rpcReq", "one", 0) == 1).await;
            drop(waiter);
            let new = light.begin_light_action(brightness(75.0), true).unwrap();
            for _ in 0..30 {
                future::yield_now().await;
            }
            assert_eq!(peer.count("rpcReq", "one", 0), 1);
            peer.release_property("one", BRIGHTNESS, accepted_items(&[(BRIGHTNESS, 0)]))
                .await;
            old.accepted().await.unwrap();
            until(|| peer.count("rpcReq", "one", 0) == 2).await;
            peer.release_property("one", BRIGHTNESS, accepted_items(&[(BRIGHTNESS, 0)]))
                .await;
            new.accepted().await.unwrap();
            new.ended().await.unwrap();
            assert_eq!(
                light.snapshot().lighting().unwrap().brightness_percent,
                Some(1.0)
            );
            assert_eq!(control.cloud.lock().unwrap().numeric_writes.len(), 0);
        })
        .await;
    }));
}

#[test]
fn missing_batch_item_fails_without_changing_actual_state() {
    block_on(Rig::light("devcea.light.ls2307").run(|c| async move {
        c.announce();
        let peer = peer_values(&c);
        let control = &c;
        c.peer(peer, |peer| async move {
            let light = ready_light(control).await;
            peer.state.borrow_mut().hold_writes = true;
            let action = light
                .begin_light_action(
                    LightTarget::rgb(None, Rgb::from_packed(0x987654).unwrap()),
                    false,
                )
                .unwrap();
            let observed = light.snapshot();
            peer.release_property("one", COLOR, accepted_items(&[(COLOR, 0)]))
                .await;
            assert!(action.accepted().await.is_err());
            assert!(!light.light_action_current(action.token()));
            assert_eq!(light.snapshot(), observed);
        })
        .await;
    }));
}

#[test]
fn a_new_light_instance_waits_for_the_original_http_receipt() {
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
    let (release, gate) = flume::bounded(1);
    block_on(rig.run(|c| async move {
        let old = function(&c.context.registry, "one", "light:2");
        until(|| old.snapshot().lighting().unwrap().brightness_percent == Some(50.0)).await;
        c.cloud.lock().unwrap().property_gate = Some(gate);
        let first = old.begin_light_action(brightness(60.0), false).unwrap();
        until(|| c.cloud.lock().unwrap().numeric_writes.len() == 1).await;
        let mut changed = c.context.live.borrow().devices["one"]
            .admitted
            .as_ref()
            .unwrap()
            .native
            .clone();
        changed = crate::xiaomi::capability::NativeDescription::new(
            changed.model().to_owned(),
            changed.instance_type().to_owned(),
            super::super::support::device_with_brightness_format(
                changed.device(),
                crate::xiaomi::capability::NumericFormat::Uint16,
            ),
        )
        .unwrap();
        let before = c
            .context
            .registry
            .get(&DeviceKey::new("xiaomi", "one"))
            .expect("the committed light is served");
        c.context
            .live
            .borrow_mut()
            .devices
            .get_mut("one")
            .unwrap()
            .latest = Some(changed);
        c.context.reconcile("one").unwrap();
        until(|| {
            c.context
                .registry
                .get(&DeviceKey::new("xiaomi", "one"))
                .is_some_and(|now| !before.same_instance(&now))
        })
        .await;
        let new = function(&c.context.registry, "one", "light:2");
        until(|| new.snapshot().lighting().unwrap().brightness_percent == Some(50.0)).await;
        let next = new.begin_light_action(brightness(70.0), true).unwrap();
        async_io::Timer::after(std::time::Duration::from_millis(30)).await;
        assert_eq!(c.cloud.lock().unwrap().numeric_writes.len(), 1);
        assert!(poll_once(next.accepted()).await.is_none());
        assert!(poll_once(first.accepted()).await.is_none());
        c.cloud.lock().unwrap().property_gate = None;
        release.send(()).unwrap();
        first.accepted().await.unwrap();
        assert_ne!(
            new.snapshot().lighting().unwrap().brightness_percent,
            Some(60.0)
        );
        next.accepted().await.unwrap();
        assert_eq!(c.cloud.lock().unwrap().numeric_writes.len(), 2);
        assert_eq!(
            new.snapshot().lighting().unwrap().brightness_percent,
            Some(50.0)
        );
    }));
}
