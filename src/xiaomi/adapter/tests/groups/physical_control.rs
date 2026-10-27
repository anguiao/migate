use super::*;

#[test]
fn physical_group_write_uses_its_own_did_and_read_owns_state() {
    let rig = Rig::new(false, &["one"]);
    let instance: Value = serde_json::from_str(include_str!(
        "../../../../../tests/fixtures/miot/group3-instance.json"
    ))
    .unwrap();
    let instance_type = instance["type"].as_str().unwrap().to_string();
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.members.push("member-a".into());
        cloud.catalog_groups = vec![group_record("group-record", "group-1", &["member-a"], true)];
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
            let cached = record.native.clone();
            let function = cached
                .device()
                .functions()
                .iter()
                .find(|function| function.lighting().is_some())
                .unwrap();
            (function.description().id.clone(), function.power().unwrap())
        };
        let light = function(&c.context.registry, "group-1", &id);
        until(|| light.snapshot().power() == Some(Power::Off)).await;
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
            .admitted
            .clone()
            .unwrap();
        work.request_dirty_read(&id, power);
        until(|| reads() == before + 1).await;
        light.execute(Command::On).await.unwrap();
        // The accepted target must not become own state before a real read.
        assert_eq!(light.snapshot().power(), Some(Power::Off));
        // The follow-up read sees a different actual value than the in-flight one.
        c.cloud
            .lock()
            .unwrap()
            .read_values
            .insert(key.clone(), false);
        release_read.send(()).unwrap();
        until(|| reads() == before + 2).await;
        // A read started before the write receipt cannot confirm its result.
        assert_eq!(light.snapshot().power(), Some(Power::Off));
        release_read.send(()).unwrap();
        until(|| work.refresh.borrow().snapshot().inflight.is_empty()).await;
        assert_eq!(light.snapshot().power(), Some(Power::Off));
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
            .set(super::super::routing::PathPriority::CloudFirst);
        let attempts = reads();
        light.execute(Command::On).await.unwrap();
        until(|| reads() > attempts && !work.refresh.borrow().snapshot().problems.is_empty()).await;
        assert_eq!(light.snapshot().power(), Some(Power::Off));
        c.context
            .priority
            .set(super::super::routing::PathPriority::LocalFirst);
    }));
}

#[test]
fn physical_group_compound_target_requires_every_property_and_readback_owns_state() {
    let rig = Rig::new(false, &["one"]);
    let instance: Value = serde_json::from_str(include_str!(
        "../../../../../tests/fixtures/miot/group3-instance.json"
    ))
    .unwrap();
    let instance_type = instance["type"].as_str().unwrap().to_string();
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.members.push("member-a".into());
        cloud.catalog_groups = vec![group_record("group-record", "group-1", &["member-a"], true)];
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
            let cached = record.native.clone();
            let light = cached
                .device()
                .functions()
                .iter()
                .find(|function| function.lighting().is_some())
                .unwrap();
            let bindings = light.lighting().unwrap();
            (
                light.description().id.clone(),
                light.power().unwrap(),
                bindings.brightness().as_ref().unwrap().property(),
            )
        };
        let light = function(&c.context.registry, "group-1", &id);
        until(|| light.snapshot().power() == Some(Power::Off)).await;
        let target =
            || crate::device::LightTarget::brightness(Some(true), 70.0).expect("valid test target");
        // Per-property results: power accepted, brightness rejected. The whole
        // action fails and no optimistic state appears.
        c.cloud.lock().unwrap().property_codes.insert(
            ("group-1".into(), brightness.siid, brightness.piid),
            -704_030_013,
        );
        let action = light.begin_light_action(target(), true).unwrap();
        assert!(action.accepted().await.is_err());
        assert_eq!(light.snapshot().power(), Some(Power::Off));
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
        let work = c.context.live.borrow().devices["group-1"]
            .admitted
            .clone()
            .unwrap();
        until(|| {
            work.refresh.borrow().snapshot().pending.is_empty()
                && work.refresh.borrow().snapshot().inflight.is_empty()
        })
        .await;
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
        let action = light.begin_light_action(target(), true).unwrap();
        action.accepted().await.unwrap();
        assert_eq!(light.snapshot().power(), Some(Power::Off));
        release.send(()).unwrap();
        release.send(()).unwrap();
        until(|| light.snapshot().power() == Some(Power::On)).await;
    }));
}

#[test]
fn failed_membership_keeps_relation_while_offline_own_source_clears_state() {
    let rig = Rig::new(false, &["one"]);
    let instance: Value = serde_json::from_str(include_str!(
        "../../../../../tests/fixtures/miot/group3-instance.json"
    ))
    .unwrap();
    let instance_type = instance["type"].as_str().unwrap().to_string();
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.members.push("member-a".into());
        cloud.catalog_groups = vec![group_record("group-record", "group-1", &["member-a"], true)];
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
                slot.admitted.as_ref().unwrap().native.device().functions()[0]
                    .description()
                    .id
                    .clone(),
                slot.admitted.clone().unwrap(),
            )
        };
        let light = function(&c.context.registry, "group-1", &id);
        until(|| light.snapshot().power() == Some(Power::On)).await;
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
                .is_none_or(|slot| {
                    slot.cloud_access
                        .as_ref()
                        .is_some_and(|access| !access.online)
                })
        })
        .await;
        {
            let live = c.context.live.borrow();
            let slot = live
                .devices
                .get("group-1")
                .expect("retained relation keeps the group slot");
            assert!(slot.group);
            assert!(c.context.relations.borrow().snapshot().iter().any(|group| {
                group
                    .membership
                    .members
                    .iter()
                    .any(|member| member == "member-a")
            }));
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
        assert_eq!(light.snapshot().power(), Some(Power::Unknown));
        // A later valid own online flag with the same failed membership keeps
        // the relation and refreshes the actual fields.
        c.cloud.lock().unwrap().catalog_groups[0]["isOnline"] = json!(true);
        c.context.request_sync();
        until(|| light.snapshot().power() == Some(Power::On)).await;
        assert_eq!(light.snapshot().availability, Availability::Available);
    }));
}

#[test]
fn physical_group_retires_subscriptions_on_virtual_transition_and_withdrawal() {
    let mut rig = Rig::new(false, &["one"]);
    let instance: Value = serde_json::from_str(include_str!(
        "../../../../../tests/fixtures/miot/group3-instance.json"
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
    let record = crate::xiaomi::capability::NativeDescription::new(
        "mijia.light.group3".into(),
        urn,
        crate::xiaomi::capability::parse_group(
            DeviceKey::new("xiaomi", "group-p"),
            "mijia.light.group3",
            &instance,
        )
        .unwrap(),
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
                    Some(super::super::super::routing::ControlPath::Local(_))
                )
            })
            .await;
            let id = {
                let live = control.context.live.borrow();
                live.devices["group-p"]
                    .admitted
                    .as_ref()
                    .unwrap()
                    .native
                    .device()
                    .functions()[0]
                    .description()
                    .id
                    .clone()
            };
            let light = function(&control.context.registry, "group-p", &id);
            until(|| light.snapshot().power() == Some(Power::On)).await;
            until(|| control.context.active_notification("group-p").is_some()).await;
            let before = peer.count("subscribe", "group-p", 2);
            let mut virtual_group = group.clone();
            virtual_group["extra"]["member_list"]["is_physical"] = json!(false);
            control.cloud.lock().unwrap().catalog_groups = vec![virtual_group];
            control.context.request_sync();
            until(|| control.context.is_virtual_group("group-p")).await;
            until(|| {
                control.context.live.borrow().devices["group-p"]
                    .admitted
                    .as_ref()
                    .unwrap()
                    .observer()
                    .is_none()
            })
            .await;
            assert_eq!(light.snapshot().power(), Some(Power::Unknown));
            control.cloud.lock().unwrap().catalog_groups = vec![group.clone()];
            control.context.request_sync();
            until(|| !control.context.is_virtual_group("group-p")).await;
            until(|| peer.count("subscribe", "group-p", 2) > before).await;
            until(|| control.context.active_notification("group-p").is_some()).await;
            until(|| light.snapshot().power() == Some(Power::On)).await;
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
            assert_eq!(light.snapshot().power(), Some(Power::Unknown));
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
                    Some(super::super::super::routing::ControlPath::Local(_))
                )
            })
            .await;
            peer.notify_value(
                "group-p",
                crate::xiaomi::capability::PropertyBinding { siid: 2, piid: 1 },
                json!(true),
                false,
            );
            until(|| refreshed.snapshot().power() == Some(Power::On)).await;
            assert_eq!(light.snapshot().power(), Some(Power::Unknown));
            assert!(matches!(
                light.snapshot().availability,
                Availability::Unavailable(_)
            ));
        })
        .await;
    }));
}
