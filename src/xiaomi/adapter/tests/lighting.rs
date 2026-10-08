use super::*;
use crate::device::{LightColorTarget, LightDimension, LightMode, LightTarget, Rgb};
use crate::xiaomi::capability::PropertyBinding;

#[test]
fn all_light_samples_read_and_subscribe_every_property() {
    for model in [
        "devcea.light.ls2307",
        "yeelink.light.light3",
        "yeelink.light.ml9",
        "yeelink.light.spot2",
        "xiaomi.light.ceil04",
        "xiaomi.light.bar2",
        "lemesh.light.wy0c15",
    ] {
        block_on(Rig::light(model).run(|c| async move {
            c.announce();
            let peer = Rc::new(Peer::default());
            let bindings = c.data[0].device.functions[0].lighting.as_ref().unwrap();
            peer.value(
                "one",
                bindings.brightness.property,
                json!(bindings.brightness.range.max),
            );
            peer.value(
                "one",
                bindings.temperature.property,
                json!(bindings.temperature.range.min),
            );
            if let Some(color) = bindings.color {
                peer.value("one", color.property, json!(0x123456));
                peer.value("one", bindings.mode.unwrap().property, json!(0));
            }
            let control = &c;
            c.peer(peer, |peer| async move {
                let light = function(&control.context.registry, "one", "main");
                until(|| {
                    light.snapshot().lighting.unwrap().brightness_percent == Some(100.0)
                        && light
                            .snapshot()
                            .lighting
                            .unwrap()
                            .temperature_kelvin
                            .is_some()
                        && control.context.active_notification("one") == Some(PathKind::Local)
                })
                .await;
                let work = control.context.live.borrow().devices["one"]
                    .work
                    .clone()
                    .unwrap();
                assert_eq!(
                    work.bindings().len(),
                    if model == "devcea.light.ls2307" { 5 } else { 3 }
                );
                assert_eq!(peer.count("subscribe", "one", 0), work.bindings().len());
            })
            .await;
        }));
    }
}

#[test]
fn local_light_write_switches_mode_in_one_batch_and_keeps_partial_acceptance() {
    block_on(Rig::light("devcea.light.ls2307").run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        peer.value("one", PropertyBinding { siid: 2, piid: 2 }, json!(50));
        peer.value("one", PropertyBinding { siid: 2, piid: 3 }, json!(2700));
        peer.value("one", PropertyBinding { siid: 4, piid: 1 }, json!(0x123456));
        peer.value("one", PropertyBinding { siid: 4, piid: 2 }, json!(0));
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| control.context.active_notification("one") == Some(PathKind::Local)).await;
            let light = function(&control.context.registry, "one", "main");
            until(|| light.snapshot().lighting.unwrap().mode == Some(LightMode::Temperature)).await;
            let action = light
                .begin_light_action(
                    LightDimension::Color,
                    LightTarget {
                        color: Some(LightColorTarget::Rgb(Rgb::from_packed(0xff0044).unwrap())),
                        ..Default::default()
                    },
                    true,
                )
                .unwrap();
            action.accepted().await.unwrap();
            assert_eq!(
                light.snapshot().lighting.unwrap().mode,
                Some(LightMode::Rgb)
            );
            let seen = peer.state.borrow();
            let (_, last) = seen
                .seen
                .iter()
                .rev()
                .find(|(op, _)| op == "rpcReq")
                .unwrap();
            assert_eq!(last["rpc"]["params"].as_array().unwrap().len(), 2);
        })
        .await;
    }));
}

const BRIGHTNESS: PropertyBinding = PropertyBinding { siid: 2, piid: 2 };
const TEMPERATURE: PropertyBinding = PropertyBinding { siid: 2, piid: 3 };
const COLOR: PropertyBinding = PropertyBinding { siid: 4, piid: 1 };
const MODE: PropertyBinding = PropertyBinding { siid: 4, piid: 2 };
const POWER: PropertyBinding = PropertyBinding { siid: 2, piid: 1 };
fn peer_values(c: &Control) -> Rc<Peer> {
    let peer = Rc::new(Peer::default());
    let bindings = c.data[0].device.functions[0].lighting.as_ref().unwrap();
    peer.value("one", POWER, json!(true));
    peer.value(
        "one",
        bindings.brightness.property,
        json!(bindings.brightness.range.min),
    );
    peer.value(
        "one",
        bindings.temperature.property,
        json!(bindings.temperature.range.min),
    );
    if let Some(color) = bindings.color {
        peer.value("one", color.property, json!(0x123456));
        peer.value("one", bindings.mode.unwrap().property, json!(0));
    }
    peer
}
async fn ready_light(c: &Control) -> crate::device::Function {
    until(|| c.context.active_notification("one") == Some(PathKind::Local)).await;
    let function = function(&c.context.registry, "one", "main");
    until(|| {
        let snapshot = function.snapshot();
        let light = snapshot.lighting.unwrap();
        snapshot.power.value().is_some()
            && light.brightness_percent.is_some()
            && light.temperature_kelvin.is_some()
            && light.mode.is_some()
            && (c.data[0].device.functions[0]
                .lighting
                .as_ref()
                .unwrap()
                .color
                .is_none()
                || light.color.is_some())
    })
    .await;
    function
}
fn brightness(percent: f64) -> LightTarget {
    LightTarget {
        brightness_percent: Some(percent),
        ..Default::default()
    }
}
fn accepted_items(items: &[(PropertyBinding, i64)]) -> Value {
    json!({"result":items.iter().map(|(binding,code)|json!({"did":"one","siid":binding.siid,"piid":binding.piid,"code":code})).collect::<Vec<_>>()})
}

#[test]
fn own_echo_cache_same_value_and_invalid_notifications_do_not_take_over_but_external_change_does() {
    block_on(Rig::light("devcea.light.ls2307").run(|c| async move {
        c.announce();
        let peer = peer_values(&c);
        let control = &c;
        c.peer(peer, |peer| async move {
            let light = ready_light(control).await;
            peer.state.borrow_mut().hold_writes = true;
            let action = light
                .begin_light_action(LightDimension::Brightness, brightness(60.0), false)
                .unwrap();
            until(|| peer.count("rpcReq", "one", 0) == 1).await;
            peer.notify_value("one", BRIGHTNESS, json!(60), false);
            until(|| light.snapshot().lighting.unwrap().brightness_percent == Some(60.0)).await;
            assert!(light.light_action_current(action.token()));
            peer.notify_value("one", BRIGHTNESS, json!(65), true);
            until(|| light.snapshot().lighting.unwrap().brightness_percent == Some(65.0)).await;
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
                light.snapshot().lighting.unwrap().brightness_percent,
                Some(67.0)
            );
        })
        .await;
    }));
}

#[test]
fn inactive_color_cache_does_not_take_over_until_mode_changes_and_external_off_cancels_both() {
    block_on(Rig::light("devcea.light.ls2307").run(|c| async move {
        c.announce();
        let peer = peer_values(&c);
        let control = &c;
        c.peer(peer, |peer| async move {
            let light = ready_light(control).await;
            let color = light
                .begin_light_action(
                    LightDimension::Color,
                    LightTarget {
                        color: Some(LightColorTarget::Rgb(Rgb::from_packed(0xff0000).unwrap())),
                        ..Default::default()
                    },
                    false,
                )
                .unwrap();
            color.accepted().await.unwrap();
            peer.notify_value("one", TEMPERATURE, json!(4000), false);
            until(|| light.snapshot().lighting.unwrap().temperature_kelvin == Some(4000)).await;
            assert!(light.light_action_current(color.token()));
            peer.notify_value("one", MODE, json!(0), false);
            until(|| !light.light_action_current(color.token())).await;
            let b = light
                .begin_light_action(LightDimension::Brightness, brightness(60.0), false)
                .unwrap();
            let color = light
                .begin_light_action(
                    LightDimension::Color,
                    LightTarget {
                        color: Some(LightColorTarget::Temperature(4200)),
                        ..Default::default()
                    },
                    false,
                )
                .unwrap();
            b.accepted().await.unwrap();
            color.accepted().await.unwrap();
            peer.notify_value("one", POWER, json!(false), false);
            until(|| {
                !light.light_action_current(b.token()) && !light.light_action_current(color.token())
            })
            .await;
            assert_eq!(light.snapshot().power, Power::Off);
        })
        .await;
    }));
}

#[test]
fn late_partial_batch_ack_keeps_related_notification_and_applies_other_success_once() {
    block_on(Rig::light("devcea.light.ls2307").run(|c| async move {
        c.announce();
        let peer = peer_values(&c);
        let control = &c;
        c.peer(peer, |peer| async move {
            let light = ready_light(control).await;
            peer.state.borrow_mut().hold_writes = true;
            let color = light
                .begin_light_action(
                    LightDimension::Color,
                    LightTarget {
                        color: Some(LightColorTarget::Rgb(Rgb::from_packed(0xabcdef).unwrap())),
                        ..Default::default()
                    },
                    true,
                )
                .unwrap();
            until(|| peer.count("rpcReq", "one", 0) == 1).await;
            peer.notify_value("one", COLOR, json!(0x334455), false);
            until(|| light.snapshot().lighting.unwrap().color == Rgb::from_packed(0x334455)).await;
            let revision = light.snapshot().revision;
            peer.release_property("one", COLOR, accepted_items(&[(COLOR, 0), (MODE, 0)]))
                .await;
            color.accepted().await.unwrap();
            assert_eq!(
                light.snapshot().lighting.unwrap().color,
                Rgb::from_packed(0x334455)
            );
            assert_eq!(
                light.snapshot().lighting.unwrap().mode,
                Some(LightMode::Rgb)
            );
            assert_eq!(light.snapshot().revision, revision + 1);
            peer.state.borrow_mut().hold_writes = false;
            let ct = light
                .begin_light_action(
                    LightDimension::Color,
                    LightTarget {
                        color: Some(LightColorTarget::Temperature(4000)),
                        ..Default::default()
                    },
                    true,
                )
                .unwrap();
            ct.accepted().await.unwrap();
            assert_eq!(
                light.snapshot().lighting.unwrap().mode,
                Some(LightMode::Temperature)
            );
            peer.state.borrow_mut().hold_writes = true;
            let rgb = light
                .begin_light_action(
                    LightDimension::Color,
                    LightTarget {
                        color: Some(LightColorTarget::Rgb(Rgb::from_packed(0xffffff).unwrap())),
                        ..Default::default()
                    },
                    true,
                )
                .unwrap();
            until(|| peer.count("rpcReq", "one", 0) == 3).await;
            let revision = light.snapshot().revision;
            peer.release_property("one", COLOR, accepted_items(&[(COLOR, 0), (MODE, -704)]))
                .await;
            assert!(rgb.accepted().await.is_err());
            assert_eq!(
                light.snapshot().lighting.unwrap().color,
                Rgb::from_packed(0xffffff)
            );
            assert_eq!(
                light.snapshot().lighting.unwrap().mode,
                Some(LightMode::Temperature)
            );
            assert_eq!(light.snapshot().revision, revision + 1);
        })
        .await;
    }));
}

#[test]
fn brightness_notification_does_not_discard_unrelated_temperature_read() {
    block_on(Rig::light("devcea.light.ls2307").run(|c| async move {
        c.announce();
        let peer = peer_values(&c);
        let control = &c;
        c.peer(peer, |peer| async move {
            let light = ready_light(control).await;
            peer.state.borrow_mut().hold_reads = true;
            let work = control.context.live.borrow().devices["one"]
                .work
                .clone()
                .unwrap();
            work.refresh();
            peer.release_property("one", POWER, json!({"value":true}))
                .await;
            peer.release_property("one", BRIGHTNESS, json!({"value":40}))
                .await;
            until(|| {
                peer.state
                    .borrow()
                    .held
                    .iter()
                    .any(|(p, _)| decode_mips(&p.payload).unwrap().payload["piid"] == 3)
            })
            .await;
            peer.notify_value("one", BRIGHTNESS, json!(70), false);
            until(|| light.snapshot().lighting.unwrap().brightness_percent == Some(70.0)).await;
            peer.release_property("one", TEMPERATURE, json!({"value":4300}))
                .await;
            until(|| light.snapshot().lighting.unwrap().temperature_kelvin == Some(4300)).await;
            assert_eq!(
                light.snapshot().lighting.unwrap().brightness_percent,
                Some(70.0)
            );
        })
        .await;
    }));
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
                    .lighting
                    .unwrap()
                    .brightness_percent
                    .is_some_and(|value| value > 50.0)
            })
            .await;
            let value = light
                .snapshot()
                .lighting
                .unwrap()
                .brightness_percent
                .unwrap();
            assert!((value - 50.5).abs() < 0.001);
            let b = light
                .begin_light_action(
                    LightDimension::Brightness,
                    brightness(value + 0.00001),
                    true,
                )
                .unwrap();
            b.accepted().await.unwrap();
            assert_eq!(peer.count("rpcReq", "one", 0), 0);
        })
        .await;
    }));
}

#[test]
fn retained_old_color_cannot_block_an_accepted_current_target() {
    block_on(Rig::light("devcea.light.ls2307").run(|c| async move {
        c.announce();
        let peer = peer_values(&c);
        let control = &c;
        c.peer(peer, |peer| async move {
            let light = ready_light(control).await;
            peer.state.borrow_mut().hold_writes = true;
            let color = light
                .begin_light_action(
                    LightDimension::Color,
                    LightTarget {
                        color: Some(LightColorTarget::Rgb(Rgb::from_packed(0xabcdef).unwrap())),
                        ..Default::default()
                    },
                    true,
                )
                .unwrap();
            until(|| peer.count("rpcReq", "one", 0) == 1).await;
            peer.notify_value("one", COLOR, json!(0x234567), true);
            until(|| light.snapshot().lighting.unwrap().color == Rgb::from_packed(0x234567)).await;
            assert!(light.light_action_current(color.token()));
            peer.release_property("one", COLOR, accepted_items(&[(COLOR, 0), (MODE, 0)]))
                .await;
            color.accepted().await.unwrap();
            assert_eq!(
                light.snapshot().lighting.unwrap().color,
                Rgb::from_packed(0xabcdef)
            );
        })
        .await;
    }));
}

#[test]
fn range_change_replaces_canonical_function_and_retires_old_sent_callback() {
    block_on(Rig::light("xiaomi.light.ceil04").run(|c| async move {
        c.announce();
        let peer = peer_values(&c);
        let control = &c;
        c.peer(peer, |peer| async move {
            let old = ready_light(control).await;
            let stored = control
                .store
                .devices()
                .load_published(&DeviceKey::new("xiaomi", "one"))
                .unwrap()
                .unwrap();
            peer.state.borrow_mut().hold_writes = true;
            let action = old
                .begin_light_action(LightDimension::Brightness, brightness(60.0), false)
                .unwrap();
            until(|| peer.count("rpcReq", "one", 0) == 1).await;
            let mut changed = control.context.live.borrow().devices["one"]
                .admitted
                .as_ref()
                .unwrap()
                .cached
                .borrow()
                .clone();
            let range = &mut changed.device.functions[0]
                .lighting
                .as_mut()
                .unwrap()
                .temperature
                .range;
            range.max = 5600;
            changed.device.functions[0].description.capabilities[2] =
                crate::device::Capability::ColorTemperature(*range);
            control
                .context
                .live
                .borrow_mut()
                .devices
                .get_mut("one")
                .unwrap()
                .candidate = Some((changed, None));
            control.context.reconcile("one").unwrap();
            let new = function(&control.context.registry, "one", "main");
            assert_eq!(new.description().temperature_range().unwrap().max, 5600);
            assert!(!old.light_action_current(action.token()));
            peer.release_property("one", BRIGHTNESS, accepted_items(&[(BRIGHTNESS, 0)]))
                .await;
            action.accepted().await.unwrap();
            for _ in 0..30 {
                future::yield_now().await;
            }
            assert_ne!(
                new.snapshot().lighting.unwrap().brightness_percent,
                Some(60.0)
            );
            let after = control
                .store
                .devices()
                .load_published(&DeviceKey::new("xiaomi", "one"))
                .unwrap()
                .unwrap();
            assert_eq!(stored.unique_id, after.unique_id);
            assert_eq!(stored.endpoint, after.endpoint);
        })
        .await;
    }));
}

#[test]
fn malformed_light_read_retries_its_property_without_changing_snapshot() {
    block_on(Rig::light("xiaomi.light.ceil04").run(|c| async move {
        c.announce();
        let peer = peer_values(&c);
        peer.value("one", BRIGHTNESS, json!(101));
        let control = &c;
        c.peer(peer, |peer| async move {
            let light = function(&control.context.registry, "one", "main");
            until(|| {
                light
                    .snapshot()
                    .lighting
                    .unwrap()
                    .temperature_kelvin
                    .is_some()
            })
            .await;
            let work = control.context.live.borrow().devices["one"]
                .work
                .clone()
                .unwrap();
            until(|| !work.read_problems.borrow().is_empty()).await;
            assert_eq!(light.snapshot().lighting.unwrap().brightness_percent, None);
            assert!(
                peer.state
                    .borrow()
                    .seen
                    .iter()
                    .filter(|(op, body)| op == "get" && body["piid"] == 2)
                    .count()
                    >= 2
            );
            peer.value("one", BRIGHTNESS, json!(60));
            work.refresh();
            until(|| light.snapshot().lighting.unwrap().brightness_percent == Some(60.0)).await;
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
        let light = function(&c.context.registry, "one", "main");
        until(|| light.snapshot().lighting.unwrap().brightness_percent == Some(50.0)).await;
        c.cloud.lock().unwrap().property_gate = Some(gate);
        let action = light
            .begin_light_action(LightDimension::Brightness, brightness(60.0), false)
            .unwrap();
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
            light.snapshot().lighting.unwrap().brightness_percent,
            Some(50.0)
        );
        assert_eq!(c.cloud.lock().unwrap().numeric_writes.len(), 1);
    }));
}

#[test]
fn acknowledged_old_targets_are_not_a_permanent_feedback_filter() {
    block_on(Rig::light("xiaomi.light.ceil04").run(|c| async move {
        c.announce();
        let peer = peer_values(&c);
        let control = &c;
        c.peer(peer, |peer| async move {
            let light = ready_light(control).await;
            let action = light
                .begin_light_action(LightDimension::Brightness, brightness(20.0), false)
                .unwrap();
            action.accepted().await.unwrap();
            light
                .submit_light_frame(action.token(), brightness(40.0), false)
                .unwrap();
            until(|| light.snapshot().lighting.unwrap().brightness_percent == Some(40.0)).await;
            peer.notify_value("one", BRIGHTNESS, json!(20), false);
            until(|| !light.light_action_current(action.token())).await;
            assert_eq!(
                light.snapshot().lighting.unwrap().brightness_percent,
                Some(20.0)
            );
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
            let action = light
                .begin_light_action(LightDimension::Brightness, brightness(60.0), false)
                .unwrap();
            until(|| peer.count("rpcReq", "one", 0) == 1).await;
            let work = control.context.live.borrow().devices["one"]
                .work
                .clone()
                .unwrap();
            work.source_changed();
            peer.notify_value("one", BRIGHTNESS, json!(60), false);
            until(|| light.snapshot().lighting.unwrap().brightness_percent == Some(60.0)).await;
            assert!(light.light_action_current(action.token()));
            peer.release_property("one", BRIGHTNESS, accepted_items(&[(BRIGHTNESS, 0)]))
                .await;
            action.accepted().await.unwrap();
        })
        .await;
    }));
}

#[test]
fn cloud_partial_auth_rejection_keeps_accepted_color_before_restricting_access() {
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
        let light = function(&c.context.registry, "one", "main");
        until(|| {
            light.snapshot().lighting.unwrap().mode == Some(LightMode::Temperature)
                && light.snapshot().lighting.unwrap().color.is_some()
        })
        .await;
        let action = light
            .begin_light_action(
                LightDimension::Color,
                LightTarget {
                    color: Some(LightColorTarget::Rgb(Rgb::from_packed(0xabcdef).unwrap())),
                    ..Default::default()
                },
                true,
            )
            .unwrap();
        assert!(action.accepted().await.is_err());
        assert_eq!(
            light.snapshot().lighting.unwrap().color,
            Rgb::from_packed(0xabcdef)
        );
        assert_eq!(
            light.snapshot().lighting.unwrap().mode,
            Some(LightMode::Temperature)
        );
        assert_eq!(c.cloud.lock().unwrap().numeric_writes.len(), 1);
    }));
}

#[test]
fn local_partial_offline_rejection_keeps_accepted_color_before_device_becomes_unavailable() {
    block_on(Rig::light("devcea.light.ls2307").run(|c| async move {
        c.announce();
        let peer = peer_values(&c);
        let control = &c;
        c.peer(peer, |peer| async move {
            let light = ready_light(control).await;
            peer.state.borrow_mut().hold_writes = true;
            let action = light
                .begin_light_action(
                    LightDimension::Color,
                    LightTarget {
                        color: Some(LightColorTarget::Rgb(Rgb::from_packed(0xabcdef).unwrap())),
                        ..Default::default()
                    },
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
            assert_eq!(
                light.snapshot().lighting.unwrap().color,
                Rgb::from_packed(0xabcdef)
            );
            assert_eq!(
                light.snapshot().lighting.unwrap().mode,
                Some(LightMode::Temperature)
            );
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
            let action = light
                .begin_light_action(LightDimension::Brightness, brightness(20.0), false)
                .unwrap();
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
            until(|| light.snapshot().lighting.unwrap().brightness_percent == Some(70.0)).await;
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
            let old = light
                .begin_light_action(LightDimension::Brightness, brightness(60.0), false)
                .unwrap();
            let mut waiter = Box::pin(old.accepted());
            assert!(poll_once(waiter.as_mut()).await.is_none());
            until(|| peer.count("rpcReq", "one", 0) == 1).await;
            drop(waiter);
            let new = light
                .begin_light_action(LightDimension::Brightness, brightness(75.0), true)
                .unwrap();
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
            assert_eq!(
                light.snapshot().lighting.unwrap().brightness_percent,
                Some(75.0)
            );
            assert_eq!(control.cloud.lock().unwrap().numeric_writes.len(), 0);
        })
        .await;
    }));
}

#[test]
fn quantized_nonfinal_skip_waits_for_final_no_write_acceptance() {
    block_on(Rig::light("yeelink.light.light3").run(|c| async move {
        c.announce();
        let peer = peer_values(&c);
        let control = &c;
        c.peer(peer, |peer| async move {
            let light = ready_light(control).await;
            let action = light
                .begin_light_action(LightDimension::Brightness, brightness(1.0), false)
                .unwrap();
            for _ in 0..30 {
                future::yield_now().await;
            }
            let mut accepted = Box::pin(action.accepted());
            assert!(poll_once(accepted.as_mut()).await.is_none());
            assert_eq!(peer.count("rpcReq", "one", 0), 0);
            light
                .submit_light_frame(action.token(), brightness(1.0), true)
                .unwrap();
            accepted.await.unwrap();
            action.ended().await.unwrap();
            assert_eq!(peer.count("rpcReq", "one", 0), 0);
        })
        .await;
    }));
}

#[test]
fn missing_batch_item_keeps_known_acceptance_and_fails_the_logical_action() {
    block_on(Rig::light("devcea.light.ls2307").run(|c| async move {
        c.announce();
        let peer = peer_values(&c);
        let control = &c;
        c.peer(peer, |peer| async move {
            let light = ready_light(control).await;
            peer.state.borrow_mut().hold_writes = true;
            let action = light
                .begin_light_action(
                    LightDimension::Color,
                    LightTarget {
                        color: Some(LightColorTarget::Rgb(Rgb::from_packed(0x987654).unwrap())),
                        ..Default::default()
                    },
                    false,
                )
                .unwrap();
            let revision = light.snapshot().revision;
            peer.release_property("one", COLOR, accepted_items(&[(COLOR, 0)]))
                .await;
            assert!(action.accepted().await.is_err());
            assert!(!light.light_action_current(action.token()));
            assert_eq!(
                light.snapshot().lighting.unwrap().color,
                Rgb::from_packed(0x987654)
            );
            assert_eq!(
                light.snapshot().lighting.unwrap().mode,
                Some(LightMode::Temperature)
            );
            assert_eq!(light.snapshot().revision, revision + 1);
        })
        .await;
    }));
}

#[test]
fn rgb_notification_source_requires_all_five_property_subscriptions() {
    block_on(Rig::light("devcea.light.ls2307").run(|c| async move {
        c.announce();
        let peer = peer_values(&c);
        peer.state
            .borrow_mut()
            .sub_reject
            .insert(("one".into(), MODE.siid, MODE.piid));
        let control = &c;
        c.peer(peer, |peer| async move {
            let work = control.context.live.borrow().devices["one"]
                .work
                .clone()
                .unwrap();
            until(|| work.local_exhausted.get()).await;
            assert!(!work.subscribed.borrow().contains(&(MODE.siid, MODE.piid)));
            assert_eq!(control.context.active_notification("one"), None);
            let light = function(&control.context.registry, "one", "main");
            assert_eq!(light.snapshot().availability, Availability::Available);
            light
                .begin_light_action(LightDimension::Brightness, brightness(60.0), true)
                .unwrap()
                .accepted()
                .await
                .unwrap();
            assert_eq!(peer.count("rpcReq", "one", 0), 1);
        })
        .await;
    }));
}

#[test]
fn new_mode_only_refreshes_its_missing_color_without_retiring_brightness_read() {
    block_on(Rig::light("devcea.light.ls2307").run(|c| async move {
        c.announce();
        let peer = peer_values(&c);
        peer.value("one", COLOR, json!(0x1000000));
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| control.context.active_notification("one") == Some(PathKind::Local)).await;
            let light = function(&control.context.registry, "one", "main");
            let work = control.context.live.borrow().devices["one"]
                .work
                .clone()
                .unwrap();
            until(|| {
                light
                    .snapshot()
                    .lighting
                    .unwrap()
                    .brightness_percent
                    .is_some()
                    && light.snapshot().lighting.unwrap().mode == Some(LightMode::Temperature)
                    && !work.read_problems.borrow().is_empty()
            })
            .await;
            peer.state.borrow_mut().hold_reads = true;
            work.refresh_property("main", BRIGHTNESS);
            until(|| {
                peer.state
                    .borrow()
                    .held
                    .iter()
                    .any(|(publish, _)| decode_mips(&publish.payload).unwrap().payload["piid"] == 2)
            })
            .await;
            peer.notify_value("one", MODE, json!(1), false);
            until(|| light.snapshot().lighting.unwrap().mode == Some(LightMode::Rgb)).await;
            assert_eq!(light.snapshot().lighting.unwrap().color, None);
            peer.release_property("one", BRIGHTNESS, json!({"value":50}))
                .await;
            until(|| light.snapshot().lighting.unwrap().brightness_percent == Some(50.0)).await;
        })
        .await;
    }));
}
