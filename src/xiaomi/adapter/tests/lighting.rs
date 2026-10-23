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
                let light = function(&control.context.registry, "one", "main");
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
            let light = function(&control.context.registry, "one", "main");
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

fn profile_instance(brightness: bool, temperature: bool) -> Value {
    let mut instance: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/xiaomi-light-ceil04.json"
    ))
    .unwrap();
    instance["services"][1]["properties"]
        .as_array_mut()
        .unwrap()
        .retain(|property| {
            let kind = property["type"].as_str().unwrap_or_default();
            kind.contains(":on:")
                || (brightness && kind.contains(":brightness:"))
                || (temperature && kind.contains(":color-temperature:"))
        });
    instance
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
                    let light = function(&control.context.registry, "one", "main");
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
                    let light = function(&control.context.registry, "one", "main");
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
fn stopping_the_source_clears_light_state_but_not_outlet_cache() {
    block_on(Rig::light("xiaomi.light.ceil04").run(|c| async move {
        c.announce();
        let peer = peer_values(&c);
        let control = &c;
        c.peer(peer, |peer| async move {
            let light = ready_light(control).await;
            assert!(
                light
                    .snapshot()
                    .lighting()
                    .unwrap()
                    .brightness_percent
                    .is_some()
            );
            control.context.stop();
            let snapshot = light.snapshot();
            assert!(matches!(
                snapshot.availability,
                Availability::Unavailable(_)
            ));
            assert_eq!(snapshot.power(), Some(Power::Unknown));
            let lighting = snapshot.lighting().unwrap();
            assert_eq!(lighting.brightness_percent, None);
            assert_eq!(lighting.temperature_kelvin, None);
            assert_eq!(lighting.color, None);
            assert_eq!(lighting.mode, Some(LightMode::Temperature));
            let _ = peer;
        })
        .await;
    }));
}

#[test]
fn narrower_supported_range_replaces_the_logical_light_while_keeping_identity() {
    block_on(Rig::light("xiaomi.light.ceil04").run(|c| async move {
        c.announce();
        let peer = peer_values(&c);
        let control = &c;
        c.peer(peer, |peer| async move {
            let key = DeviceKey::new("xiaomi", "one");
            let light = ready_light(control).await;
            assert_eq!(
                light.description().temperature_interval().unwrap().max(),
                5700
            );
            let before = control
                .store
                .devices()
                .load_configuration()
                .unwrap()
                .device(&key)
                .cloned()
                .unwrap();
            let original_physical = control.context.registry.get(&key).unwrap();
            let original = control.context.live.borrow().devices["one"]
                .admitted
                .as_ref()
                .unwrap()
                .native
                .clone();
            let mut narrower = original.clone();
            narrower = crate::xiaomi::capability::NativeDescription::new(
                narrower.model().to_owned(),
                narrower.instance_type().to_owned(),
                super::support::device_with_temperature_grid(
                    narrower.device(),
                    crate::device::NumericRange::new(2700, 5600, 1).unwrap(),
                ),
            )
            .unwrap();
            control
                .context
                .live
                .borrow_mut()
                .devices
                .get_mut("one")
                .unwrap()
                .latest = Some(narrower);
            control.context.reconcile("one").unwrap();

            // Wait for the actual instance replacement, not the still-serving
            // old available instance.
            let replaced = ready_replacement_light(control).await;
            assert_eq!(
                replaced.description().temperature_interval().unwrap().max(),
                5600
            );
            let physical = control.context.registry.get(&key).unwrap();
            assert_eq!(physical.availability(), Availability::Available);
            assert!(!physical.same_instance(&original_physical));
            let saved = control
                .store
                .devices()
                .load_configuration()
                .unwrap()
                .device(&key)
                .cloned()
                .unwrap();
            assert_eq!(saved.unique_id, before.unique_id);
            assert_eq!(saved.endpoint, before.endpoint);
            assert_eq!(saved.label, before.label);
            assert_eq!(
                saved.configuration_version,
                before.configuration_version + 1
            );
            assert_eq!(
                saved.native.as_ref().unwrap().device().functions()[0]
                    .lighting()
                    .unwrap()
                    .temperature()
                    .unwrap()
                    .range()
                    .max(),
                5600
            );
            let _ = peer;
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
    let bindings = c.data[0].device().functions()[0].lighting().unwrap();
    let brightness = bindings.brightness().unwrap();
    let temperature = bindings.temperature().unwrap();
    peer.value("one", POWER, json!(true));
    peer.value(
        "one",
        brightness.property(),
        json!(brightness.range().min()),
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
    peer
}
async fn ready_light(c: &Control) -> crate::device::Function {
    until(|| c.context.active_notification("one") == Some(PathKind::Local)).await;
    let function = function(&c.context.registry, "one", "main");
    until(|| {
        let snapshot = function.snapshot();
        let light = snapshot.lighting().unwrap();
        snapshot
            .power()
            .and_then(crate::device::Power::value)
            .is_some()
            && light.brightness_percent.is_some()
            && light.temperature_kelvin.is_some()
            && light.mode.is_some()
            && (c.data[0].device().functions()[0]
                .lighting()
                .unwrap()
                .color()
                .is_none()
                || light.color.is_some())
    })
    .await;
    function
}
fn brightness(percent: f64) -> LightTarget {
    LightTarget::brightness(None, percent).expect("valid test target")
}
fn color_target(color: LightColorTarget) -> LightTarget {
    match color {
        LightColorTarget::Temperature(kelvin) => {
            LightTarget::temperature(None, kelvin).expect("valid test target")
        }
        LightColorTarget::Rgb(rgb) => LightTarget::rgb(None, rgb),
    }
}
fn freeze_light_clock(c: &Control) {
    c.clock
        .automatic
        .store(false, std::sync::atomic::Ordering::Relaxed);
}
async fn ready_replacement_light(c: &Control) -> crate::device::Function {
    let before = c.context.registry.get(&DeviceKey::new("xiaomi", "one"));
    future::or(
        async {
            until(|| {
                c.context
                    .registry
                    .get(&DeviceKey::new("xiaomi", "one"))
                    .is_some_and(|now| before.as_ref().is_none_or(|old| !old.same_instance(&now)))
                    && c.context.active_notification("one")
                        == Some(super::super::model::PathKind::Local)
            })
            .await;
            ready_light(c).await
        },
        async {
            // Replacement waits 10 ms for subscriptions, then reads one property
            // per 200 ms round. Bound recovery below the accepted feedback window.
            for _ in 0..100 {
                async_io::Timer::after(Duration::from_millis(1)).await;
                c.clock.advance(Duration::from_millis(10));
            }
            panic!("replacement light did not become ready within one simulated second");
        },
    )
    .await
}
async fn received_light_frame(c: &Control, target: LightTarget) {
    until(|| {
        c.context.diagnostics().devices[0].commands[0]
            .last
            .as_ref()
            .is_some_and(|status| {
                status.target == super::super::model::CommandTarget::Light(target)
                    && status.result == Some(Ok(()))
            })
    })
    .await;
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
            let old_read = work.observations.read_stamp("main", BRIGHTNESS);
            light
                .begin_light_action(brightness(80.0), true)
                .unwrap()
                .accepted()
                .await
                .unwrap();
            // Neither a read started before the receipt nor a cached report
            // proves that the displayed value still matches the device.
            work.observe_read_at("main", BRIGHTNESS, &old_read, &json!(32768), None, 0)
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
            until(|| !work.observations.has_unconfirmed_write("main", BRIGHTNESS)).await;
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
fn binding_change_retires_old_feedback_authority() {
    block_on(Rig::light("xiaomi.light.ceil04").run(|c| async move {
        c.announce();
        let peer = peer_values(&c);
        let control = &c;
        c.peer(peer, |peer| async move {
            let old = ready_light(control).await;
            freeze_light_clock(control);
            peer.state.borrow_mut().hold_writes = true;
            let action = old.begin_light_action(brightness(60.0), false).unwrap();
            until(|| peer.count("rpcReq", "one", 0) == 1).await;
            let mut changed = control.context.live.borrow().devices["one"]
                .admitted
                .as_ref()
                .unwrap()
                .native
                .clone();
            changed = crate::xiaomi::capability::NativeDescription::new(
                changed.model().to_owned(),
                changed.instance_type().to_owned(),
                super::support::device_with_brightness_format(
                    changed.device(),
                    crate::xiaomi::capability::NumericFormat::Uint16,
                ),
            )
            .unwrap();
            control
                .context
                .live
                .borrow_mut()
                .devices
                .get_mut("one")
                .unwrap()
                .latest = Some(changed);
            control.context.reconcile("one").unwrap();
            let new = ready_replacement_light(control).await;
            assert_eq!(
                new.description().temperature_interval().unwrap().max(),
                5700
            );
            assert!(!old.light_action_current(action.token()));
            let next = new.begin_light_action(brightness(70.0), false).unwrap();
            async_io::Timer::after(std::time::Duration::from_millis(30)).await;
            assert_eq!(peer.count("rpcReq", "one", 0), 1);
            assert!(poll_once(next.accepted()).await.is_none());
            assert!(poll_once(action.accepted()).await.is_none());
            // The old receipt lands after the binding authority was replaced;
            // it must not re-enter the new accepted history.
            peer.release_property("one", BRIGHTNESS, accepted_items(&[(BRIGHTNESS, 0)]))
                .await;
            action.accepted().await.unwrap();
            until(|| peer.count("rpcReq", "one", 0) == 2).await;
            peer.release_property("one", BRIGHTNESS, accepted_items(&[(BRIGHTNESS, 0)]))
                .await;
            next.accepted().await.unwrap();
            assert_eq!(
                new.snapshot().lighting().unwrap().brightness_percent,
                Some(1.0)
            );
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
fn retired_work_replacement_retires_old_feedback_authority() {
    block_on(Rig::light("xiaomi.light.ceil04").run(|c| async move {
        c.announce();
        let peer = peer_values(&c);
        let control = &c;
        c.peer(peer, |peer| async move {
            let old = ready_light(control).await;
            freeze_light_clock(control);
            let first = old.begin_light_action(brightness(60.0), true).unwrap();
            first.accepted().await.unwrap();
            let accepted_at = control.clock.now();
            first.ended().await.unwrap();
            drop(first);
            assert_eq!(peer.count("rpcReq", "one", 2), 1);
            assert_eq!(
                old.snapshot().lighting().unwrap().brightness_percent,
                Some(1.0)
            );
            drop(old);

            let old_work = control.context.live.borrow().devices["one"]
                .admitted
                .clone()
                .unwrap();
            let retired_work = Rc::downgrade(&old_work);
            let mut changed = old_work.native.clone();
            changed = crate::xiaomi::capability::NativeDescription::new(
                changed.model().to_owned(),
                changed.instance_type().to_owned(),
                super::support::device_with_temperature_grid(
                    changed.device(),
                    crate::device::NumericRange::new(3000, 5500, 1).unwrap(),
                ),
            )
            .unwrap();
            drop(old_work);
            peer.value("one", TEMPERATURE, json!(4000));
            control
                .context
                .live
                .borrow_mut()
                .devices
                .get_mut("one")
                .unwrap()
                .latest = Some(changed);
            control.context.reconcile("one").unwrap();
            let new = ready_replacement_light(control).await;
            assert_eq!(
                new.description().temperature_interval().unwrap().max(),
                5500
            );
            assert!(retired_work.upgrade().is_none());
            let next = new.begin_light_action(brightness(70.0), false).unwrap();
            next.accepted().await.unwrap();
            assert_eq!(peer.count("rpcReq", "one", 2), 2);
            assert_eq!(
                new.snapshot().lighting().unwrap().brightness_percent,
                Some(1.0)
            );
            // The replaced binding authority retires the old accepted
            // history, so a later old-value report is an external operation.
            control
                .clock
                .advance(accepted_at + Duration::from_millis(2999) - control.clock.now());
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
            assert_eq!(peer.count("rpcReq", "one", 2), 2);
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
            let slot = super::super::state::DeviceSlot {
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
                    let stamp = work.observations.read_stamp("main", BRIGHTNESS);
                    work.observe_read("main", BRIGHTNESS, &stamp, &json!(value))
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
            let light = function(&c.context.registry, "one", "main");
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
        let light = function(&c.context.registry, "one", "main");
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
            let light = function(&control.context.registry, "one", "main");
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
            let light = function(&control.context.registry, "one", "main");
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
                let light = function(&control.context.registry, "one", "main");
                let work = control.context.live.borrow().devices["one"]
                    .admitted
                    .clone()
                    .unwrap();
                let key = super::super::lighting::key("main", active);
                until(|| work.refresh.borrow().snapshot().problems.contains_key(&key)).await;
                peer.state.borrow_mut().hold_reads = true;
                peer.notify_value("one", MODE, json!(if rgb { 1 } else { 0 }), false);
                until(|| work.refresh.borrow().snapshot().inflight.contains(&key)).await;
                let stamp = work.observations.read_stamp("main", active);
                for _ in 0..3 {
                    peer.notify_value("one", MODE, json!(if rgb { 1 } else { 0 }), false);
                }
                for _ in 0..100 {
                    future::yield_now().await;
                }
                assert!(
                    work.observations
                        .read_is_current(&work, "main", active, &stamp)
                );
                assert!(
                    !work
                        .refresh
                        .borrow()
                        .snapshot()
                        .pending
                        .iter()
                        .any(|read| read.id == "main" && read.binding == active)
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
            let light = function(&control.context.registry, "one", "main");
            let work = control.context.live.borrow().devices["one"]
                .admitted
                .clone()
                .unwrap();
            let key = super::super::lighting::key("main", COLOR);
            until(|| work.refresh.borrow().snapshot().problems.contains_key(&key)).await;
            peer.state.borrow_mut().hold_reads = true;
            work.refresh_property("main", COLOR);
            until(|| work.refresh.borrow().snapshot().inflight.contains(&key)).await;
            let stamp = work.observations.read_stamp("main", COLOR);
            peer.notify_value("one", MODE, json!(1), false);
            until(|| light.snapshot().lighting().unwrap().mode == Some(LightMode::Rgb)).await;
            assert!(
                !work
                    .observations
                    .read_is_current(&work, "main", COLOR, &stamp)
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
            let light = function(&control.context.registry, "one", "main");
            let work = control.context.live.borrow().devices["one"]
                .admitted
                .clone()
                .unwrap();
            let key = super::super::lighting::key("main", COLOR);
            until(|| work.refresh.borrow().snapshot().problems.contains_key(&key)).await;
            peer.state.borrow_mut().hold_reads = true;
            work.refresh_property("main", COLOR);
            work.refresh.borrow_mut().exhaust_pending("main", COLOR);
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
                        .any(|read| read.id == "main" && read.binding == COLOR)
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
        let old = function(&c.context.registry, "one", "main");
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
            super::support::device_with_brightness_format(
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
        let new = function(&c.context.registry, "one", "main");
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
                let light = function(&control.context.registry, "one", "main");
                let work = control.context.live.borrow().devices["one"]
                    .admitted
                    .clone()
                    .unwrap();
                until(|| {
                    work.refresh
                        .borrow()
                        .snapshot()
                        .problems
                        .contains_key(&super::super::lighting::key("main", MODE))
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
