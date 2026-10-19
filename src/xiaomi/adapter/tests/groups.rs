use super::*;
use std::cell::Cell;

use super::support::group_record;

#[test]
fn native_group_description_restarts_after_its_loading_token_is_retired() {
    let rig = Rig::new(true, &["one"]);
    rig.clock
        .automatic
        .store(false, std::sync::atomic::Ordering::Relaxed);
    let instance = group_instance();
    let urn = instance["type"].as_str().unwrap().to_string();
    let (release, gate) = flume::bounded(1);
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.catalog_groups = vec![group_record("record", "group-1", &["one"], false)];
        cloud.catalog_instances.insert(urn.clone(), instance);
        cloud.instance_gates.insert(urn.clone(), gate);
    }
    block_on(rig.run(|c| async move {
        until(|| c.cloud.lock().unwrap().instance_reads.get(&urn) == Some(&1)).await;
        let loading = c.context.live.borrow().devices["group-1"]
            .loading
            .clone()
            .unwrap();
        c.cloud.lock().unwrap().instance_gates.remove(&urn);
        let mut credentials = c.context.live.borrow().credentials.clone().unwrap();
        credentials.tokens.access_token = "replacement-access".into();
        c.store.xiaomi().replace(&credentials).unwrap();
        c.context.adopt_credentials(credentials);
        future::or(
            async {
                until(|| {
                    c.context.live.borrow().devices["group-1"]
                        .loading
                        .as_ref()
                        .is_none_or(|current| !Rc::ptr_eq(current, &loading))
                })
                .await;
            },
            async {
                async_io::Timer::after(Duration::from_secs(2)).await;
                panic!("retired group loading still owns its slot");
            },
        )
        .await;
        release.send(()).unwrap();
        c.context.request_sync();
        until(|| {
            c.context
                .registry
                .get(&DeviceKey::new("xiaomi", "group-1"))
                .is_some()
        })
        .await;
        assert!(c.cloud.lock().unwrap().instance_reads[&urn] >= 2);
    }));
}

#[test]
fn native_group_cloud_permission_recovers_only_after_fresh_confirmation() {
    let rig = super::support::repair_rig(1, true);
    block_on(rig.run(|c| async move {
        super::support::repair_ready(&c).await;
        c.clock
            .automatic
            .store(false, std::sync::atomic::Ordering::Relaxed);
        let light = function(&c.context.registry, "group-1", "main");
        c.cloud.lock().unwrap().property_code = -704_012_904;
        assert!(light.execute(Command::Off).await.is_err());
        assert!(matches!(
            light.snapshot().availability,
            Availability::Unavailable(_)
        ));

        // A failed membership read retains the group but cannot restore access.
        {
            let mut cloud = c.cloud.lock().unwrap();
            cloud.property_code = 0;
            cloud.catalog_groups[0]["extra"]["member_list"]["status"] = json!("invalid");
        }
        c.context.request_sync();
        until(|| c.context.relations.borrow().snapshot()[0].error.is_some()).await;
        assert!(matches!(
            light.execute(Command::Off).await,
            Err(crate::device::CommandError::Unavailable(_))
        ));
        assert_eq!(c.cloud.lock().unwrap().numeric_writes.len(), 1);

        c.cloud.lock().unwrap().catalog_groups[0]["extra"]["member_list"]["status"] = json!("1");
        c.context.request_sync();
        until(|| c.context.relations.borrow().snapshot()[0].error.is_none()).await;
        assert_eq!(light.snapshot().availability, Availability::Available);
        light.execute(Command::Off).await.unwrap();
        assert_eq!(c.cloud.lock().unwrap().numeric_writes.len(), 2);
    }));
}

#[test]
fn native_group_publication_preserves_complete_configuration_on_storage_failure() {
    let rig = Rig::new(false, &["one"]);
    let instance: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/group3-instance.json"
    ))
    .unwrap();
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.members.push("member-new".into());
        cloud
            .catalog_instances
            .insert(instance["type"].as_str().unwrap().into(), instance);
        cloud.catalog_groups = vec![group_record("record-1", "group-1", &["one"], false)];
    }
    block_on(rig.run(|c| async move {
        c.announce();
        until(|| c.context.registry.get(&DeviceKey::new("xiaomi", "group-1")).is_some()).await;
        let key = DeviceKey::new("xiaomi", "group-1");
        let committed = c.store.devices().load_published(&key).unwrap()
            .expect("native admission must use the complete configuration commit");
        let scope = c.context.live.borrow().scope.clone().unwrap();
        let old = c.store.devices().load_publication(Some(&scope)).unwrap();
        assert!(old.cached.iter().any(|cached| cached.device.key == key));
        assert_eq!(old.relations[0].members, ["one"]);
        assert!(c.store.devices().load_published(&DeviceKey::new("xiaomi", "one")).unwrap().is_none(), "a member of a published group must be hidden");

        let db = rusqlite::Connection::open(c.store.path()).unwrap();
        db.execute_batch("CREATE TRIGGER fail_configuration BEFORE INSERT ON native_group_member WHEN NEW.member_did = 'member-new' BEGIN SELECT RAISE(ABORT, 'injected'); END;").unwrap();
        // A newly confirmed member changes the candidate relation; the serving
        // source, identity and repair inputs must remain on the old full view.
        {
            let mut cloud = c.cloud.lock().unwrap();
            cloud.catalog_groups = vec![group_record("record-1", "group-1", &["one", "member-new"], false)];
        }
        c.context.request_sync();
        until(|| c.context.relations.borrow().snapshot().iter().any(|group| group.membership.members.iter().any(|member| member == "member-new"))).await;
        until(|| c.publication.failure().is_some()).await;
        let status = crate::terminal::AuthStatus::new(
            c.context.status.borrow().authentication.clone(),
            "/bin/migate".into(),
            "/data".into(),
        );
        let terminal = crate::terminal::bridge::Context::configured(
            c.publication.clone(),
            &c.store.devices(),
            &status,
        );
        let report = terminal.handle_line("status", 0).await.unwrap().unwrap();
        assert!(report.contains("pending application"));
        assert!(report.contains("Configuration apply failed:"));
        assert_eq!(c.context.live.borrow().devices["group-1"].effective_group.as_ref().unwrap().members, ["one"]);
        let saved = c.store.devices().load_publication(Some(&scope)).unwrap();
        assert_eq!(saved.root_version, old.root_version);
        assert_eq!(saved.relations[0].members, old.relations[0].members);
        assert_eq!(c.store.devices().load_published(&key).unwrap().unwrap().unique_id, committed.unique_id);
    }));
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
                .snapshot()
                .iter()
                .any(|group| {
                    group.active
                        && group
                            .membership
                            .members
                            .iter()
                            .any(|member| member == "one")
                })
        })
        .await;
        assert!(control.context.live.borrow().scope.is_some());
        assert_eq!(control.cloud.lock().unwrap().catalogs, 1);
    }));
}

#[test]
fn missing_description_member_group_is_controllable_with_unknown_state() {
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
            // Native group commands use cloud access; its state needs known members.
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
        let light = first.physical.functions()[0].clone();
        assert_eq!(light.snapshot().power, Power::Unknown);
        let state = light.snapshot().lighting.unwrap();
        assert_eq!(state.brightness_percent, None);
        assert_eq!(state.temperature_kelvin, None);
        assert_eq!(state.color, None);
        assert_eq!(state.mode, None);
        light.execute(Command::On).await.unwrap();
        assert_eq!(light.snapshot().power, Power::Unknown);
        assert!(
            c.cloud
                .lock()
                .unwrap()
                .property_reads
                .iter()
                .all(|(did, _, _)| did != "group-1")
        );
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
fn physical_group_write_uses_its_own_did_and_read_owns_state() {
    let rig = Rig::new(false, &["one"]);
    let instance: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/group3-instance.json"
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
        until(|| reads() == before + 2).await;
        // A read started before the write receipt cannot confirm its result.
        assert_eq!(light.snapshot().power, Power::Off);
        release_read.send(()).unwrap();
        until(|| work.refresh.borrow().snapshot().inflight.is_empty()).await;
        assert_eq!(light.snapshot().power, Power::Off);
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
        until(|| reads() > attempts && !work.refresh.borrow().snapshot().problems.is_empty()).await;
        assert_eq!(light.snapshot().power, Power::Off);
        c.context
            .priority
            .set(super::routing::PathPriority::LocalFirst);
    }));
}

#[test]
fn physical_group_compound_target_requires_every_property_and_readback_owns_state() {
    let rig = Rig::new(false, &["one"]);
    let instance: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/group3-instance.json"
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
        let work = c.context.live.borrow().devices["group-1"]
            .work
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
fn physical_group_retires_subscriptions_on_virtual_transition_and_withdrawal() {
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
            until(|| control.context.active_notification("group-p").is_some()).await;
            let before = peer.count("subscribe", "group-p", 2);
            let mut virtual_group = group.clone();
            virtual_group["extra"]["member_list"]["is_physical"] = json!(false);
            control.cloud.lock().unwrap().catalog_groups = vec![virtual_group];
            control.context.request_sync();
            until(|| control.context.is_virtual_group("group-p")).await;
            until(|| {
                control.context.live.borrow().devices["group-p"]
                    .work
                    .as_ref()
                    .unwrap()
                    .active
                    .borrow()
                    .is_none()
            })
            .await;
            assert_eq!(light.snapshot().power, Power::Unknown);
            control.cloud.lock().unwrap().catalog_groups = vec![group.clone()];
            control.context.request_sync();
            until(|| !control.context.is_virtual_group("group-p")).await;
            until(|| peer.count("subscribe", "group-p", 2) > before).await;
            until(|| control.context.active_notification("group-p").is_some()).await;
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

fn free_udp_port() -> u16 {
    std::net::UdpSocket::bind(std::net::SocketAddrV6::new(
        std::net::Ipv6Addr::UNSPECIFIED,
        0,
        0,
        0,
    ))
    .unwrap()
    .local_addr()
    .unwrap()
    .port()
}

fn runtime_socket_live(port: u16) -> bool {
    std::net::UdpSocket::bind(std::net::SocketAddrV6::new(
        std::net::Ipv6Addr::UNSPECIFIED,
        port,
        0,
        0,
    ))
    .is_err()
}

#[test]
fn configured_runtime_rebuilds_only_for_configuration_and_keeps_committed_state() {
    let rig = Rig::new(false, &["one", "two"]);
    let instance: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/group3-instance.json"
    ))
    .unwrap();
    let urn = instance["type"].as_str().unwrap().to_string();
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.catalog_instances.insert(urn, instance);
        cloud.catalog_groups = vec![group_record("record-1", "group-1", &["one"], false)];
        cloud.online.insert("two".into());
        for (piid, value) in [(1, json!(false)), (2, json!(50)), (3, json!(3000))] {
            cloud.raw_values.insert(("group-1".into(), 2, piid), value);
        }
    }
    block_on(rig.run_result_without_publication(|c| async move {
        c.announce();
        until(|| {
            c.context.relations.borrow().snapshot().iter().any(|group| {
                group.active
                    && group
                        .membership
                        .members
                        .iter()
                        .any(|member| member == "one")
            })
        })
        .await;
        let port = free_udp_port();
        let store = c.store.clone();
        let identity = store.load_identity().unwrap();
        let report = crate::xiaomi::auth::AuthReport::for_test(
            crate::xiaomi::auth::AuthenticationState::NotSignedIn,
            None,
            crate::xiaomi::auth::CertificateUpdate::NotNeeded,
            0,
        );
        let status = crate::terminal::AuthStatus::new(report, "/bin/migate".into(), "/data".into());
        let handle = crate::xiaomi::adapter::SourceHandle(c.context.clone());
        let registry = handle.registry();
        let options = crate::runtime::BridgeOptions {
            store: &store,
            identity: &identity,
            status: &status,
            port,
        };
        let runtime = crate::runtime::run_xiaomi_bridge(
            options,
            async move { Ok((handle, registry)) },
            future::pending::<Result<(), crate::RuntimeError>>(),
            &b""[..],
            Vec::new(),
            future::pending::<Result<(), crate::RuntimeError>>(),
            |_| Ok(()),
        );
        let assertions = async {
            let group = DeviceKey::new("xiaomi", "group-1");
            let two = DeviceKey::new("xiaomi", "two");
            until(|| store.devices().load_published(&group).unwrap().is_some()).await;
            until(|| store.devices().load_published(&two).unwrap().is_some()).await;
            until(|| runtime_socket_live(port)).await;
            let version = || store.devices().load_publication(None).unwrap().root_version;
            let root_before = version();
            let group_before = store.devices().load_published(&group).unwrap().unwrap();
            let two_before = c.context.registry.get(&two).unwrap();

            // A small cfg(test) observer records whether the real UDP socket was
            // free at each snapshot and can inject a late confirmed candidate.
            let observations = Rc::new(RefCell::new(Vec::new()));
            let injected = Rc::new(Cell::new(false));
            let inject_enabled = Rc::new(Cell::new(false));
            {
                let weak = Rc::downgrade(&c.context);
                let observations = observations.clone();
                let injected = injected.clone();
                let inject_enabled = inject_enabled.clone();
                *c.context.snapshot_observer.borrow_mut() = Some(Box::new(move || {
                    let Some(context) = weak.upgrade() else {
                        return;
                    };
                    let free = !runtime_socket_live(port);
                    observations.borrow_mut().push(free);
                    if inject_enabled.get() && free && !injected.get() {
                        injected.set(true);
                        let mut live = context.live.borrow_mut();
                        if let Some(slot) = live.devices.get_mut("group-1") {
                            let mut cached = slot
                                .candidate
                                .clone()
                                .map(|(cached, _)| cached)
                                .or_else(|| {
                                    slot.admitted
                                        .as_ref()
                                        .map(|record| record.cached.borrow().clone())
                                })
                                .unwrap();
                            for capability in
                                &mut cached.device.functions[0].description.capabilities
                            {
                                if let crate::device::Capability::ColorTemperature(range) =
                                    capability
                                {
                                    *range = crate::device::NumericRange {
                                        min: 2700,
                                        max: 6500,
                                        step: 1,
                                    };
                                }
                            }
                            if let Some(temperature) = cached.device.functions[0]
                                .lighting
                                .as_mut()
                                .and_then(|lighting| lighting.temperature.as_mut())
                            {
                                temperature.range = crate::device::NumericRange {
                                    min: 2700,
                                    max: 6500,
                                    step: 1,
                                };
                            }
                            slot.candidate = Some((cached, None));
                            slot.confirmed = true;
                        }
                    }
                }));
            }

            // Phase 1: a pure relation change rebuilds but must not bump any
            // configuration version.
            // Seed an old subscription slot after startup. The next instance
            // must discard it before attempting to decode or resume it.
            let subscription_key = rs_matter::persist::PERSISTENT_SUBSCRIPTIONS_START;
            store.matter().put(subscription_key, &[0xff]).unwrap();
            c.cloud.lock().unwrap().members.push("member-a".into());
            c.cloud.lock().unwrap().catalog_groups = vec![group_record(
                "record-1",
                "group-1",
                &["one", "member-a"],
                false,
            )];
            c.context.request_sync();
            until(|| {
                store
                    .devices()
                    .load_publication(c.context.live.borrow().scope.as_ref())
                    .unwrap()
                    .relations
                    .first()
                    .is_some_and(|relation| relation.members.iter().any(|m| m == "member-a"))
            })
            .await;
            until(|| runtime_socket_live(port)).await;
            assert!(!injected.get());
            assert!(observations.borrow().iter().any(|free| *free));
            assert_eq!(version(), root_before);
            let group_after_relation = store.devices().load_published(&group).unwrap().unwrap();
            assert!(!store.matter().contains(subscription_key).unwrap());
            assert_eq!(
                group_after_relation.configuration_version,
                group_before.configuration_version
            );
            // The unchanged outlet keeps serving through the rebuild.
            let two_after_relation = c.context.registry.get(&two).unwrap();
            assert!(two_before.same_instance(&two_after_relation));
            assert_eq!(two_before.label(), two_after_relation.label());

            // A real active group light action is cancelled when the runner is
            // dropped for the next rebuild.
            let (id, _) = {
                let live = c.context.live.borrow();
                let record = live.devices["group-1"].admitted.as_ref().unwrap();
                let cached = record.cached.borrow();
                let light = cached
                    .device
                    .functions
                    .iter()
                    .find(|function| function.lighting.is_some())
                    .unwrap();
                (light.description.id.clone(), light.power)
            };
            let light = function(&c.context.registry, "group-1", &id);
            let action = light
                .begin_light_action(
                    crate::device::LightDimension::Brightness,
                    crate::device::LightTarget {
                        power: Some(true),
                        brightness_percent: Some(70.0),
                        ..Default::default()
                    },
                    false,
                )
                .unwrap();
            action.accepted().await.unwrap();
            let token = action.token();
            assert!(light.light_action_current(token));

            // Phase 2: the relation returns to its confirmed members while the
            // late native range candidate changes the published fixed shape.
            inject_enabled.set(true);
            c.cloud.lock().unwrap().catalog_groups =
                vec![group_record("record-1", "group-1", &["one"], false)];
            c.context.request_sync();
            until(|| version() > root_before).await;
            assert!(
                !light.light_action_current(token),
                "the rebuild cancelled the active light action"
            );
            until(|| runtime_socket_live(port)).await;
            assert!(injected.get());
            assert_eq!(version(), root_before + 1);
            let group_after_bounds = store.devices().load_published(&group).unwrap().unwrap();
            assert_eq!(
                group_after_bounds.configuration_version,
                group_before.configuration_version + 1
            );
            assert_eq!(group_after_bounds.unique_id, group_before.unique_id);
            assert_eq!(group_after_bounds.endpoint, group_before.endpoint);

            // The unchanged outlet still accepts real control after the
            // rebuild: an On/Off round trip reaches the source as property
            // writes for did "two".
            let two_id = {
                let live = c.context.live.borrow();
                let record = live.devices["two"].admitted.as_ref().unwrap();
                record.cached.borrow().device.functions[0]
                    .description
                    .id
                    .clone()
            };
            let outlet = function(&c.context.registry, "two", &two_id);
            c.cloud.lock().unwrap().property_writes.clear();
            outlet.execute(crate::device::Command::On).await.unwrap();
            until(|| {
                c.cloud
                    .lock()
                    .unwrap()
                    .property_writes
                    .iter()
                    .any(|(did, _, _, value, _)| did == "two" && *value)
            })
            .await;
            outlet.execute(crate::device::Command::Off).await.unwrap();
            until(|| {
                c.cloud
                    .lock()
                    .unwrap()
                    .property_writes
                    .iter()
                    .any(|(did, _, _, value, _)| did == "two" && !*value)
            })
            .await;

            // Ordinary state changes do not rebuild or touch the socket.
            let snapshots = observations.borrow().len();
            let root_settled = version();
            light
                .observe_light(crate::device::LightPatch {
                    power: Some(true),
                    ..Default::default()
                })
                .unwrap();
            c.context.changed.notify(usize::MAX);
            async_io::Timer::after(Duration::from_millis(20)).await;
            assert_eq!(observations.borrow().len(), snapshots);
            assert_eq!(version(), root_settled);
            assert!(runtime_socket_live(port));

            // A failed save keeps the whole committed configuration and only
            // records the retryable failure.
            let db = rusqlite::Connection::open(store.path()).unwrap();
            db.execute_batch(
                "CREATE TRIGGER fail_identity BEFORE UPDATE ON device_identity
                 BEGIN SELECT RAISE(ABORT, 'injected'); END;",
            )
            .unwrap();
            let before_failure = observations.borrow().len();
            c.cloud.lock().unwrap().catalog_groups = vec![group_record(
                "record-1",
                "group-1",
                &["one", "member-a"],
                false,
            )];
            c.context.request_sync();
            until(|| {
                observations.borrow()[before_failure..]
                    .iter()
                    .any(|free| *free)
            })
            .await;
            until(|| runtime_socket_live(port)).await;
            assert_eq!(version(), root_settled);
            assert_eq!(
                c.context.live.borrow().devices["group-1"]
                    .effective_group
                    .as_ref()
                    .unwrap()
                    .members,
                ["one"]
            );
            let group_after_failure = store.devices().load_published(&group).unwrap().unwrap();
            assert_eq!(
                group_after_failure.configuration_version,
                group_after_bounds.configuration_version
            );
            assert_eq!(group_after_failure.unique_id, group_before.unique_id);
            assert_eq!(group_after_failure.endpoint, group_before.endpoint);

            Ok::<(), crate::RuntimeError>(())
        };
        future::or(runtime, assertions).await.unwrap();
    }))
    .unwrap();
}

#[test]
fn failed_source_wins_before_configured_startup_commits() {
    let rig = Rig::new(false, &["one"]);
    block_on(rig.run_result_without_publication(|c| async move {
        c.announce();
        until(|| {
            let live = c.context.live.borrow();
            live.scope.is_some()
                && live
                    .devices
                    .get("one")
                    .is_some_and(|slot| slot.candidate.is_some())
        })
        .await;
        let port = free_udp_port();
        let store = c.store.clone();
        let identity = store.load_identity().unwrap();
        let report = crate::xiaomi::auth::AuthReport::for_test(
            crate::xiaomi::auth::AuthenticationState::NotSignedIn,
            None,
            crate::xiaomi::auth::CertificateUpdate::NotNeeded,
            0,
        );
        let status = crate::terminal::AuthStatus::new(report, "/bin/migate".into(), "/data".into());
        let handle = crate::xiaomi::adapter::SourceHandle(c.context.clone());
        let registry = handle.registry();
        let before = store.devices().load_publication(None).unwrap();
        let paired = Rc::new(Cell::new(false));
        let paired_flag = paired.clone();
        let error = crate::runtime::run_xiaomi_bridge(
            crate::runtime::BridgeOptions {
                store: &store,
                identity: &identity,
                status: &status,
                port,
            },
            async move { Ok((handle, registry)) },
            async { Err("source failed before startup".into()) },
            &b""[..],
            Vec::new(),
            future::pending::<Result<(), crate::RuntimeError>>(),
            move |_| {
                paired_flag.set(true);
                Ok(())
            },
        )
        .await
        .unwrap_err();
        assert_eq!(error.to_string(), "source failed before startup");
        assert!(!paired.get(), "a failed source must not start pairing");
        let after = store.devices().load_publication(None).unwrap();
        assert_eq!(after.root_version, before.root_version);
        assert_eq!(after.records, before.records);
        assert_eq!(after.revocations, before.revocations);
        assert!(
            store
                .devices()
                .load_published(&DeviceKey::new("xiaomi", "one"))
                .unwrap()
                .is_none(),
            "a failed source must not publish any device"
        );
    }))
    .unwrap();
}

#[test]
fn revoked_native_group_stays_reserved_and_startup_recovery_skips_it() {
    let rig = Rig::new(false, &["one"]);
    let instance: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/group3-instance.json"
    ))
    .unwrap();
    let urn = instance["type"].as_str().unwrap().to_string();
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.catalog_instances.insert(urn, instance);
        cloud.catalog_groups = vec![group_record("record-1", "group-1", &["one"], false)];
    }
    block_on(rig.run(|c| async move {
        c.announce();
        let store = c.store.clone();
        let group = DeviceKey::new("xiaomi", "group-1");
        let one = DeviceKey::new("xiaomi", "one");
        until(|| {
            c.context.relations.borrow().snapshot().iter().any(|group| {
                group.active
                    && group
                        .membership
                        .members
                        .iter()
                        .any(|member| member == "one")
            })
        })
        .await;
        until(|| c.store.devices().load_published(&group).unwrap().is_some()).await;
        until(|| c.publication.current().published.get(&group).is_some()).await;
        let group_before = store.devices().load_published(&group).unwrap().unwrap();
        let group_physical = c.publication.current().published.get(&group).unwrap();

        // Recovery requires the hidden member's own description to be saved;
        // its admission may lag the group's native publication.
        until(|| {
            store
                .devices()
                .load_publication(None)
                .unwrap()
                .records
                .iter()
                .any(|record| record.key == one)
        })
        .await;

        // The real complete-catalog deletion closes the retained authority and
        // writes its tombstone while the identity update keeps failing.
        let db = rusqlite::Connection::open(store.path()).unwrap();
        db.execute_batch(
            "CREATE TRIGGER fail_identity BEFORE UPDATE ON device_identity
             BEGIN SELECT RAISE(ABORT, 'injected'); END;",
        )
        .unwrap();
        c.cloud.lock().unwrap().catalog_groups = vec![];
        c.context.request_sync();
        until(|| {
            store
                .devices()
                .load_publication(None)
                .unwrap()
                .revocations
                .iter()
                .any(|revocation| revocation.key == group)
        })
        .await;
        assert!(matches!(
            group_physical.availability(),
            crate::device::Availability::Unavailable(_)
        ));
        assert!(
            group_physical.functions()[0]
                .execute(crate::device::Command::On)
                .await
                .is_err()
        );
        let saved = store.devices().load_publication(None).unwrap();
        assert!(saved.revocations.iter().any(|revocation| {
            revocation.key == group && revocation.reason == "native group withdrawn"
        }));
        assert!(
            saved
                .records
                .iter()
                .any(|record| record.key == group && record.published)
        );

        // A real startup while the save still fails must error before it ever
        // binds or serves the revoked endpoint.
        let identity = store.load_identity().unwrap();
        let report = crate::xiaomi::auth::AuthReport::for_test(
            crate::xiaomi::auth::AuthenticationState::NotSignedIn,
            None,
            crate::xiaomi::auth::CertificateUpdate::NotNeeded,
            0,
        );
        let status = crate::terminal::AuthStatus::new(report, "/bin/migate".into(), "/data".into());
        let startup_port = free_udp_port();
        let failing = XiaomiAdapter::build(
            store.clone(),
            c.context.cloud.clone(),
            c.discovery.clone(),
            c.context.clock.clone(),
        )
        .unwrap()
        .0;
        let failing_handle = crate::xiaomi::adapter::SourceHandle(failing.context.clone());
        let failing_registry = failing_handle.registry();
        let error = crate::runtime::run_xiaomi_bridge(
            crate::runtime::BridgeOptions {
                store: &store,
                identity: &identity,
                status: &status,
                port: startup_port,
            },
            async move { Ok((failing_handle, failing_registry)) },
            future::pending::<Result<(), crate::RuntimeError>>(),
            &b""[..],
            Vec::new(),
            future::pending::<Result<(), crate::RuntimeError>>(),
            |_| Ok(()),
        )
        .await
        .unwrap_err();
        assert!(
            error.to_string().contains("commit published configuration"),
            "{error}"
        );
        assert!(!runtime_socket_live(startup_port));

        // Once the save can succeed, a fresh restart applies the bootstrap
        // revocation overlay without reviving the group; the hidden member
        // returns with unknown state and the identity stays reserved. The
        // whole recovery stays synchronous so the running applier cannot
        // interleave.
        let db = rusqlite::Connection::open(store.path()).unwrap();
        db.execute_batch("DROP TRIGGER fail_identity;").unwrap();
        let restored = XiaomiAdapter::build(
            store.clone(),
            c.context.cloud.clone(),
            c.discovery.clone(),
            c.context.clock.clone(),
        )
        .unwrap()
        .0;
        {
            let live = restored.context.live.borrow();
            assert!(
                live.devices
                    .get("group-1")
                    .is_none_or(|slot| slot.admitted.is_none()),
                "bootstrap overlay must not admit a revoked group"
            );
            assert!(
                live.devices
                    .get("one")
                    .is_some_and(|slot| slot.admitted.is_some()),
                "hidden member is restored"
            );
        }
        let member = restored.context.registry.get(&one).unwrap();
        assert_eq!(member.functions()[0].snapshot().power, Power::Unknown);
        let reopened = crate::publication::Publication::open(
            crate::xiaomi::adapter::SourceHandle(restored.context.clone()),
            store.clone(),
        )
        .unwrap();
        if let Some(prepared) = reopened.prepare().unwrap() {
            reopened.apply(prepared).unwrap();
        }
        assert!(reopened.current().published.get(&group).is_none());
        let recovered = store.devices().load_publication(None).unwrap();
        let reserved = recovered
            .records
            .iter()
            .find(|record| record.key == group)
            .unwrap();
        assert!(!reserved.published);
        assert_eq!(reserved.unique_id, group_before.unique_id);
        assert_eq!(reserved.endpoint, group_before.endpoint);
        assert_eq!(
            reserved.configuration_version,
            group_before.configuration_version
        );
    }));
}

#[test]
fn restart_restores_group_and_hidden_member_identity_and_effective_status() {
    let rig = Rig::new(false, &["one"]);
    let instance: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/group3-instance.json"
    ))
    .unwrap();
    let urn = instance["type"].as_str().unwrap().to_string();
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.catalog_instances.insert(urn, instance);
        cloud.catalog_groups = vec![group_record("record-1", "group-1", &["one"], false)];
    }
    block_on(rig.run(|c| async move {
        c.announce();
        let store = c.store.clone();
        let group = DeviceKey::new("xiaomi", "group-1");
        let one = DeviceKey::new("xiaomi", "one");
        until(|| c.store.devices().load_published(&group).unwrap().is_some()).await;
        until(|| c.publication.current().published.get(&group).is_some()).await;
        let group_before = store.devices().load_published(&group).unwrap().unwrap();
        // The hidden member keeps a reserved identity as soon as its own
        // description is admitted (it may lag the group confirmation).
        until(|| {
            store
                .devices()
                .load_publication(None)
                .unwrap()
                .records
                .iter()
                .any(|record| record.key == one)
        })
        .await;
        let member_before = store
            .devices()
            .load_publication(None)
            .unwrap()
            .records
            .into_iter()
            .find(|record| record.key == one)
            .expect("hidden member keeps its reserved identity");
        assert!(!member_before.published);
        let root_before = store.devices().load_publication(None).unwrap().root_version;
        assert!(c.publication.current().published.get(&one).is_none());
        assert!(
            c.publication
                .current()
                .source
                .relations
                .iter()
                .any(|relation| {
                    relation.group_did == "group-1"
                        && relation.members.iter().any(|member| member == "one")
                })
        );

        // Terminal renders the effective hidden member and its membership.
        let status = crate::terminal::AuthStatus::new(
            crate::xiaomi::auth::AuthReport::for_test(
                crate::xiaomi::auth::AuthenticationState::NotSignedIn,
                None,
                crate::xiaomi::auth::CertificateUpdate::NotNeeded,
                0,
            ),
            "/bin/migate".into(),
            "/data".into(),
        );
        let terminal = crate::terminal::bridge::Context::configured(
            c.publication.clone(),
            &store.devices(),
            &status,
        );
        let list = terminal.handle_line("list", 0).await.unwrap().unwrap();
        let help = terminal.handle_line("help", 0).await.unwrap().unwrap();
        assert!(help.contains(&format!(
            "Configuration: root version {}.",
            c.publication.current().root_version
        )));
        // The effective registry keeps every real source ref; a hidden member
        // is a normal row marked hidden with its confirmed memberships.
        assert!(list.contains("one (name one):"), "{list}");
        assert!(list.contains("Matter: hidden or reserved;"), "{list}");
        assert!(
            list.contains("Group group-1 (virtual): published."),
            "{list}"
        );
        let detail = terminal
            .handle_line("status one", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(
            detail.contains("Group group-1 (virtual): published."),
            "{detail}"
        );

        // Withdraw the confirmed relation (record present, empty membership);
        // the group is reserved and the member becomes an ordinary device.
        c.cloud.lock().unwrap().catalog_groups =
            vec![group_record("record-1", "group-1", &[], false)];
        c.context.request_sync();
        until(|| store.devices().load_published(&group).unwrap().is_none()).await;
        let root_after = store.devices().load_publication(None).unwrap().root_version;
        assert!(root_after > root_before);
        let saved = store.devices().load_publication(None).unwrap();
        let reserved = saved
            .records
            .iter()
            .find(|record| record.key == group)
            .unwrap();
        assert!(!reserved.published);
        assert_eq!(reserved.unique_id, group_before.unique_id);
        assert_eq!(reserved.endpoint, group_before.endpoint);
        assert_eq!(
            reserved.configuration_version,
            group_before.configuration_version
        );
        until(|| c.publication.current().published.get(&one).is_some()).await;
        let list = terminal.handle_line("list", 0).await.unwrap().unwrap();
        assert!(
            list.contains("group-1 (native group): not published (confirmed empty relation)."),
            "{list}"
        );

        // A fresh restart restores the withdrawn relation, the published member
        // with unknown state and the reserved group identity.
        let restored = XiaomiAdapter::build(
            store.clone(),
            c.context.cloud.clone(),
            c.discovery.clone(),
            c.context.clock.clone(),
        )
        .unwrap()
        .0;
        {
            let live = restored.context.live.borrow();
            assert!(
                live.devices
                    .get("group-1")
                    .is_none_or(|slot| slot.admitted.is_none()),
                "a withdrawn group is not admitted"
            );
            assert!(
                live.devices
                    .get("one")
                    .is_some_and(|slot| slot.admitted.is_some()),
                "the former hidden member is restored"
            );
        }
        let member = restored.context.registry.get(&one).unwrap();
        assert_eq!(member.functions()[0].snapshot().power, Power::Unknown);
        let opened = crate::publication::Publication::open(
            crate::xiaomi::adapter::SourceHandle(restored.context.clone()),
            store.clone(),
        )
        .unwrap();
        // Install the bootstrap relation overlay through the normal boundary.
        if let Some(prepared) = opened.prepare().unwrap() {
            opened.apply(prepared).unwrap();
        }
        assert_eq!(opened.current().root_version, root_after);
        assert!(opened.current().published.get(&group).is_none());
        assert!(opened.current().published.get(&one).is_some());
        let recovered = store.devices().load_publication(None).unwrap();
        let reserved = recovered
            .records
            .iter()
            .find(|record| record.key == group)
            .unwrap();
        assert!(!reserved.published);
        assert_eq!(reserved.unique_id, group_before.unique_id);
        assert_eq!(reserved.endpoint, group_before.endpoint);
        assert_eq!(
            reserved.configuration_version,
            group_before.configuration_version
        );
        let member_record = recovered
            .records
            .iter()
            .find(|record| record.key == one)
            .unwrap();
        assert_eq!(member_record.unique_id, member_before.unique_id);
        assert_eq!(member_record.endpoint, member_before.endpoint);
        let status = crate::terminal::AuthStatus::new(
            crate::xiaomi::auth::AuthReport::for_test(
                crate::xiaomi::auth::AuthenticationState::NotSignedIn,
                None,
                crate::xiaomi::auth::CertificateUpdate::NotNeeded,
                0,
            ),
            "/bin/migate".into(),
            "/data".into(),
        );
        let terminal = crate::terminal::bridge::Context::configured(
            Rc::new(opened),
            &store.devices(),
            &status,
        );
        let list = terminal.handle_line("list", 0).await.unwrap().unwrap();
        assert!(
            list.contains("group-1 (native group): not published (native group withdrawn)."),
            "{list}"
        );
    }));
}

#[test]
fn virtual_group_native_writes_refresh_actual_member_values_and_mode() {
    let rig = super::support::repair_rig(1, true);
    block_on(rig.run(|c| async move {
        super::support::repair_ready(&c).await;
        let light = function(&c.context.registry, "group-1", "main");
        // Native control acceptance does not manufacture actual state. Reads
        // must use the members, including mode when a CT command selects white.
        for (dimension, target, values) in [
            (
                None,
                crate::device::LightTarget {
                    power: Some(false),
                    ..Default::default()
                },
                vec![(2, 1, json!(false))],
            ),
            (
                Some(crate::device::LightDimension::Brightness),
                crate::device::LightTarget {
                    brightness_percent: Some(70.0),
                    ..Default::default()
                },
                vec![(2, 2, json!(60))],
            ),
            (
                Some(crate::device::LightDimension::Color),
                crate::device::LightTarget {
                    color: Some(crate::device::LightColorTarget::Temperature(4000)),
                    ..Default::default()
                },
                vec![(2, 3, json!(3800)), (4, 2, json!(0))],
            ),
        ] {
            until(|| {
                c.context
                    .live
                    .borrow()
                    .devices
                    .values()
                    .filter_map(|slot| slot.work.as_ref())
                    .all(|work| {
                        work.refresh.borrow().snapshot().pending.is_empty()
                            && work.refresh.borrow().snapshot().inflight.is_empty()
                    })
            })
            .await;
            {
                let mut cloud = c.cloud.lock().unwrap();
                cloud.property_reads.clear();
                for did in ["member-a", "member-b"] {
                    for (siid, piid, value) in &values {
                        cloud
                            .raw_values
                            .insert((did.into(), *siid, *piid), value.clone());
                    }
                }
            }
            if let Some(dimension) = dimension {
                light
                    .begin_light_action(dimension, target, true)
                    .unwrap()
                    .accepted()
                    .await
                    .unwrap();
            } else {
                light.execute(Command::Off).await.unwrap();
            }
            until(|| {
                let cloud = c.cloud.lock().unwrap();
                ["member-a", "member-b"].iter().all(|did| {
                    values.iter().all(|(siid, piid, _)| {
                        cloud
                            .property_reads
                            .contains(&(did.to_string(), *siid, *piid))
                    })
                })
            })
            .await;
            assert!(
                c.cloud
                    .lock()
                    .unwrap()
                    .property_reads
                    .iter()
                    .all(|(did, _, _)| did != "group-1")
            );
            until(|| match dimension {
                None => light.snapshot().power == Power::Off,
                Some(crate::device::LightDimension::Brightness) => {
                    light.snapshot().lighting.unwrap().brightness_percent == Some(60.0)
                }
                Some(crate::device::LightDimension::Color) => {
                    let state = light.snapshot().lighting.unwrap();
                    state.temperature_kelvin == Some(3800)
                        && state.mode == Some(crate::device::LightMode::Temperature)
                }
            })
            .await;
        }
        assert!(
            c.cloud
                .lock()
                .unwrap()
                .numeric_writes
                .iter()
                .all(|(items, _)| items.iter().all(|item| item["did"] == "group-1"))
        );
    }));
}
