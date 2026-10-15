use super::*;

fn group_record(record_did: &str, group_did: &str, members: &[&str], physical: bool) -> Value {
    let member_ship = members
        .iter()
        .map(|did| ((*did).to_string(), serde_json::json!("1")))
        .collect::<serde_json::Map<_, _>>();
    serde_json::json!({
        "did": record_did,
        "name": "Native group",
        "model": "mijia.light.group3",
        "spec_type": "urn:miot-spec-v2:device:light:0000A001:mijia-group3:3:0000C802",
        "isOnline": true,
        "extra": {
            "member_list": {
                "group_did": group_did,
                "status": "1",
                "display_member_switch": 0,
                "member_ship": member_ship,
                "is_physical": physical
            }
        }
    })
}

#[test]
fn first_binding_applies_the_initial_catalog_without_a_cached_scope() {
    let rig = Rig::new(false, &["one"]);
    rig.cloud.lock().unwrap().catalog_groups =
        vec![group_record("group-record", "group-1", &["one"], false)];
    block_on(rig.run(|control| async move {
        control.announce();
        until(|| control.cloud.lock().unwrap().homes > 0).await;
        until(|| {
            control
                .context
                .relations
                .borrow()
                .member_watches()
                .contains_key("one")
        })
        .await;
        assert!(control.context.live.borrow().scope.is_some());
        assert_eq!(control.cloud.lock().unwrap().catalogs, 1);
    }));
}

#[test]
fn missing_description_member_group_is_admitted_and_cloud_owned() {
    let rig = Rig::new(false, &["one"]);
    let instance: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/group3-instance.json"
    ))
    .unwrap();
    let instance_type = instance["type"].as_str().unwrap().to_string();
    {
        let mut cloud = rig.cloud.lock().unwrap();
        // A home member that never receives its own description record.
        cloud.members.push("member-a".into());
        cloud.catalog_groups = vec![group_record(
            "group-record",
            "group-1",
            &["member-a"],
            false,
        )];
        cloud.catalog_instances.insert(instance_type, instance);
    }
    block_on(rig.run(|c| async move {
        c.announce();
        until(|| c.cloud.lock().unwrap().homes > 0).await;
        until(|| {
            c.context
                .live
                .borrow()
                .devices
                .get("group-1")
                .is_some_and(|slot| slot.admitted.is_some())
        })
        .await;
        {
            let live = c.context.live.borrow();
            let slot = &live.devices["group-1"];
            let record = slot.admitted.as_ref().unwrap();
            let cached = record.cached.borrow();
            assert_eq!(cached.model, "mijia.light.group3");
            let light = cached
                .device
                .functions
                .iter()
                .find(|function| function.lighting.is_some())
                .unwrap();
            assert!(
                light
                    .description
                    .capabilities
                    .iter()
                    .any(|capability| matches!(capability, crate::device::Capability::Brightness))
            );
            assert!(light.description.capabilities.iter().any(|capability| {
                matches!(capability, crate::device::Capability::ColorTemperature(_))
            }));
            // The member has no description and no source of its own.
            assert!(live.devices["member-a"].admitted.is_none());
            // Virtual group is cloud-only: no local session work for its DID.
            assert!(
                live.session
                    .as_ref()
                    .is_none_or(|session| !session.devices.contains_key("group-1"))
            );
        }
        assert!(matches!(
            c.context.control_path("group-1"),
            Some(super::routing::ControlPath::Cloud(_))
        ));
        let first = c.context.live.borrow().devices["group-1"]
            .admitted
            .clone()
            .unwrap();
        assert_eq!(first.physical.label(), "Native group");
        // A repeated complete directory must preserve the native instance.
        c.context.request_sync();
        until(|| c.cloud.lock().unwrap().catalogs >= 2).await;
        {
            let live = c.context.live.borrow();
            let slot = &live.devices["group-1"];
            assert!(std::rc::Rc::ptr_eq(slot.admitted.as_ref().unwrap(), &first));
            assert!(slot.cloud_access.is_some());
        }
        // A failed account catalog keeps confirmed groups and their access.
        c.cloud.lock().unwrap().catalog_status = 503;
        c.context.request_sync();
        until(|| c.cloud.lock().unwrap().catalogs >= 3).await;
        {
            let live = c.context.live.borrow();
            let slot = &live.devices["group-1"];
            assert!(std::rc::Rc::ptr_eq(slot.admitted.as_ref().unwrap(), &first));
            assert!(slot.cloud_access.is_some());
        }
    }));
}

#[test]
fn native_group_write_uses_its_own_did_and_read_owns_state() {
    let rig = Rig::new(false, &["one"]);
    let instance: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/group3-instance.json"
    ))
    .unwrap();
    let instance_type = instance["type"].as_str().unwrap().to_string();
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.members.push("member-a".into());
        cloud.catalog_groups = vec![group_record(
            "group-record",
            "group-1",
            &["member-a"],
            false,
        )];
        cloud.catalog_instances.insert(instance_type, instance);
    }
    let (release_read, read_gate) = flume::bounded::<()>(2);
    block_on(rig.run(|c| async move {
        c.announce();
        until(|| c.cloud.lock().unwrap().homes > 0).await;
        until(|| {
            c.context
                .live
                .borrow()
                .devices
                .get("group-1")
                .is_some_and(|slot| slot.admitted.is_some())
        })
        .await;
        let (id, power) = {
            let live = c.context.live.borrow();
            let record = live.devices["group-1"].admitted.as_ref().unwrap();
            let cached = record.cached.borrow();
            let function = cached
                .device
                .functions
                .iter()
                .find(|function| function.lighting.is_some())
                .unwrap();
            (function.description.id.clone(), function.power)
        };
        let light = function(&c.context.registry, "group-1", &id);
        until(|| light.snapshot().power == Power::Off).await;
        let key = ("group-1".to_string(), power.siid, power.piid);
        let reads = || {
            c.cloud
                .lock()
                .unwrap()
                .property_reads
                .iter()
                .filter(|key| key.0 == "group-1" && key.1 == power.siid && key.2 == power.piid)
                .count()
        };
        let before = reads();
        {
            let mut cloud = c.cloud.lock().unwrap();
            cloud.read_values.insert(key.clone(), true);
            cloud.read_gates.insert(key.clone(), read_gate);
        }
        // A genuine gated HTTP read is already in flight before the write.
        let work = c.context.live.borrow().devices["group-1"]
            .work
            .clone()
            .unwrap();
        work.request_dirty_read(&id, power);
        until(|| reads() == before + 1).await;
        light.execute(Command::On).await.unwrap();
        // The accepted target must not become own state before a real read.
        assert_eq!(light.snapshot().power, Power::Off);
        // The follow-up read sees a different actual value than the in-flight one.
        c.cloud
            .lock()
            .unwrap()
            .read_values
            .insert(key.clone(), false);
        release_read.send(()).unwrap();
        until(|| light.snapshot().power == Power::On).await;
        until(|| reads() == before + 2).await;
        release_read.send(()).unwrap();
        until(|| light.snapshot().power == Power::Off).await;
        let write = c
            .cloud
            .lock()
            .unwrap()
            .numeric_writes
            .iter()
            .find(|(items, _)| items[0]["did"] == "group-1")
            .cloned()
            .expect("native group write");
        assert_eq!(write.0[0]["siid"].as_u64().unwrap() as u32, power.siid);
        assert_eq!(write.0[0]["piid"].as_u64().unwrap() as u32, power.piid);
        assert_eq!(write.0[0]["value"], json!(true));
        // Exactly one follow-up: no further native read after the pair settles.
        for _ in 0..50 {
            future::yield_now().await;
        }
        assert_eq!(reads(), before + 2);
        // A failing own read keeps the last applied state, never the target.
        // The failure is per-property while the account route stays usable.
        {
            let mut cloud = c.cloud.lock().unwrap();
            cloud.read_gates.remove(&key);
            cloud.raw_values.insert(key.clone(), json!("invalid"));
        }
        // CloudFirst records the failed read on its first attempt; the route
        // stays usable because only the property value is invalid.
        c.context
            .priority
            .set(super::routing::PathPriority::CloudFirst);
        let attempts = reads();
        light.execute(Command::On).await.unwrap();
        until(|| reads() > attempts && !work.read_problems.borrow().is_empty()).await;
        assert_eq!(light.snapshot().power, Power::Off);
        c.context
            .priority
            .set(super::routing::PathPriority::LocalFirst);
    }));
}

#[test]
fn native_compound_target_requires_every_property_and_readback_owns_state() {
    let rig = Rig::new(false, &["one"]);
    let instance: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/group3-instance.json"
    ))
    .unwrap();
    let instance_type = instance["type"].as_str().unwrap().to_string();
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.members.push("member-a".into());
        cloud.catalog_groups = vec![group_record(
            "group-record",
            "group-1",
            &["member-a"],
            false,
        )];
        cloud.catalog_instances.insert(instance_type, instance);
        for (piid, value) in [(1, json!(false)), (2, json!(50)), (3, json!(2700))] {
            cloud.raw_values.insert(("group-1".into(), 2, piid), value);
        }
    }
    block_on(rig.run(|c| async move {
        c.announce();
        until(|| c.cloud.lock().unwrap().homes > 0).await;
        until(|| {
            c.context
                .live
                .borrow()
                .devices
                .get("group-1")
                .is_some_and(|slot| slot.admitted.is_some())
        })
        .await;
        let (id, power, brightness) = {
            let live = c.context.live.borrow();
            let record = live.devices["group-1"].admitted.as_ref().unwrap();
            let cached = record.cached.borrow();
            let light = cached
                .device
                .functions
                .iter()
                .find(|function| function.lighting.is_some())
                .unwrap();
            let bindings = light.lighting.as_ref().unwrap();
            (
                light.description.id.clone(),
                light.power,
                bindings.brightness.as_ref().unwrap().property,
            )
        };
        let light = function(&c.context.registry, "group-1", &id);
        until(|| light.snapshot().power == Power::Off).await;
        let target = || crate::device::LightTarget {
            power: Some(true),
            brightness_percent: Some(70.0),
            ..Default::default()
        };
        // Per-property results: power accepted, brightness rejected. The whole
        // action fails and no optimistic state appears.
        c.cloud.lock().unwrap().property_codes.insert(
            ("group-1".into(), brightness.siid, brightness.piid),
            -704_030_013,
        );
        let action = light
            .begin_light_action(crate::device::LightDimension::Brightness, target(), true)
            .unwrap();
        assert!(action.accepted().await.is_err());
        assert_eq!(light.snapshot().power, Power::Off);
        {
            let cloud = c.cloud.lock().unwrap();
            let (items, _) = cloud
                .numeric_writes
                .iter()
                .find(|(items, _)| items[0]["did"] == "group-1")
                .expect("native group compound write");
            assert_eq!(items.len(), 2);
            assert!(items.iter().all(|item| item["did"] == "group-1"));
            assert_eq!(items[0]["siid"], power.siid);
            assert_eq!(items[0]["piid"], power.piid);
        }
        // All properties accepted while the readback is held: acceptance is
        // independent of actual fields, which are never optimistic targets.
        c.cloud.lock().unwrap().property_codes.clear();
        let (release, gate) = flume::bounded::<()>(2);
        {
            let mut cloud = c.cloud.lock().unwrap();
            cloud
                .read_gates
                .insert(("group-1".into(), power.siid, power.piid), gate.clone());
            cloud
                .read_gates
                .insert(("group-1".into(), brightness.siid, brightness.piid), gate);
            cloud
                .raw_values
                .insert(("group-1".into(), power.siid, power.piid), json!(true));
        }
        let action = light
            .begin_light_action(crate::device::LightDimension::Brightness, target(), true)
            .unwrap();
        action.accepted().await.unwrap();
        assert_eq!(light.snapshot().power, Power::Off);
        release.send(()).unwrap();
        release.send(()).unwrap();
        until(|| light.snapshot().power == Power::On).await;
    }));
}

#[test]
fn failed_membership_keeps_relation_while_offline_own_source_clears_state() {
    let rig = Rig::new(false, &["one"]);
    let instance: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/group3-instance.json"
    ))
    .unwrap();
    let instance_type = instance["type"].as_str().unwrap().to_string();
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.members.push("member-a".into());
        cloud.catalog_groups = vec![group_record(
            "group-record",
            "group-1",
            &["member-a"],
            false,
        )];
        cloud.catalog_instances.insert(instance_type, instance);
        for (piid, value) in [(1, json!(true)), (2, json!(50)), (3, json!(2700))] {
            cloud.raw_values.insert(("group-1".into(), 2, piid), value);
        }
    }
    block_on(rig.run(|c| async move {
        c.announce();
        until(|| c.cloud.lock().unwrap().homes > 0).await;
        until(|| {
            c.context
                .live
                .borrow()
                .devices
                .get("group-1")
                .is_some_and(|slot| slot.admitted.is_some())
        })
        .await;
        let (id, admitted) = {
            let live = c.context.live.borrow();
            let slot = &live.devices["group-1"];
            (
                slot.admitted
                    .as_ref()
                    .unwrap()
                    .cached
                    .borrow()
                    .device
                    .functions[0]
                    .description
                    .id
                    .clone(),
                slot.admitted.clone().unwrap(),
            )
        };
        let light = function(&c.context.registry, "group-1", &id);
        until(|| light.snapshot().power == Power::On).await;
        // The record stays present, its own online turns false and the member
        // list becomes invalid: the last confirmed relation is retained.
        {
            let mut cloud = c.cloud.lock().unwrap();
            cloud.catalog_groups[0]["isOnline"] = json!(false);
            cloud.catalog_groups[0]["extra"] = json!({});
        }
        c.context.request_sync();
        // Wait for the actually applied metadata; a removed slot is reported
        // by the assertions below instead of passing a request counter.
        until(|| {
            c.context
                .live
                .borrow()
                .devices
                .get("group-1")
                .is_none_or(|slot| slot.group.as_ref().is_some_and(|group| !group.online))
        })
        .await;
        {
            let live = c.context.live.borrow();
            let slot = live
                .devices
                .get("group-1")
                .expect("retained relation keeps the group slot");
            let group = slot.group.as_ref().expect("retained relation");
            assert!(group.members.iter().any(|member| member == "member-a"));
            assert!(std::rc::Rc::ptr_eq(
                slot.admitted.as_ref().unwrap(),
                &admitted
            ));
        }
        assert!(c.context.control_path("group-1").is_none());
        assert!(matches!(
            light.snapshot().availability,
            Availability::Unavailable(_)
        ));
        assert_eq!(light.snapshot().power, Power::Unknown);
        // A later valid own online flag with the same failed membership keeps
        // the relation and refreshes the actual fields.
        c.cloud.lock().unwrap().catalog_groups[0]["isOnline"] = json!(true);
        c.context.request_sync();
        until(|| light.snapshot().power == Power::On).await;
        assert_eq!(light.snapshot().availability, Availability::Available);
    }));
}

#[test]
fn withdrawn_native_group_clears_retained_view_and_retires_own_subscriptions() {
    let mut rig = Rig::new(false, &["one"]);
    let instance: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/group3-instance.json"
    ))
    .unwrap();
    let urn = instance["type"].as_str().unwrap().to_string();
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.catalog_groups = vec![group_record("group-record", "group-p", &["one"], true)];
        cloud
            .catalog_instances
            .insert(urn.clone(), instance.clone());
        for (piid, value) in [(1, json!(true)), (2, json!(50)), (3, json!(2700))] {
            cloud.raw_values.insert(("group-p".into(), 2, piid), value);
        }
    }
    let mut record = cached(&rig.data[0].uid, "group-p", false);
    record.model = "mijia.light.group3".into();
    record.instance_type = urn;
    record.device = crate::xiaomi::capability::parse_group(
        DeviceKey::new("xiaomi", "group-p"),
        &record.model,
        &instance,
    )
    .unwrap();
    rig.data.push(record);
    let group = rig.cloud.lock().unwrap().catalog_groups[0].clone();
    block_on(rig.run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        for (piid, value) in [(1, json!(true)), (2, json!(50)), (3, json!(2700))] {
            peer.value(
                "group-p",
                crate::xiaomi::capability::PropertyBinding { siid: 2, piid },
                value,
            );
        }
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| {
                control
                    .context
                    .live
                    .borrow()
                    .devices
                    .get("group-p")
                    .is_some_and(|slot| slot.admitted.is_some())
            })
            .await;
            control.cloud.lock().unwrap().catalog_groups = vec![group.clone()];
            control.context.request_sync();
            until(|| {
                matches!(
                    control.context.control_path("group-p"),
                    Some(super::super::routing::ControlPath::Local(_))
                )
            })
            .await;
            let id = {
                let live = control.context.live.borrow();
                live.devices["group-p"]
                    .admitted
                    .as_ref()
                    .unwrap()
                    .cached
                    .borrow()
                    .device
                    .functions[0]
                    .description
                    .id
                    .clone()
            };
            let light = function(&control.context.registry, "group-p", &id);
            until(|| light.snapshot().power == Power::On).await;
            // Withdraw the confirmed relation in a complete catalog.
            control.cloud.lock().unwrap().catalog_groups = vec![];
            control.context.request_sync();
            until(|| {
                control
                    .context
                    .registry
                    .get(&DeviceKey::new("xiaomi", "group-p"))
                    .is_none()
            })
            .await;
            // The retained view is permanently unavailable and cleared.
            assert!(matches!(
                light.snapshot().availability,
                Availability::Unavailable(_)
            ));
            assert_eq!(light.snapshot().power, Power::Unknown);
            // The own UNSUBSCRIBE is an actual wire packet: wait for it.
            until(|| {
                peer.state
                    .borrow()
                    .unsubscribes
                    .iter()
                    .any(|filter| filter.contains("group-p"))
            })
            .await;
            assert!(light.execute(Command::On).await.is_err());
            // The same identity can reappear and become usable again.
            let before = peer.count("subscribe", "group-p", 2);
            control.cloud.lock().unwrap().catalog_groups = vec![group.clone()];
            control.context.request_sync();
            until(|| {
                control
                    .context
                    .registry
                    .get(&DeviceKey::new("xiaomi", "group-p"))
                    .is_some()
            })
            .await;
            // Reappearance produces a fresh view; the old retained handle
            // stays unavailable and cleared.
            let refreshed = function(&control.context.registry, "group-p", &id);
            until(|| peer.count("subscribe", "group-p", 2) > before).await;
            until(|| {
                matches!(
                    control.context.control_path("group-p"),
                    Some(super::super::routing::ControlPath::Local(_))
                )
            })
            .await;
            peer.notify_value(
                "group-p",
                crate::xiaomi::capability::PropertyBinding { siid: 2, piid: 1 },
                json!(true),
                false,
            );
            until(|| refreshed.snapshot().power == Power::On).await;
            assert_eq!(light.snapshot().power, Power::Unknown);
            assert!(matches!(
                light.snapshot().availability,
                Availability::Unavailable(_)
            ));
        })
        .await;
    }));
}
