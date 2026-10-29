use super::*;

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
            let bindings = c.data[0].device().functions()[0].lighting().unwrap();
            let brightness = bindings.brightness().unwrap();
            let temperature = bindings.temperature().unwrap();
            peer.value(
                "one",
                brightness.property(),
                json!(brightness.range().max()),
            );
            peer.value(
                "one",
                temperature.property(),
                json!(temperature.range().min()),
            );
            if let Some(color) = bindings.color() {
                peer.value("one", color.property(), json!(0x123456));
                peer.value("one", bindings.mode().unwrap().property(), json!(0));
            }
            let control = &c;
            c.peer(peer, |peer| async move {
                let light = function(&control.context.registry, "one", "light:2");
                until(|| {
                    light.snapshot().lighting().unwrap().brightness_percent == Some(100.0)
                        && light
                            .snapshot()
                            .lighting()
                            .unwrap()
                            .temperature_kelvin
                            .is_some()
                        && control.context.active_notification("one") == Some(PathKind::Local)
                })
                .await;
                let work = control.context.live.borrow().devices["one"]
                    .admitted
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
fn local_light_acceptance_keeps_actual_state_until_feedback() {
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
            let light = function(&control.context.registry, "one", "light:2");
            until(|| light.snapshot().lighting().unwrap().mode == Some(LightMode::Temperature))
                .await;
            let action = light
                .begin_light_action(
                    LightTarget::rgb(None, Rgb::from_packed(0xff0044).unwrap()),
                    false,
                )
                .unwrap();
            let before = light.snapshot();
            action.accepted().await.unwrap();
            assert_eq!(light.snapshot(), before);
            peer.notify_value("one", COLOR, json!(0xff0044), false);
            peer.notify_value("one", MODE, json!(1), false);
            until(|| {
                let state = light.snapshot().lighting().unwrap();
                state.color == Rgb::from_packed(0xff0044) && state.mode == Some(LightMode::Rgb)
            })
            .await;
            assert!(light.light_action_current(action.token()));
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

#[test]
fn on_off_and_dimmable_source_io_use_only_real_bindings() {
    block_on(
        Rig::profile_light("xiaomi.light.ceil04", profile_instance(false, false)).run(
            |c| async move {
                c.announce();
                let peer = Rc::new(Peer::default());
                peer.value("one", POWER, json!(true));
                let control = &c;
                c.peer(peer, |peer| async move {
                    until(|| control.context.active_notification("one") == Some(PathKind::Local))
                        .await;
                    let light = function(&control.context.registry, "one", "light:2");
                    until(|| {
                        light
                            .snapshot()
                            .power()
                            .and_then(crate::device::Power::value)
                            == Some(true)
                    })
                    .await;
                    let work = control.context.live.borrow().devices["one"]
                        .admitted
                        .clone()
                        .unwrap();
                    assert_eq!(work.bindings().len(), 1);
                    light.execute(Command::Off).await.unwrap();
                    let seen = peer.state.borrow();
                    let (_, last) = seen
                        .seen
                        .iter()
                        .rev()
                        .find(|(op, _)| op == "rpcReq")
                        .unwrap();
                    let params = last["rpc"]["params"].as_array().unwrap();
                    assert_eq!(params.len(), 1);
                    assert_eq!(params[0]["siid"], 2);
                    assert_eq!(params[0]["piid"], 1);
                    assert_eq!(params[0]["value"], false);
                })
                .await;
            },
        ),
    );
    block_on(
        Rig::profile_light("xiaomi.light.ceil04", profile_instance(true, false)).run(
            |c| async move {
                c.announce();
                let peer = Rc::new(Peer::default());
                peer.value("one", POWER, json!(true));
                peer.value("one", BRIGHTNESS, json!(50));
                let control = &c;
                c.peer(peer, |peer| async move {
                    until(|| control.context.active_notification("one") == Some(PathKind::Local))
                        .await;
                    let light = function(&control.context.registry, "one", "light:2");
                    until(|| light.snapshot().lighting().unwrap().brightness_percent == Some(50.0))
                        .await;
                    let work = control.context.live.borrow().devices["one"]
                        .admitted
                        .clone()
                        .unwrap();
                    assert_eq!(work.bindings().len(), 2);
                    let action = light.begin_light_action(brightness(70.0), true).unwrap();
                    action.accepted().await.unwrap();
                    let seen = peer.state.borrow();
                    let (_, last) = seen
                        .seen
                        .iter()
                        .rev()
                        .find(|(op, _)| op == "rpcReq")
                        .unwrap();
                    let params = last["rpc"]["params"].as_array().unwrap();
                    assert_eq!(params.len(), 1);
                    assert_eq!(params[0]["siid"], 2);
                    assert_eq!(params[0]["piid"], 2);
                })
                .await;
            },
        ),
    );
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
                    LightTarget::rgb(None, Rgb::from_packed(0xff0000).unwrap()),
                    false,
                )
                .unwrap();
            color.accepted().await.unwrap();
            peer.notify_value("one", MODE, json!(1), true);
            until(|| light.snapshot().lighting().unwrap().mode == Some(LightMode::Rgb)).await;
            peer.notify_value("one", TEMPERATURE, json!(4000), false);
            until(|| light.snapshot().lighting().unwrap().temperature_kelvin == Some(4000)).await;
            assert!(light.light_action_current(color.token()));
            peer.notify_value("one", MODE, json!(0), false);
            until(|| !light.light_action_current(color.token())).await;
            let b = light.begin_light_action(brightness(60.0), false).unwrap();
            let color = light
                .begin_light_action(
                    LightTarget::temperature(None, 4200.0).expect("valid test target"),
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
            assert_eq!(light.snapshot().power(), Some(Power::Off));
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
                .admitted
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
            until(|| light.snapshot().lighting().unwrap().brightness_percent == Some(70.0)).await;
            peer.release_property("one", TEMPERATURE, json!({"value":4300}))
                .await;
            until(|| light.snapshot().lighting().unwrap().temperature_kelvin == Some(4300)).await;
            assert_eq!(
                light.snapshot().lighting().unwrap().brightness_percent,
                Some(70.0)
            );
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
            let light = function(&control.context.registry, "one", "light:2");
            until(|| {
                light
                    .snapshot()
                    .lighting()
                    .unwrap()
                    .temperature_kelvin
                    .is_some()
            })
            .await;
            let work = control.context.live.borrow().devices["one"]
                .admitted
                .clone()
                .unwrap();
            until(|| !work.refresh.borrow().snapshot().problems.is_empty()).await;
            assert_eq!(
                light.snapshot().lighting().unwrap().brightness_percent,
                None
            );
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
            until(|| light.snapshot().lighting().unwrap().brightness_percent == Some(60.0)).await;
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
            let action = light.begin_light_action(brightness(1.0), false).unwrap();
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
                .admitted
                .clone()
                .unwrap();
            until(|| work.test_local_exhausted()).await;
            assert!(!work.test_local_subscribed_pair(MODE.siid, MODE.piid));
            assert_eq!(control.context.active_notification("one"), None);
            let light = function(&control.context.registry, "one", "light:2");
            assert_eq!(light.snapshot().availability, Availability::Available);
            light
                .begin_light_action(brightness(60.0), true)
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
            let light = function(&control.context.registry, "one", "light:2");
            let work = control.context.live.borrow().devices["one"]
                .admitted
                .clone()
                .unwrap();
            until(|| {
                light
                    .snapshot()
                    .lighting()
                    .unwrap()
                    .brightness_percent
                    .is_some()
                    && light.snapshot().lighting().unwrap().mode == Some(LightMode::Temperature)
                    && !work.refresh.borrow().snapshot().problems.is_empty()
            })
            .await;
            peer.state.borrow_mut().hold_reads = true;
            work.refresh_property("light:2", BRIGHTNESS);
            until(|| {
                peer.state
                    .borrow()
                    .held
                    .iter()
                    .any(|(publish, _)| decode_mips(&publish.payload).unwrap().payload["piid"] == 2)
            })
            .await;
            peer.notify_value("one", MODE, json!(1), false);
            until(|| light.snapshot().lighting().unwrap().mode == Some(LightMode::Rgb)).await;
            assert_eq!(light.snapshot().lighting().unwrap().color, None);
            peer.release_property("one", BRIGHTNESS, json!({"value":50}))
                .await;
            until(|| light.snapshot().lighting().unwrap().brightness_percent == Some(50.0)).await;
        })
        .await;
    }));
}

#[test]
fn repeated_same_mode_coalesces_an_existing_missing_color_read() {
    for rgb in [true, false] {
        block_on(Rig::light("devcea.light.ls2307").run(|c| async move {
            c.announce();
            let peer = peer_values(&c);
            let active = if rgb { COLOR } else { TEMPERATURE };
            peer.value("one", active, json!(0x1000000));
            peer.value("one", MODE, json!(if rgb { 0 } else { 1 }));
            let control = &c;
            c.peer(peer, |peer| async move {
                until(|| control.context.active_notification("one") == Some(PathKind::Local)).await;
                let light = function(&control.context.registry, "one", "light:2");
                let work = control.context.live.borrow().devices["one"]
                    .admitted
                    .clone()
                    .unwrap();
                let key = super::super::super::observations::key("light:2", active);
                until(|| work.refresh.borrow().snapshot().problems.contains_key(&key)).await;
                peer.state.borrow_mut().hold_reads = true;
                peer.notify_value("one", MODE, json!(if rgb { 1 } else { 0 }), false);
                until(|| work.refresh.borrow().snapshot().inflight.contains(&key)).await;
                let stamp = work.observations.read_stamp("light:2", active);
                for _ in 0..3 {
                    peer.notify_value("one", MODE, json!(if rgb { 1 } else { 0 }), false);
                }
                for _ in 0..100 {
                    future::yield_now().await;
                }
                assert!(
                    work.observations
                        .read_is_current(&work, "light:2", active, &stamp)
                );
                assert!(
                    !work
                        .refresh
                        .borrow()
                        .snapshot()
                        .pending
                        .iter()
                        .any(|read| read.id == "light:2" && read.binding == active)
                );
                peer.release_property(
                    "one",
                    active,
                    json!({"value": if rgb { 0xabcdef } else { 4000 }}),
                )
                .await;
                until(|| {
                    if rgb {
                        light.snapshot().lighting().unwrap().color == Rgb::from_packed(0xabcdef)
                    } else {
                        light.snapshot().lighting().unwrap().temperature_kelvin == Some(4000)
                    }
                })
                .await;
            })
            .await;
        }));
    }
}

#[test]
fn a_changed_mode_retires_an_old_read_of_its_missing_active_color() {
    block_on(Rig::light("devcea.light.ls2307").run(|c| async move {
        c.announce();
        let peer = peer_values(&c);
        peer.value("one", COLOR, json!(0x1000000));
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| control.context.active_notification("one") == Some(PathKind::Local)).await;
            let light = function(&control.context.registry, "one", "light:2");
            let work = control.context.live.borrow().devices["one"]
                .admitted
                .clone()
                .unwrap();
            let key = super::super::super::observations::key("light:2", COLOR);
            until(|| work.refresh.borrow().snapshot().problems.contains_key(&key)).await;
            peer.state.borrow_mut().hold_reads = true;
            work.refresh_property("light:2", COLOR);
            until(|| work.refresh.borrow().snapshot().inflight.contains(&key)).await;
            let stamp = work.observations.read_stamp("light:2", COLOR);
            peer.notify_value("one", MODE, json!(1), false);
            until(|| light.snapshot().lighting().unwrap().mode == Some(LightMode::Rgb)).await;
            assert!(
                !work
                    .observations
                    .read_is_current(&work, "light:2", COLOR, &stamp)
            );
            peer.release_property("one", COLOR, json!({"value":0x456789}))
                .await;
            peer.release_property("one", COLOR, json!({"value":0xabcdef}))
                .await;
            until(|| light.snapshot().lighting().unwrap().color == Rgb::from_packed(0xabcdef))
                .await;
        })
        .await;
    }));
}

#[test]
fn an_old_exhausted_read_failure_cannot_remove_a_new_mode_refresh() {
    block_on(Rig::light("devcea.light.ls2307").run(|c| async move {
        c.announce();
        let peer = peer_values(&c);
        peer.value("one", COLOR, json!(0x1000000));
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| control.context.active_notification("one") == Some(PathKind::Local)).await;
            let light = function(&control.context.registry, "one", "light:2");
            let work = control.context.live.borrow().devices["one"]
                .admitted
                .clone()
                .unwrap();
            let key = super::super::super::observations::key("light:2", COLOR);
            until(|| work.refresh.borrow().snapshot().problems.contains_key(&key)).await;
            peer.state.borrow_mut().hold_reads = true;
            work.refresh_property("light:2", COLOR);
            work.refresh.borrow_mut().exhaust_pending("light:2", COLOR);
            until(|| {
                peer.state.borrow().held.iter().any(|(publish, _)| {
                    let payload = decode_mips(&publish.payload).unwrap().payload;
                    payload["siid"] == COLOR.siid && payload["piid"] == COLOR.piid
                })
            })
            .await;
            peer.notify_value("one", MODE, json!(1), false);
            until(|| light.snapshot().lighting().unwrap().mode == Some(LightMode::Rgb)).await;
            peer.release_property("one", COLOR, json!({"code":-1}))
                .await;
            for _ in 0..100 {
                future::yield_now().await;
            }
            assert!(
                work.refresh.borrow().snapshot().inflight.contains(&key)
                    || work
                        .refresh
                        .borrow()
                        .snapshot()
                        .pending
                        .iter()
                        .any(|read| read.id == "light:2" && read.binding == COLOR)
            );
            peer.release_property("one", COLOR, json!({"value":0xabcdef}))
                .await;
            until(|| light.snapshot().lighting().unwrap().color == Rgb::from_packed(0xabcdef))
                .await;
        })
        .await;
    }));
}

#[test]
fn unknown_mode_color_values_update_only_cache_until_the_mode_changes() {
    for rgb in [true, false] {
        block_on(Rig::light("devcea.light.ls2307").run(|c| async move {
            c.announce();
            let peer = peer_values(&c);
            peer.value("one", MODE, json!(2));
            let control = &c;
            c.peer(peer, |peer| async move {
                until(|| control.context.active_notification("one") == Some(PathKind::Local)).await;
                let light = function(&control.context.registry, "one", "light:2");
                let work = control.context.live.borrow().devices["one"]
                    .admitted
                    .clone()
                    .unwrap();
                until(|| {
                    work.refresh
                        .borrow()
                        .snapshot()
                        .problems
                        .contains_key(&super::super::super::observations::key("light:2", MODE))
                })
                .await;
                assert_eq!(light.snapshot().lighting().unwrap().mode, None);
                peer.state.borrow_mut().hold_writes = true;
                let action = light
                    .begin_light_action(
                        color_target(if rgb {
                            LightColorTarget::Rgb(Rgb::from_packed(0xabcdef).unwrap())
                        } else {
                            LightColorTarget::Temperature(4000.0)
                        }),
                        false,
                    )
                    .unwrap();
                until(|| peer.count("rpcReq", "one", 0) == 1).await;
                if rgb {
                    peer.notify_value("one", TEMPERATURE, json!(5000), false);
                } else {
                    peer.notify_value("one", COLOR, json!(0x456789), false);
                }
                until(|| {
                    if rgb {
                        light.snapshot().lighting().unwrap().temperature_kelvin == Some(5000)
                    } else {
                        light.snapshot().lighting().unwrap().color == Rgb::from_packed(0x456789)
                    }
                })
                .await;
                assert_eq!(light.snapshot().lighting().unwrap().mode, None);
                assert!(light.light_action_current(action.token()));
                peer.notify_value("one", MODE, json!(if rgb { 0 } else { 1 }), false);
                until(|| light.snapshot().lighting().unwrap().mode.is_some()).await;
                assert!(!light.light_action_current(action.token()));
                assert_eq!(
                    action.ended().await,
                    Err(crate::device::CommandError::Superseded)
                );
                let target = if rgb { COLOR } else { TEMPERATURE };
                peer.release_property("one", target, accepted_items(&[(target, 0), (MODE, 0)]))
                    .await;
                action.accepted().await.unwrap();
            })
            .await;
        }));
    }
}
