//! Synthetic multi-control inputs exercise real source routing and feedback.
use super::*;
use crate::xiaomi::capability::PropertyBinding;

fn instance() -> Value {
    let light = |siid| {
        json!({
            "iid": siid, "type": "urn:miot-spec-v2:service:light:00007802:example:1",
            "properties": [{"iid": 1, "type": "urn:miot-spec-v2:property:on:00000006:example:1", "format": "bool", "access": ["read", "write", "notify"]}]
        })
    };
    json!({"type": "urn:miot-spec-v2:device:air-conditioner:0000A004:example:1", "services": [
        light(2),
        {"iid": 3, "type": "urn:miot-spec-v2:service:fan:00007808:example:1", "properties": [
            {"iid": 1, "type": "urn:miot-spec-v2:property:on:00000006:example:1", "format": "bool", "access": ["read", "write", "notify"]},
            {"iid": 2, "type": "urn:miot-spec-v2:property:fan-level:00000016:example:1", "format": "uint8", "access": ["read", "write", "notify"], "value-range": [1, 3, 1]}
        ]},
        light(4)
    ]})
}

#[test]
fn terminal_describes_optional_recognition_failure_separately_from_connectivity() {
    let mut input = instance();
    input["services"][1]["properties"]
        .as_array_mut()
        .unwrap()
        .push(json!({
            "iid": 3, "type": "urn:miot-spec-v2:property:horizontal-swing:00000017:example:1",
            "format": "bool", "access": ["read", "write"]
        }));
    block_on(
        Rig::custom_lights(&[("one", "example.composite", input)]).run(|c| async move {
            c.announce();
            let peer = Rc::new(Peer::default());
            for (binding, value) in [
                (PropertyBinding { siid: 2, piid: 1 }, json!(false)),
                (PropertyBinding { siid: 3, piid: 1 }, json!(true)),
                (PropertyBinding { siid: 3, piid: 2 }, json!(1)),
                (PropertyBinding { siid: 4, piid: 1 }, json!(false)),
            ] {
                peer.value("one", binding, value);
            }
            let control = &c;
            c.peer(peer, |_peer| async move {
                until(|| control.context.active_notification("one") == Some(PathKind::Local)).await;
                let status = crate::terminal::AuthStatus::new(
                    control.context.status.borrow().authentication.clone(),
                    "/bin/migate".into(),
                    "/data".into(),
                );
                status.update_adapter(control.context.diagnostics());
                let text = crate::terminal::bridge::Context::committed(
                    control.publication.clone(),
                    &status,
                )
                .handle_line("status one", 0)
                .await
                .unwrap()
                .unwrap();
                assert!(
                    text.contains("fan:3: additional; capabilities on/off, fan"),
                    "{text}"
                );
                assert!(
                    text.contains("Recognition issue fan:3: horizontal swing")
                        && text.contains("notify"),
                    "{text}"
                );
                assert!(!text.contains("Not connected fan:3"), "{text}");
                assert!(text.contains("Notification source: local"), "{text}");
            })
            .await;
        }),
    );
}

#[test]
fn composite_controls_route_commands_and_feedback_to_their_own_bindings() {
    block_on(
        Rig::custom_lights(&[("one", "example.composite", instance())]).run(|c| async move {
            c.announce();
            let peer = Rc::new(Peer::default());
            for (binding, value) in [
                (PropertyBinding { siid: 2, piid: 1 }, json!(false)),
                (PropertyBinding { siid: 3, piid: 1 }, json!(true)),
                (PropertyBinding { siid: 3, piid: 2 }, json!(1)),
                (PropertyBinding { siid: 4, piid: 1 }, json!(false)),
            ] {
                peer.value("one", binding, value);
            }
            let control = &c;
            c.peer(peer, |peer| async move {
                until(|| control.context.active_notification("one") == Some(PathKind::Local)).await;
                let first = function(&control.context.registry, "one", "light:2");
                let fan = function(&control.context.registry, "one", "fan:3");
                let second = function(&control.context.registry, "one", "light:4");
                until(|| {
                    first.snapshot().power().is_some()
                        && fan.snapshot().fan().unwrap().speed.value.is_some()
                        && second.snapshot().power().is_some()
                })
                .await;
                let status = crate::terminal::AuthStatus::new(
                    control.context.status.borrow().authentication.clone(),
                    "/bin/migate".into(),
                    "/data".into(),
                );
                let terminal = crate::terminal::bridge::Context::committed(
                    control.publication.clone(),
                    &status,
                );
                for command in ["on one light:2", "off one fan:3", "on one light:4"] {
                    assert!(
                        terminal
                            .handle_line(command, 0)
                            .await
                            .unwrap()
                            .unwrap()
                            .contains("command accepted")
                    );
                }
                until(|| {
                    peer.state
                        .borrow()
                        .seen
                        .iter()
                        .filter(|(op, _)| op == "rpcReq")
                        .count()
                        >= 3
                })
                .await;
                let writes = peer
                    .state
                    .borrow()
                    .seen
                    .iter()
                    .filter(|(op, _)| op == "rpcReq")
                    .flat_map(|(_, value)| {
                        value["rpc"]["params"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|item| {
                                (
                                    item["siid"].as_u64().unwrap(),
                                    item["piid"].as_u64().unwrap(),
                                    item["value"].clone(),
                                )
                            })
                            .collect::<Vec<_>>()
                    })
                    .collect::<Vec<_>>();
                assert!(writes.contains(&(2, 1, json!(true))), "{writes:?}");
                assert!(writes.contains(&(3, 1, json!(false))), "{writes:?}");
                assert!(writes.contains(&(4, 1, json!(true))), "{writes:?}");
                peer.notify_value(
                    "one",
                    PropertyBinding { siid: 2, piid: 1 },
                    json!(true),
                    false,
                );
                peer.notify_value("one", PropertyBinding { siid: 3, piid: 2 }, json!(3), false);
                peer.notify_value(
                    "one",
                    PropertyBinding { siid: 4, piid: 1 },
                    json!(false),
                    false,
                );
                until(|| {
                    first.snapshot().power() == Some(crate::device::Power::On)
                        && fan.snapshot().fan().unwrap().speed.value == Some(3)
                        && second.snapshot().power() == Some(crate::device::Power::Off)
                })
                .await;
                let current = control.publication.current();
                let saved = current
                    .configuration
                    .device(&DeviceKey::new("xiaomi", "one"))
                    .unwrap();
                assert_eq!(saved.functions.len(), 3);
                assert!(saved.functions.iter().all(|f| f.endpoint != saved.endpoint));
                assert_eq!(current.derived.visible_functions(&saved.key).len(), 3);
            })
            .await;
        }),
    );
}

#[test]
fn stopping_one_light_does_not_cancel_another_lights_queued_action() {
    let mut input = instance();
    for service in input["services"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .filter(|service| service["iid"] == 2 || service["iid"] == 4)
    {
        service["properties"].as_array_mut().unwrap().push(json!({
            "iid": 2, "type": "urn:miot-spec-v2:property:brightness:0000000D:example:1", "format": "uint8",
            "access": ["read", "write", "notify"], "unit": "percentage", "value-range": [1, 100, 1]
        }));
    }
    block_on(
        Rig::custom_lights(&[("one", "example.composite", input)]).run(|c| async move {
            c.announce();
            let peer = Rc::new(Peer::default());
            for (binding, value) in [
                (PropertyBinding { siid: 2, piid: 1 }, json!(true)),
                (PropertyBinding { siid: 2, piid: 2 }, json!(30)),
                (PropertyBinding { siid: 3, piid: 1 }, json!(true)),
                (PropertyBinding { siid: 3, piid: 2 }, json!(1)),
                (PropertyBinding { siid: 4, piid: 1 }, json!(true)),
                (PropertyBinding { siid: 4, piid: 2 }, json!(30)),
            ] {
                peer.value("one", binding, value);
            }
            let control = &c;
            c.peer(peer, |peer| async move {
                let first = function(&control.context.registry, "one", "light:2");
                let second = function(&control.context.registry, "one", "light:4");
                until(|| {
                    first
                        .snapshot()
                        .lighting()
                        .unwrap()
                        .brightness_percent
                        .is_some()
                        && second
                            .snapshot()
                            .lighting()
                            .unwrap()
                            .brightness_percent
                            .is_some()
                })
                .await;
                peer.state.borrow_mut().hold_writes = true;
                let first_action = first
                    .begin_light_action(
                        crate::device::LightTarget::brightness(None, 80.0).unwrap(),
                        false,
                    )
                    .unwrap();
                until(|| {
                    peer.state.borrow().held.iter().any(|(publish, _)| {
                        decode_mips(&publish.payload).unwrap().payload["rpc"]["params"][0]["siid"]
                            == 2
                    })
                })
                .await;
                let second_action = second
                    .begin_light_action(
                        crate::device::LightTarget::brightness(None, 60.0).unwrap(),
                        false,
                    )
                    .unwrap();
                first
                    .stop_light(crate::device::LightDimension::Brightness)
                    .unwrap();
                assert_eq!(
                    first_action.ended().await.unwrap_err(),
                    crate::device::CommandError::Stopped
                );
                let release =
                    |siid| json!({"result": [{"did": "one", "siid": siid, "piid": 2, "code": 0}]});
                peer.release_property("one", PropertyBinding { siid: 2, piid: 2 }, release(2))
                    .await;
                peer.release_property("one", PropertyBinding { siid: 4, piid: 2 }, release(4))
                    .await;
                second_action.accepted().await.unwrap();
            })
            .await;
        }),
    );
}

#[test]
fn core_metadata_can_change_without_rebuilding_source_or_matter() {
    let input = instance();
    block_on(
        Rig::custom_lights(&[("one", "example.composite", input.clone())]).run(|c| async move {
            c.announce();
            let peer = Rc::new(Peer::default());
            for (binding, value) in [
                (PropertyBinding { siid: 2, piid: 1 }, json!(false)),
                (PropertyBinding { siid: 3, piid: 1 }, json!(true)),
                (PropertyBinding { siid: 3, piid: 2 }, json!(1)),
                (PropertyBinding { siid: 4, piid: 1 }, json!(false)),
            ] {
                peer.value("one", binding, value);
            }
            let control = &c;
            c.peer(peer, |peer| async move {
                until(|| {
                    peer.count("subscribe", "one", 2) == 1
                        && peer.count("subscribe", "one", 3) == 2
                        && peer.count("subscribe", "one", 4) == 1
                })
                .await;
                let key = DeviceKey::new("xiaomi", "one");
                let physical = control.context.registry.get(&key).unwrap();
                let before = control.publication.current();
                let old = before.configuration.device(&key).unwrap().clone();
                assert!(
                    old.native
                        .as_ref()
                        .unwrap()
                        .device()
                        .recognition()
                        .unwrap()
                        .core()
                        .is_empty()
                );
                let subscriptions = peer
                    .state
                    .borrow()
                    .seen
                    .iter()
                    .filter(|(op, _)| op == "subscribe")
                    .count();
                let mut updated = input;
                updated["type"] = json!("urn:miot-spec-v2:device:light:0000A001:example:1");
                let parsed = crate::xiaomi::capability::parse_device_for_model(
                    key.clone(),
                    "example.composite",
                    &updated,
                )
                .unwrap();
                let native = crate::xiaomi::capability::NativeDescription::new(
                    "example.composite".into(),
                    updated["type"].as_str().unwrap().into(),
                    parsed,
                )
                .unwrap();
                control
                    .context
                    .live
                    .borrow_mut()
                    .devices
                    .get_mut("one")
                    .unwrap()
                    .latest = Some(native);
                control.context.reconcile("one").unwrap();
                until(|| {
                    control
                        .publication
                        .current()
                        .configuration
                        .device(&key)
                        .unwrap()
                        .native
                        .as_ref()
                        .unwrap()
                        .device()
                        .recognition()
                        .unwrap()
                        .category()
                        == "light"
                })
                .await;
                let current = control.publication.current();
                let new = current.configuration.device(&key).unwrap();
                assert!(physical.same_instance(&control.context.registry.get(&key).unwrap()));
                assert_eq!(new.functions, old.functions);
                assert_eq!(new.configuration_version, old.configuration_version);
                assert_eq!(
                    current.configuration.root_version,
                    before.configuration.root_version
                );
                assert_eq!(
                    new.native
                        .as_ref()
                        .unwrap()
                        .device()
                        .recognition()
                        .unwrap()
                        .core(),
                    ["light:2", "light:4"]
                );
                assert_eq!(
                    peer.state
                        .borrow()
                        .seen
                        .iter()
                        .filter(|(op, _)| op == "subscribe")
                        .count(),
                    subscriptions
                );
            })
            .await;
        }),
    );
}
