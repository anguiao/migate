use super::*;

fn key(did: &str) -> DeviceKey {
    DeviceKey::new("xiaomi", did)
}

fn group_members(c: &Control, did: &str) -> Option<Vec<DeviceKey>> {
    c.context
        .registry
        .snapshot()
        .applied
        .groups
        .iter()
        .find(|group| group.key == key(did))
        .map(|group| group.members.clone())
}

fn stored_group(store: &Store, uid: &str, did: &str) -> Option<crate::storage::StoredGroup> {
    store
        .devices()
        .restore_groups(uid, "local")
        .unwrap()
        .groups
        .into_iter()
        .find(|group| group.key == key(did))
}

async fn wait_group(c: &Control, did: &str) {
    until(|| group_members(c, did).is_some()).await;
}

#[test]
fn bootstrap_restores_confirmed_group_without_a_group_physical_device() {
    let rig = Rig::restored_group(
        &["one", "two"],
        "group.1",
        "mijia.light.group3",
        "Reading lamp",
    );
    let uid = rig.data[0].uid.clone();
    let registry = block_on(rig.ready.wait()).unwrap();
    let snapshot = registry.snapshot();
    assert_eq!(snapshot.applied.confirmed, Some(1));
    assert_eq!(snapshot.applied.groups.len(), 1);
    let group = &snapshot.applied.groups[0];
    assert_eq!(group.key, key("group.1"));
    assert_eq!(group.label, "Reading lamp");
    assert_eq!(group.members, vec![key("one"), key("two")]);
    assert!(registry.get(&key("group.1")).is_none());
    for member in ["one", "two"] {
        let device = registry.get(&key(member)).expect("member restored");
        assert!(matches!(
            device.availability(),
            Availability::Unavailable(_)
        ));
        for function in device.functions() {
            assert_eq!(function.snapshot().power, Power::Unknown);
        }
    }
    let stored = rig.store.devices().restore_groups(&uid, "local").unwrap();
    assert_eq!(stored.version, 1);
    assert_eq!(stored.groups.len(), 1);
    assert_eq!(stored.groups[0].key, key("group.1"));
    assert_eq!(stored.groups[0].label, "Reading lamp");
    assert_eq!(stored.groups[0].members, vec![key("one"), key("two")]);
}

#[test]
fn physical_and_virtual_groups_publish_after_members_are_admitted() {
    for physical in [true, false] {
        let rig = Rig::group(&["one", "two"], "group.1", "mijia.light.group3");
        {
            let mut cloud = rig.cloud.lock().unwrap();
            cloud.names.insert("group.1".into(), "Reading lamp".into());
            cloud.groups.get_mut("group.1").unwrap().physical = physical;
        }
        block_on(rig.run(|c| async move {
            c.announce();
            wait_group(&c, "group.1").await;
            let snapshot = c.context.registry.snapshot();
            let group = snapshot
                .applied
                .groups
                .iter()
                .find(|group| group.key == key("group.1"))
                .unwrap();
            assert_eq!(group.members, vec![key("one"), key("two")]);
            assert_eq!(group.label, "Reading lamp");
            assert!(snapshot.applied.confirmed.is_some());
            assert!(c.context.registry.get(&key("group.1")).is_none());
            for member in ["one", "two"] {
                assert!(c.context.registry.get(&key(member)).is_some());
            }
            let uid = c.data[0].uid.clone();
            let stored = stored_group(&c.store, &uid, "group.1").expect("published group");
            assert_eq!(stored.members, vec![key("one"), key("two")]);
            let cloud = c.cloud.lock().unwrap();
            assert!(
                cloud
                    .property_reads
                    .iter()
                    .all(|(did, _, _)| did != "group.1")
            );
            assert!(
                cloud
                    .property_writes
                    .iter()
                    .all(|write| write.0 != "group.1")
            );
        }));
    }
}

#[test]
fn restored_group_survives_a_local_reconcile_before_the_first_home_sync() {
    let rig = Rig::restored_group(
        &["one", "two"],
        "group.1",
        "mijia.light.group3",
        "Reading lamp",
    );
    rig.cloud.lock().unwrap().home_status = 500;
    block_on(rig.run(|c| async move {
        c.announce();
        until(|| c.cloud.lock().unwrap().homes > 0).await;
        c.context.reconcile_all().unwrap();
        let snapshot = c.context.registry.snapshot();
        assert_eq!(snapshot.applied.confirmed, Some(1));
        assert!(
            snapshot
                .applied
                .groups
                .iter()
                .any(|group| group.key == key("group.1"))
        );
        assert!(c.context.registry.get(&key("group.1")).is_none());
    }));
}

#[test]
fn incompatible_member_description_keeps_the_published_group_and_capabilities() {
    for change_range in [false, true] {
        let rig = Rig::group(&["one", "two"], "group.1", "mijia.light.group3");
        block_on(rig.run(|c| async move {
            c.announce();
            wait_group(&c, "group.1").await;
            let record = c.context.live.borrow().devices["one"]
                .admitted
                .clone()
                .unwrap();
            let original = record.cached.borrow().clone();
            let before = c.context.registry.snapshot();
            let stored = c
                .store
                .devices()
                .load_published(&key("one"))
                .unwrap()
                .unwrap();
            let mut incompatible = original.clone();
            let function = &mut incompatible.device.functions[0];
            if change_range {
                let range = &mut function
                    .lighting
                    .as_mut()
                    .unwrap()
                    .temperature
                    .as_mut()
                    .unwrap()
                    .range;
                range.max += range.step;
                for capability in &mut function.description.capabilities {
                    if let crate::device::Capability::ColorTemperature(value) = capability {
                        *value = *range;
                    }
                }
            } else {
                function.power = None;
                function
                    .description
                    .capabilities
                    .retain(|capability| !matches!(capability, crate::device::Capability::OnOff));
            }
            c.context
                .live
                .borrow_mut()
                .devices
                .get_mut("one")
                .unwrap()
                .candidate = Some((incompatible, None));
            c.context.reconcile("one").unwrap();
            c.context.reconcile_groups().unwrap();
            let after = c.context.registry.snapshot();
            assert_eq!(after.revision, before.revision);
            assert_eq!(after.applied.confirmed, before.applied.confirmed);
            assert_eq!(after.applied.devices.len(), before.applied.devices.len());
            assert!(
                after
                    .applied
                    .devices
                    .iter()
                    .zip(&before.applied.devices)
                    .all(|(after, before)| after.same_instance(before))
            );
            assert_eq!(
                after
                    .applied
                    .groups
                    .iter()
                    .map(|group| (group.description(), group.members.clone()))
                    .collect::<Vec<_>>(),
                before
                    .applied
                    .groups
                    .iter()
                    .map(|group| (group.description(), group.members.clone()))
                    .collect::<Vec<_>>()
            );
            assert!(
                record
                    .physical
                    .same_instance(&c.context.registry.get(&key("one")).unwrap())
            );
            assert!(matches!(
                record.physical.availability(),
                Availability::Unavailable(_)
            ));
            assert!(!record.life.is_closed());
            assert_eq!(*record.cached.borrow(), original);
            assert_eq!(
                c.store
                    .devices()
                    .load_published(&key("one"))
                    .unwrap()
                    .unwrap(),
                stored
            );
            assert_eq!(
                c.store
                    .devices()
                    .load_parsed(&original.uid, "local", &key("one"))
                    .unwrap()
                    .unwrap(),
                original
            );
            assert!(
                c.context.live.borrow().devices["one"]
                    .blocked
                    .as_ref()
                    .is_some_and(|reason| reason.contains("incompatible"))
            );
        }));
    }
}

#[test]
fn an_async_unsupported_description_keeps_the_confirmed_group_and_old_capabilities() {
    let rig = Rig::group(&["one", "two"], "group.1", "mijia.light.group3");
    block_on(rig.run(|c| async move {
        c.announce();
        wait_group(&c, "group.1").await;
        let before = c
            .store
            .devices()
            .load_published(&key("one"))
            .unwrap()
            .unwrap();
        let reads = c.cloud.lock().unwrap().detail_counts["one"];
        c.cloud.lock().unwrap().unsupported.insert("one".into());
        // Force the cloud member description through a fresh read.
        c.context
            .live
            .borrow_mut()
            .devices
            .get_mut("one")
            .unwrap()
            .blocked = Some("refresh required".into());
        c.context.request_sync();
        until(|| {
            c.cloud.lock().unwrap().detail_counts["one"] > reads
                && c.context.live.borrow().devices["one"].blocked.is_some()
        })
        .await;
        // A parse failure is temporary evidence, not a confirmed capability
        // removal: the last valid shape and the confirmed group stay, and the
        // member keeps its running work instead of being revoked.
        assert!(c.context.registry.get(&key("one")).is_some());
        assert!(group_members(&c, "group.1").is_some());
        assert_eq!(
            c.store
                .devices()
                .load_published(&key("one"))
                .unwrap()
                .unwrap(),
            before
        );
        assert!(c.context.live.borrow().devices["one"].work.is_some());

        // A valid description returns and the same member recovers in place.
        c.cloud.lock().unwrap().unsupported.remove("one");
        c.context
            .live
            .borrow_mut()
            .devices
            .get_mut("one")
            .unwrap()
            .blocked = None;
        c.context.reconcile("one").unwrap();
        until(|| {
            c.context.live.borrow().devices["one"].work.is_some()
                && c.context
                    .status
                    .borrow()
                    .pending
                    .iter()
                    .all(|pending| pending.did != "one")
        })
        .await;
        assert!(group_members(&c, "group.1").is_some());
        assert!(c.context.registry.get(&key("two")).is_some());
    }));
}

#[test]
fn a_failed_group_keeps_its_relation_while_other_groups_confirm_shared_members() {
    let rig = Rig::groups(&[
        GroupSpec {
            did: "group.a",
            model: "mijia.light.group3",
            members: &["one", "two"],
            label: "A",
        },
        GroupSpec {
            did: "group.b",
            model: "mijia.light.group3",
            members: &["three"],
            label: "B",
        },
        GroupSpec {
            did: "group.c",
            model: "mijia.light.group3",
            members: &["four"],
            label: "C",
        },
    ]);
    block_on(rig.run(|c| async move {
        c.announce();
        until(|| c.context.registry.snapshot().applied.groups.len() == 3).await;
        {
            let mut cloud = c.cloud.lock().unwrap();
            cloud.groups.get_mut("group.a").unwrap().missing = true;
            cloud.groups.get_mut("group.b").unwrap().members = vec!["one".into()];
            cloud.groups.get_mut("group.c").unwrap().members = vec!["three".into()];
        }
        c.context.request_sync();
        until(|| {
            group_members(&c, "group.b") == Some(vec![key("one")])
                && group_members(&c, "group.c") == Some(vec![key("three")])
                && c.context
                    .status
                    .borrow()
                    .pending
                    .iter()
                    .any(|pending| pending.did == "group.a")
        })
        .await;
        assert_eq!(
            group_members(&c, "group.a"),
            Some(vec![key("one"), key("two")])
        );
        assert!(
            c.context
                .status
                .borrow()
                .pending
                .iter()
                .all(|pending| { pending.did != "group.b" && pending.did != "group.c" })
        );
        {
            let mut cloud = c.cloud.lock().unwrap();
            cloud.groups.get_mut("group.a").unwrap().missing = false;
            cloud.groups.get_mut("group.a").unwrap().members = vec!["two".into()];
        }
        c.context.request_sync();
        until(|| group_members(&c, "group.a") == Some(vec![key("two")])).await;
        assert_eq!(group_members(&c, "group.b"), Some(vec![key("one")]));
    }));
}

#[test]
fn a_confirmed_member_deletion_revokes_the_group_even_when_group_reads_fail() {
    let rig = Rig::group(&["one", "two"], "group.1", "mijia.light.group3");
    block_on(rig.run(|c| async move {
        c.announce();
        wait_group(&c, "group.1").await;
        {
            let mut cloud = c.cloud.lock().unwrap();
            cloud.members = vec!["group.1".into(), "two".into()];
            cloud.detail_failures.insert("group.1".into(), 500);
        }
        c.context.request_sync();
        until(|| {
            let snapshot = c.context.registry.snapshot();
            snapshot.applied.confirmed.is_some() && snapshot.applied.groups.is_empty()
        })
        .await;
        assert!(c.context.registry.get(&key("one")).is_none());
        assert!(c.context.registry.get(&key("two")).is_some());
        assert!(!c.context.live.borrow().devices.contains_key("one"));
        let uid = c.data[0].uid.clone();
        assert!(
            c.store
                .devices()
                .load_published(&key("one"))
                .unwrap()
                .is_none()
        );
        let stored = c.store.devices().restore_groups(&uid, "local").unwrap();
        assert!(stored.groups.is_empty());
        let snapshot = c.context.registry.snapshot();
        assert_eq!(snapshot.applied.confirmed, Some(stored.version));
    }));
}

#[test]
fn compatible_member_binding_update_keeps_the_instance_group_and_endpoint() {
    let rig = Rig::group(&["one", "two"], "group.1", "mijia.light.group3");
    block_on(rig.run(|c| async move {
        c.announce();
        wait_group(&c, "group.1").await;
        let uid = c.data[0].uid.clone();
        let endpoint = stored_group(&c.store, &uid, "group.1").unwrap().endpoint;
        let record = c.context.live.borrow().devices["one"]
            .admitted
            .clone()
            .unwrap();
        let work = c.context.live.borrow().devices["one"].work.clone().unwrap();
        let mut changed = record.cached.borrow().clone();
        changed.device.functions[0].power.as_mut().unwrap().siid = 7;
        c.context
            .live
            .borrow_mut()
            .devices
            .get_mut("one")
            .unwrap()
            .candidate = Some((changed, None));
        c.context.reconcile("one").unwrap();
        let snapshot = c.context.registry.snapshot();
        let group = snapshot
            .applied
            .groups
            .iter()
            .find(|group| group.key == key("group.1"))
            .expect("group survives a member binding replacement");
        assert_eq!(group.members, vec![key("one"), key("two")]);
        assert!(snapshot.applied.confirmed.is_some());
        let stored = stored_group(&c.store, &uid, "group.1").unwrap();
        assert_eq!(stored.endpoint, endpoint);
        assert_eq!(stored.members, vec![key("one"), key("two")]);
        let live = c.context.live.borrow();
        let current = live.devices["one"].admitted.clone().unwrap();
        assert!(Rc::ptr_eq(&record, &current));
        assert!(!record.life.is_closed());
        assert!(!work.life.is_closed());
        assert!(Rc::ptr_eq(
            &work,
            live.devices["one"].work.as_ref().unwrap()
        ));
        assert_eq!(
            record.cached.borrow().device.functions[0]
                .power
                .unwrap()
                .siid,
            7
        );
        assert!(
            live.devices["one"].work.is_some(),
            "the member resumes work with the new bindings"
        );
        drop(work);
    }));
}

#[test]
fn group_lifecycle_keeps_member_instances_work_and_consumers() {
    let rig = Rig::group(&["one", "two"], "group.1", "mijia.light.group3");
    block_on(rig.run(|c| async move {
        c.announce();
        wait_group(&c, "group.1").await;
        let record_one = c.context.live.borrow().devices["one"]
            .admitted
            .clone()
            .unwrap();
        let record_two = c.context.live.borrow().devices["two"]
            .admitted
            .clone()
            .unwrap();
        let work_one = c.context.live.borrow().devices["one"].work.clone().unwrap();
        let work_two = c.context.live.borrow().devices["two"].work.clone().unwrap();

        // A member exit and a later rejoin publish role changes without
        // recreating the member instance, work controller or consumers.
        c.cloud
            .lock()
            .unwrap()
            .groups
            .get_mut("group.1")
            .unwrap()
            .members = vec!["two".into()];
        c.context.request_sync();
        until(|| group_members(&c, "group.1") == Some(vec![key("two")])).await;
        assert!(c.context.registry.get(&key("one")).is_some());
        c.cloud
            .lock()
            .unwrap()
            .groups
            .get_mut("group.1")
            .unwrap()
            .members = vec!["one".into(), "two".into()];
        c.context.request_sync();
        until(|| group_members(&c, "group.1") == Some(vec![key("one"), key("two")])).await;

        {
            let live = c.context.live.borrow();
            assert!(Rc::ptr_eq(
                live.devices["one"].admitted.as_ref().unwrap(),
                &record_one
            ));
            assert!(Rc::ptr_eq(
                live.devices["two"].admitted.as_ref().unwrap(),
                &record_two
            ));
            assert!(Rc::ptr_eq(
                live.devices["one"].work.as_ref().unwrap(),
                &work_one
            ));
            assert!(Rc::ptr_eq(
                live.devices["two"].work.as_ref().unwrap(),
                &work_two
            ));
        }
        assert!(!record_one.life.is_closed());
        assert!(!record_two.life.is_closed());

        // Dissolving the group returns both members to standalone lights with
        // the same instances.
        c.cloud
            .lock()
            .unwrap()
            .groups
            .get_mut("group.1")
            .unwrap()
            .members = Vec::new();
        c.context.request_sync();
        until(|| group_members(&c, "group.1").is_none()).await;
        assert!(c.context.registry.get(&key("one")).is_some());
        assert!(c.context.registry.get(&key("two")).is_some());
        {
            let live = c.context.live.borrow();
            assert!(Rc::ptr_eq(
                live.devices["one"].admitted.as_ref().unwrap(),
                &record_one
            ));
            assert!(Rc::ptr_eq(
                live.devices["two"].admitted.as_ref().unwrap(),
                &record_two
            ));
        }
    }));
}

#[test]
fn a_transport_failure_does_not_replace_an_incompatible_member() {
    let rig = Rig::group(&["one", "two"], "group.1", "mijia.light.group3");
    block_on(rig.run(|c| async move {
        c.announce();
        wait_group(&c, "group.1").await;
        let mut incompatible = {
            let uid = c.data[0].uid.clone();
            cached(&uid, "one", true)
        };
        incompatible.device.key = key("one");
        c.context
            .live
            .borrow_mut()
            .devices
            .get_mut("one")
            .unwrap()
            .candidate = Some((incompatible, None));
        c.context.reconcile("one").unwrap();
        until(|| {
            let snapshot = c.context.registry.snapshot();
            snapshot.applied.groups.len() == 1
                && c.context.live.borrow().devices["one"].blocked.is_some()
        })
        .await;
        let reads = c.cloud.lock().unwrap().detail_counts["one"];
        c.cloud.lock().unwrap().missing_records.insert("one".into());
        c.context.request_sync();
        until(|| c.cloud.lock().unwrap().detail_counts["one"] > reads).await;
        c.context.reconcile_groups().unwrap();
        let snapshot = c.context.registry.snapshot();
        assert_eq!(snapshot.applied.groups.len(), 1);
        assert!(c.context.live.borrow().devices["one"].blocked.is_some());
    }));
}

#[test]
fn confirmed_deletion_applies_before_a_failed_home_information_batch() {
    let rig = Rig::restored_group(
        &["one", "two"],
        "group.1",
        "mijia.light.group3",
        "Reading lamp",
    );
    let (release, gate) = flume::unbounded();
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.members = vec!["group.1".into(), "two".into()];
        cloud.info_status = 503;
        cloud.home_gate = Some(gate);
    }
    block_on(rig.run(|c| async move {
        c.announce();
        release.send(()).unwrap();
        until(|| {
            let snapshot = c.context.registry.snapshot();
            snapshot.applied.confirmed.is_some() && snapshot.applied.groups.is_empty()
        })
        .await;
        assert!(c.context.registry.get(&key("one")).is_none());
        assert!(c.context.registry.get(&key("two")).is_some());
        let uid = c.data[0].uid.clone();
        assert!(
            c.store
                .devices()
                .load_published(&key("one"))
                .unwrap()
                .is_none()
        );
        let stored = c.store.devices().restore_groups(&uid, "local").unwrap();
        assert!(stored.groups.is_empty());
        assert_eq!(
            c.context.registry.snapshot().applied.confirmed,
            Some(stored.version)
        );
    }));
}

#[test]
fn a_delayed_group_read_keeps_the_retained_relation_until_it_finishes() {
    let rig = Rig::groups(&[
        GroupSpec {
            did: "group.a",
            model: "mijia.light.group3",
            members: &["one", "two"],
            label: "A",
        },
        GroupSpec {
            did: "group.b",
            model: "mijia.light.group3",
            members: &["three"],
            label: "B",
        },
    ]);
    block_on(rig.run(|c| async move {
        c.announce();
        until(|| c.context.registry.snapshot().applied.groups.len() == 2).await;
        let (release, gate) = flume::unbounded();
        {
            let mut cloud = c.cloud.lock().unwrap();
            cloud.groups.get_mut("group.a").unwrap().members = vec!["one".into()];
            cloud.groups.get_mut("group.b").unwrap().members = vec!["two".into()];
            cloud.detail_gates.insert("group.a".into(), gate);
        }
        c.context.request_sync();
        // Group B confirms independently while group A retains its old relation.
        until(|| group_members(&c, "group.b") == Some(vec![key("two")])).await;
        assert_eq!(
            group_members(&c, "group.a"),
            Some(vec![key("one"), key("two")])
        );
        release.send(()).unwrap();
        until(|| {
            group_members(&c, "group.a") == Some(vec![key("one")])
                && group_members(&c, "group.b") == Some(vec![key("two")])
        })
        .await;
    }));
}

#[test]
fn a_repeated_sync_does_not_duplicate_an_outstanding_group_read() {
    let rig = Rig::group(&["one", "two"], "group.1", "mijia.light.group3");
    block_on(rig.run(|c| async move {
        c.announce();
        wait_group(&c, "group.1").await;
        let reads = c.cloud.lock().unwrap().detail_counts["group.1"];
        let (release, gate) = flume::unbounded();
        c.cloud
            .lock()
            .unwrap()
            .detail_gates
            .insert("group.1".into(), gate);
        c.context.request_sync();
        until(|| c.cloud.lock().unwrap().detail_counts["group.1"] == reads + 1).await;
        c.context.request_sync();
        for _ in 0..100 {
            future::yield_now().await;
        }
        assert_eq!(
            c.cloud.lock().unwrap().detail_counts["group.1"],
            reads + 1,
            "a second sync must not start another group read"
        );
        release.send(()).unwrap();
        until(|| {
            c.context
                .live
                .borrow()
                .devices
                .get("group.1")
                .and_then(|slot| slot.group.as_ref())
                .is_some_and(|meta| meta.relation.is_some())
        })
        .await;
    }));
}

#[test]
fn a_failed_group_refresh_keeps_its_reason_and_group_classification() {
    let rig = Rig::group(&["one", "two"], "group.1", "mijia.light.group3");
    block_on(rig.run(|c| async move {
        c.announce();
        wait_group(&c, "group.1").await;
        let reads = c.cloud.lock().unwrap().detail_counts["group.1"];
        c.cloud
            .lock()
            .unwrap()
            .detail_failures
            .insert("group.1".into(), 503);
        c.context.request_sync();
        until(|| c.cloud.lock().unwrap().detail_counts["group.1"] > reads).await;
        until(|| {
            c.context
                .status
                .borrow()
                .pending
                .iter()
                .any(|pending| pending.did == "group.1" && pending.reason.contains("503"))
        })
        .await;
        assert!(
            c.context.live.borrow().devices["group.1"]
                .group
                .as_ref()
                .is_some(),
            "group classification survives a failed refresh"
        );
        c.context.reconcile_groups().unwrap();
        assert!(
            c.context
                .status
                .borrow()
                .pending
                .iter()
                .any(|pending| pending.did == "group.1" && pending.reason.contains("503")),
            "the generic evaluator reason must not erase the transport failure"
        );
        assert!(
            group_members(&c, "group.1").is_some(),
            "the last published relation stays authoritative"
        );
    }));
}

#[test]
fn a_group_waits_only_for_its_own_member_descriptions() {
    let rig = Rig::groups(&[
        GroupSpec {
            did: "group.a",
            model: "mijia.light.group3",
            members: &["one"],
            label: "A",
        },
        GroupSpec {
            did: "group.b",
            model: "mijia.light.group3",
            members: &["one", "two"],
            label: "B",
        },
    ]);
    let (release, gate) = flume::unbounded();
    rig.cloud
        .lock()
        .unwrap()
        .detail_gates
        .insert("two".into(), gate);
    block_on(rig.run(|c| async move {
        c.announce();
        until(|| {
            group_members(&c, "group.a").is_some()
                && c.context.status.borrow().pending.iter().any(|pending| {
                    pending.did == "group.b" && pending.reason.contains("descriptions")
                })
        })
        .await;
        assert!(group_members(&c, "group.b").is_none());
        assert!(c.context.live.borrow().devices["two"].admitted.is_none());
        release.send(()).unwrap();
        wait_group(&c, "group.b").await;
        assert_eq!(group_members(&c, "group.a"), Some(vec![key("one")]));
        assert_eq!(
            group_members(&c, "group.b"),
            Some(vec![key("one"), key("two")])
        );
        assert!(c.context.registry.snapshot().applied.devices.is_empty());
    }));
}

#[test]
fn heterogeneous_ct_and_rgb_members_keep_independent_capabilities() {
    let rig = Rig::group_mixed(
        &[
            ("one", "yeelink.light.light3"),
            ("two", "devcea.light.ls2307"),
        ],
        "group.1",
        "mijia.light.group3",
    );
    block_on(rig.run(|c| async move {
        c.announce();
        until(|| {
            group_members(&c, "group.1").is_some()
                && c.context.live.borrow().devices["one"].admitted.is_some()
                && c.context.live.borrow().devices["two"].admitted.is_some()
        })
        .await;
        let published = |did: &str| {
            c.context
                .registry
                .get(&key(did))
                .and_then(|physical| physical.function("main"))
                .map(|function| function.description().clone())
                .unwrap()
        };
        let fixture = |did: &str| {
            c.data
                .iter()
                .find(|record| record.device.key.id == did)
                .unwrap()
                .device
                .functions[0]
                .description
                .clone()
        };
        let one = published("one");
        let two = published("two");
        assert!(
            !one.capabilities
                .iter()
                .any(|capability| matches!(capability, crate::device::Capability::ColorRgb))
        );
        assert!(one.capabilities.iter().any(|capability| matches!(
            capability,
            crate::device::Capability::ColorTemperature(_)
        )));
        assert!(
            two.capabilities
                .iter()
                .any(|capability| matches!(capability, crate::device::Capability::ColorRgb))
        );
        assert_eq!(one.capabilities, fixture("one").capabilities);
        assert_eq!(two.capabilities, fixture("two").capabilities);
        assert_eq!(one.temperature_range(), fixture("one").temperature_range());
        assert_eq!(two.temperature_range(), fixture("two").temperature_range());
        let cloud = c.cloud.lock().unwrap();
        assert!(
            cloud
                .property_reads
                .iter()
                .all(|(did, _, _)| !did.starts_with("group."))
        );
        assert!(
            cloud
                .property_writes
                .iter()
                .all(|write| !write.0.starts_with("group."))
        );
    }));
}

#[test]
fn an_unpublished_numeric_group_refreshes_metadata_each_sync_until_members_are_known() {
    let rig = Rig::group(&["one", "two"], "43", "mijia.light.group3");
    let (release, gate) = flume::unbounded();
    rig.cloud
        .lock()
        .unwrap()
        .detail_gates
        .insert("two".into(), gate);
    block_on(rig.run(|c| async move {
        c.announce();
        // The first classification goes through the ordinary description path,
        // which preserves the model and typed relation metadata.
        until(|| {
            c.context
                .live
                .borrow()
                .devices
                .get("43")
                .is_some_and(|slot| {
                    slot.group
                        .as_ref()
                        .is_some_and(|meta| meta.relation.is_some())
                })
        })
        .await;
        assert!(c.context.registry.snapshot().applied.groups.is_empty());
        let reads = c.cloud.lock().unwrap().detail_counts["43"];
        c.context.request_sync();
        until(|| c.cloud.lock().unwrap().detail_counts["43"] > reads).await;
        assert!(c.context.registry.snapshot().applied.groups.is_empty());
        assert_eq!(
            c.context.live.borrow().devices["43"]
                .group
                .as_ref()
                .expect("classification survives the refresh")
                .model,
            "mijia.light.group3"
        );
        until(|| {
            c.context.live.borrow().devices["43"]
                .group
                .as_ref()
                .is_some_and(|meta| meta.relation.is_some())
        })
        .await;
        release.send(()).unwrap();
        wait_group(&c, "43").await;
        // Published groups are still refreshed on the normal member interval.
        let reads = c.cloud.lock().unwrap().detail_counts["43"];
        c.context.request_sync();
        until(|| c.cloud.lock().unwrap().detail_counts["43"] > reads).await;
    }));
}

#[test]
fn consecutive_confirmations_coalesce_while_the_applied_view_waits() {
    let rig = Rig::group(&["one", "two"], "group.1", "mijia.light.group3");
    block_on(rig.run(|c| async move {
        c.announce();
        wait_group(&c, "group.1").await;
        c.hold_apply();
        let relation = |c: &Control, members: Vec<&str>| {
            let mut live = c.context.live.borrow_mut();
            let meta = live
                .devices
                .get_mut("group.1")
                .unwrap()
                .group
                .as_mut()
                .unwrap();
            let crate::xiaomi::cloud::CloudGroup::Confirmed {
                members: current, ..
            } = meta.relation.as_mut().unwrap()
            else {
                panic!("expected confirmed relation");
            };
            *current = members.into_iter().map(str::to_string).collect();
        };
        relation(&c, vec!["one"]);
        c.context.reconcile_groups().unwrap();
        assert_eq!(
            c.context.registry.pending_configuration().unwrap().groups[0].members,
            vec![key("one")]
        );
        relation(&c, vec!["two"]);
        c.context.reconcile_groups().unwrap();
        assert_eq!(
            c.context.registry.pending_configuration().unwrap().groups[0].members,
            vec![key("two")],
            "consecutive confirmations merge into the latest candidate"
        );
        assert_eq!(
            group_members(&c, "group.1"),
            Some(vec![key("one"), key("two")]),
            "the applied relation never follows an uncommitted candidate"
        );
        c.context
            .live
            .borrow_mut()
            .devices
            .get_mut("group.1")
            .unwrap()
            .group
            .as_mut()
            .unwrap()
            .relation = Some(crate::xiaomi::cloud::CloudGroup::Unconfirmed);
        c.context.reconcile_groups().unwrap();
        assert_eq!(
            c.context.registry.pending_configuration().unwrap().groups[0].members,
            vec![key("two")],
            "a later failed read preserves the newest pending confirmation"
        );
        c.apply_now();
        assert_eq!(group_members(&c, "group.1"), Some(vec![key("two")]));
        assert!(c.context.registry.pending_configuration().is_none());
    }));
}

#[test]
fn a_new_admission_joins_the_group_only_at_the_apply_boundary() {
    let rig = Rig::group(&["one", "two"], "group.1", "mijia.light.group3");
    block_on(rig.run(|c| async move {
        c.hold_apply();
        c.announce();
        until(|| {
            c.context.registry.get(&key("one")).is_some()
                && c.context.registry.get(&key("two")).is_some()
                && c.context.registry.pending_configuration().is_some()
        })
        .await;
        let snapshot = c.context.registry.snapshot();
        assert!(snapshot.applied.groups.is_empty());
        assert!(snapshot.applied.confirmed.is_none());
        let pending = snapshot.pending.unwrap();
        assert_eq!(pending.groups[0].members, vec![key("one"), key("two")]);
        c.apply_now();
        assert_eq!(
            group_members(&c, "group.1"),
            Some(vec![key("one"), key("two")])
        );
        assert!(c.context.registry.pending_configuration().is_none());
    }));
}

#[test]
fn offline_members_do_not_block_admission_and_failures_stay_independent() {
    let rig = Rig::group(&["one", "two"], "group.1", "mijia.light.group3");
    rig.cloud.lock().unwrap().online.insert("one".into());
    block_on(rig.run(|c| async move {
        c.announce();
        until(|| {
            group_members(&c, "group.1").is_some()
                && function(&c.context.registry, "one", "main")
                    .snapshot()
                    .availability
                    == Availability::Available
        })
        .await;
        let one = function(&c.context.registry, "one", "main");
        let two = function(&c.context.registry, "two", "main");
        assert!(matches!(
            two.snapshot().availability,
            Availability::Unavailable(_)
        ));
        one.execute(Command::On).await.unwrap();
        assert_eq!(c.cloud.lock().unwrap().numeric_writes.len(), 1);
        assert!(two.execute(Command::On).await.is_err());
        assert_eq!(
            c.cloud.lock().unwrap().numeric_writes.len(),
            1,
            "a failed member does not retry another member"
        );
        assert_eq!(one.snapshot().power, Power::On);
        let cloud = c.cloud.lock().unwrap();
        assert!(
            cloud
                .property_reads
                .iter()
                .all(|(did, _, _)| !did.starts_with("group."))
        );
        assert!(
            cloud
                .property_writes
                .iter()
                .all(|write| !write.0.starts_with("group."))
        );
    }));
}

#[test]
fn group_members_can_use_distinct_local_and_cloud_routes() {
    let rig = Rig::group(&["one", "two"], "group.1", "mijia.light.group3");
    rig.cloud
        .lock()
        .unwrap()
        .online
        .extend(["one".into(), "two".into()]);
    block_on(rig.run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        for did in ["one", "two"] {
            peer.value(
                did,
                crate::xiaomi::capability::PropertyBinding { siid: 2, piid: 1 },
                json!(false),
            );
        }
        let control = &c;
        c.peer(peer, |peer| async move {
            let power = |did: &str| {
                control
                    .context
                    .registry
                    .get(&key(did))
                    .and_then(|physical| physical.function("main"))
                    .map(|function| function.snapshot().power)
            };
            until(|| {
                power("one") == Some(Power::Off)
                    && power("two") == Some(Power::Off)
                    && group_members(control, "group.1").is_some()
            })
            .await;
            // Two keeps its cloud access while the local session supplies one.
            control
                .context
                .live
                .borrow_mut()
                .devices
                .get_mut("two")
                .unwrap()
                .access = None;
            control.context.update_all_availability();
            let one = function(&control.context.registry, "one", "main");
            let two = function(&control.context.registry, "two", "main");
            one.execute(Command::On).await.unwrap();
            assert_eq!(peer.count("rpcReq", "one", 2), 1);
            assert!(control.cloud.lock().unwrap().numeric_writes.is_empty());
            two.execute(Command::On).await.unwrap();
            let cloud = control.cloud.lock().unwrap();
            assert_eq!(cloud.numeric_writes.len(), 1);
            assert_eq!(cloud.numeric_writes[0].0[0]["did"], "two");
            assert!(peer.count("rpcReq", "two", 2) == 0);
            assert!(
                cloud
                    .property_reads
                    .iter()
                    .all(|(did, _, _)| !did.starts_with("group."))
            );
        })
        .await;
    }));
}

#[test]
fn external_notifications_change_only_the_addressed_group_member() {
    let rig = Rig::group(&["one", "two"], "group.1", "mijia.light.group3");
    block_on(rig.run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        peer.value(
            "one",
            crate::xiaomi::capability::PropertyBinding { siid: 2, piid: 1 },
            json!(false),
        );
        peer.value(
            "two",
            crate::xiaomi::capability::PropertyBinding { siid: 2, piid: 1 },
            json!(false),
        );
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| {
                let power = |did: &str| {
                    control
                        .context
                        .registry
                        .get(&key(did))
                        .and_then(|physical| physical.function("main"))
                        .map(|function| function.snapshot().power)
                };
                power("one") == Some(Power::Off)
                    && power("two") == Some(Power::Off)
                    && control.context.active_notification("one") == Some(PathKind::Local)
                    && group_members(control, "group.1").is_some()
            })
            .await;
            peer.notify("one", 2, true);
            until(|| {
                control
                    .context
                    .registry
                    .get(&key("one"))
                    .and_then(|physical| physical.function("main"))
                    .is_some_and(|function| function.snapshot().power == Power::On)
            })
            .await;
            assert_eq!(
                control
                    .context
                    .registry
                    .get(&key("two"))
                    .and_then(|physical| physical.function("main"))
                    .unwrap()
                    .snapshot()
                    .power,
                Power::Off,
                "the sibling member keeps its own observed state"
            );
        })
        .await;
    }));
}

#[test]
fn an_unsupported_group_does_not_block_a_supported_group_with_a_shared_member() {
    let rig = Rig::groups(&[
        GroupSpec {
            did: "group.a",
            model: "mijia.light.group3",
            members: &["one"],
            label: "A",
        },
        GroupSpec {
            did: "group.b",
            model: "mijia.light.group3",
            members: &["one", "multi"],
            label: "B",
        },
    ]);
    block_on(rig.run(|c| async move {
        c.announce();
        until(|| {
            group_members(&c, "group.a").is_some()
                && c.context.status.borrow().pending.iter().any(|pending| {
                    pending.did == "group.b" && pending.reason.contains("supported single light")
                })
        })
        .await;
        assert!(group_members(&c, "group.b").is_none());
        assert_eq!(group_members(&c, "group.a"), Some(vec![key("one")]));
        let snapshot = c.context.registry.snapshot();
        assert_eq!(snapshot.applied.groups.len(), 1);
        assert!(
            snapshot
                .applied
                .devices
                .iter()
                .any(|device| device.key() == &key("multi"))
        );
    }));
}

#[test]
fn invalid_relations_are_rejected_independently_with_reasons() {
    let rig = Rig::groups(&[
        GroupSpec {
            did: "group.empty",
            model: "mijia.light.group3",
            members: &[],
            label: "Empty",
        },
        GroupSpec {
            did: "group.self",
            model: "mijia.light.group3",
            members: &["group.self"],
            label: "Self",
        },
        GroupSpec {
            did: "group.outer",
            model: "mijia.light.group3",
            members: &["one", "group.inner"],
            label: "Outer",
        },
        GroupSpec {
            did: "group.inner",
            model: "mijia.light.group3",
            members: &["two"],
            label: "Inner",
        },
    ]);
    block_on(rig.run(|c| async move {
        c.announce();
        until(|| {
            let status = c.context.status.borrow();
            [
                ("group.empty", "no confirmed members"),
                ("group.self", "contains itself"),
                ("group.outer", "contains group"),
            ]
            .iter()
            .all(|(did, reason)| {
                status
                    .pending
                    .iter()
                    .any(|pending| pending.did == *did && pending.reason.contains(reason))
            }) && group_members(&c, "group.inner").is_some()
        })
        .await;
        let snapshot = c.context.registry.snapshot();
        assert_eq!(snapshot.applied.groups.len(), 1);
        assert_eq!(snapshot.applied.groups[0].key, key("group.inner"));
        assert_eq!(snapshot.applied.groups[0].members, vec![key("two")]);
        for did in ["group.empty", "group.self", "group.outer"] {
            assert!(c.context.registry.get(&key(did)).is_none());
        }
        assert!(c.context.registry.get(&key("one")).is_some());
        assert!(c.context.registry.get(&key("two")).is_some());
    }));
}

#[test]
fn a_missing_member_description_is_retried_until_the_group_publishes() {
    let rig = Rig::group(&["one", "two"], "group.1", "mijia.light.group3");
    rig.cloud
        .lock()
        .unwrap()
        .missing_records
        .insert("two".into());
    block_on(rig.run(|c| async move {
        c.announce();
        until(|| {
            c.cloud
                .lock()
                .unwrap()
                .detail_counts
                .get("two")
                .copied()
                .unwrap_or(0)
                >= 1
                && c.context
                    .live
                    .borrow()
                    .devices
                    .get("two")
                    .is_some_and(|slot| slot.blocked.is_some())
        })
        .await;
        assert!(
            c.context.registry.snapshot().applied.groups.is_empty(),
            "an unavailable member description is pending, not a partial group"
        );
        c.cloud.lock().unwrap().missing_records.remove("two");
        c.context.request_sync();
        wait_group(&c, "group.1").await;
        assert!(c.context.registry.get(&key("one")).is_some());
        assert!(c.context.registry.get(&key("two")).is_some());
    }));
}

#[test]
fn a_failed_deletion_commit_still_closes_the_source_and_work() {
    let rig = Rig::group(&["one", "two"], "group.1", "mijia.light.group3");
    let store = rig.store.clone();
    block_on(rig.run(|c| async move {
        c.announce();
        wait_group(&c, "group.1").await;
        let record = c.context.live.borrow().devices["one"]
            .admitted
            .clone()
            .unwrap();
        let work = c.context.live.borrow().devices["one"].work.clone().unwrap();
        c.hold_apply();
        let connection = rusqlite::Connection::open(store.path()).unwrap();
        connection
            .execute_batch(
                "CREATE TRIGGER fail_relation_delete BEFORE DELETE ON group_relation
             WHEN OLD.device_id='group.1'
             BEGIN SELECT RAISE(ABORT,'deletion failed'); END;",
            )
            .unwrap();
        c.context.remove("one").unwrap();
        assert!(record.life.is_closed());
        assert!(work.life.is_closed());
        assert!(!c.context.live.borrow().devices.contains_key("one"));
        assert!(c.context.registry.get(&key("one")).is_none());
        assert!(
            record
                .physical
                .function("main")
                .unwrap()
                .execute(Command::On)
                .await
                .is_err()
        );
        assert!(matches!(c.try_apply(), Err(AdapterError::Storage(_))));
        assert!(c.context.registry.pending_configuration().is_some());
        assert_eq!(
            group_members(&c, "group.1"),
            Some(vec![key("one"), key("two")])
        );
        connection
            .execute_batch("DROP TRIGGER fail_relation_delete")
            .unwrap();
        c.try_apply().unwrap().expect("pending deletion applies");
        assert!(group_members(&c, "group.1").is_none());
        assert!(c.context.registry.get(&key("one")).is_none());
    }));
}

#[test]
fn a_restored_deleted_member_has_no_control_before_the_directory_confirms() {
    let rig = Rig::restored_group(
        &["one", "two"],
        "group.1",
        "mijia.light.group3",
        "Reading lamp",
    );
    rig.cloud.lock().unwrap().members.retain(|did| did != "one");
    block_on(rig.run(|c| async move {
        c.hold_apply();
        let member = c
            .context
            .registry
            .get(&key("one"))
            .expect("restored reservation");
        assert!(matches!(
            member.availability(),
            Availability::Unavailable(_)
        ));
        assert!(
            member
                .function("main")
                .unwrap()
                .execute(Command::On)
                .await
                .is_err()
        );
        c.announce();
        until(|| c.context.registry.get(&key("one")).is_none()).await;
        assert_eq!(
            group_members(&c, "group.1"),
            Some(vec![key("one"), key("two")])
        );
        assert!(!c.context.live.borrow().devices.contains_key("one"));
        c.try_apply().unwrap().expect("pending deletion applies");
        assert!(c.context.registry.snapshot().applied.groups.is_empty());
        assert!(c.context.registry.get(&key("one")).is_none());
        assert!(
            member
                .function("main")
                .unwrap()
                .execute(Command::On)
                .await
                .is_err()
        );
    }));
}

#[test]
fn a_first_property_read_failure_does_not_block_admission_or_fabricate_state() {
    let rig = Rig::group(&["one", "two"], "group.1", "mijia.light.group3");
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.online.extend(["one".into(), "two".into()]);
        cloud.read_status = 500;
    }
    block_on(rig.run(|c| async move {
        c.announce();
        until(|| {
            group_members(&c, "group.1").is_some()
                && c.cloud
                    .lock()
                    .unwrap()
                    .property_reads
                    .iter()
                    .any(|(did, _, _)| did == "one")
        })
        .await;
        let one = function(&c.context.registry, "one", "main");
        let two = function(&c.context.registry, "two", "main");
        assert_eq!(one.snapshot().power, Power::Unknown);
        assert_eq!(two.snapshot().power, Power::Unknown);
        assert!(c.context.registry.get(&key("group.1")).is_none());
    }));
}

#[test]
fn a_failed_device_information_batch_does_not_block_group_refresh() {
    failed_information_batch_does_not_block_group_refresh("group.1", "Reading lamp");
}

#[test]
fn a_failed_device_information_batch_does_not_block_numeric_group_refresh() {
    failed_information_batch_does_not_block_group_refresh("43", "Numeric lamp");
}

fn failed_information_batch_does_not_block_group_refresh(did: &str, label: &str) {
    let rig = Rig::restored_group(&["one", "two"], did, "mijia.light.group3", label);
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.detail_failures.insert("one".into(), 503);
        cloud.groups.get_mut(did).unwrap().members = vec!["two".into()];
    }
    block_on(rig.run(|c| async move {
        c.announce();
        // The ordinary access batch fails on one malformed/missing member, but
        // the independent group refresh is still requested and converges the
        // relation.
        until(|| c.cloud.lock().unwrap().detail_counts.contains_key(did)).await;
        until(|| {
            c.context
                .registry
                .snapshot()
                .applied
                .groups
                .iter()
                .any(|group| group.key == key(did) && group.members == vec![key("two")])
        })
        .await;
        // The failed ordinary batch must have completed before asserting.
        until(|| c.context.status.borrow().catalog_problem.is_some()).await;
        assert!(c.context.status.borrow().catalog_problem.is_some());
        assert!(c.context.registry.get(&key("one")).is_some());
        assert!(c.context.registry.get(&key("two")).is_some());
        assert!(c.context.registry.snapshot().applied.confirmed.is_some());
    }));
}

#[test]
fn a_group_with_unrepresentable_common_temperature_is_rejected_at_admission() {
    let rig = Rig::group_mixed(
        &[
            ("one", "synthetic.light.ct:2700-3700"),
            ("two", "synthetic.light.ct:3699-6500"),
        ],
        "group.1",
        "mijia.light.group3",
    );
    block_on(rig.run(|c| async move {
        c.announce();
        until(|| {
            let applied = c
                .context
                .registry
                .snapshot()
                .applied
                .groups
                .iter()
                .any(|group| group.key == key("group.1"));
            let rejected = c.context.status.borrow().pending.iter().any(|pending| {
                pending.did == "group.1" && pending.reason.contains("cannot be published")
            });
            applied || rejected
        })
        .await;
        let problem = c
            .context
            .status
            .borrow()
            .pending
            .iter()
            .filter(|pending| pending.did == "group.1")
            .map(|pending| pending.reason.clone())
            .collect::<Vec<_>>();
        assert!(
            problem
                .iter()
                .any(|reason| reason.contains("cannot be published")
                    && reason.contains("temperature range")),
            "{problem:?}"
        );
        assert!(
            c.context
                .registry
                .snapshot()
                .applied
                .groups
                .iter()
                .all(|group| group.key != key("group.1")),
            "an unrepresentable group must not publish"
        );
        // The members stay standalone candidates rather than being hidden.
        c.apply_now();
        let snapshot = c.context.registry.snapshot();
        assert!(snapshot.applied.groups.is_empty());
        assert!(
            snapshot
                .applied
                .devices
                .iter()
                .any(|device| device.key() == &key("one"))
        );
        assert!(
            snapshot
                .applied
                .devices
                .iter()
                .any(|device| device.key() == &key("two"))
        );
    }));
}

#[test]
fn an_applied_group_that_becomes_unrepresentable_is_revoked_at_the_boundary() {
    let rig = Rig::group_with_members(
        &["one", "two"],
        &["one"],
        "group.1",
        "mijia.light.group3",
        &[
            ("one", "synthetic.light.ct:2700-3700"),
            ("two", "synthetic.light.ct:3699-6500"),
        ],
    );
    block_on(rig.run(|c| async move {
        c.announce();
        wait_group(&c, "group.1").await;
        let applied_endpoint = c
            .store
            .devices()
            .load_group_identity(&key("group.1"))
            .unwrap()
            .unwrap()
            .endpoint;
        c.hold_apply();
        c.cloud
            .lock()
            .unwrap()
            .groups
            .get_mut("group.1")
            .unwrap()
            .members = vec!["one".into(), "two".into()];
        c.context.request_sync();
        until(|| {
            let applied = group_members(&c, "group.1") == Some(vec![key("one"), key("two")]);
            let rejected = c.context.status.borrow().pending.iter().any(|pending| {
                pending.did == "group.1" && pending.reason.contains("cannot be published")
            });
            applied || rejected
        })
        .await;
        assert!(
            c.context.status.borrow().pending.iter().any(|pending| {
                pending.did == "group.1" && pending.reason.contains("cannot be published")
            }),
            "the candidate must reject the unrepresentable group"
        );
        // The old applied relation, member runtime and identity/endpoint stay
        // until the apply boundary; the candidate restores both members as
        // standalone candidates instead of hiding them.
        let snapshot = c.context.registry.snapshot();
        let group = snapshot
            .applied
            .groups
            .iter()
            .find(|group| group.key == key("group.1"))
            .expect("old relation retained until the boundary");
        assert_eq!(group.members, vec![key("one")]);
        let pending = snapshot.pending.expect("staged candidate");
        assert!(
            pending
                .groups
                .iter()
                .all(|group| group.key != key("group.1"))
        );
        assert!(
            pending
                .devices
                .iter()
                .any(|device| device.key() == &key("one"))
        );
        assert!(
            pending
                .devices
                .iter()
                .any(|device| device.key() == &key("two"))
        );
        c.apply_now();
        let snapshot = c.context.registry.snapshot();
        assert!(
            snapshot
                .applied
                .groups
                .iter()
                .all(|group| group.key != key("group.1"))
        );
        assert!(
            snapshot
                .applied
                .devices
                .iter()
                .any(|device| device.key() == &key("one"))
        );
        assert!(
            snapshot
                .applied
                .devices
                .iter()
                .any(|device| device.key() == &key("two"))
        );
        let identity = c
            .store
            .devices()
            .load_group_identity(&key("group.1"))
            .unwrap()
            .unwrap();
        assert_eq!(identity.endpoint, applied_endpoint);
    }));
}

#[test]
fn a_group_without_a_common_temperature_interval_keeps_publishing() {
    let rig = Rig::group_mixed(
        &[
            ("one", "synthetic.light.ct:2700-3000"),
            ("two", "synthetic.light.ct:3500-6500"),
        ],
        "group.1",
        "mijia.light.group3",
    );
    block_on(rig.run(|c| async move {
        c.announce();
        wait_group(&c, "group.1").await;
        let uid = c.data[0].uid.clone();
        let stored = stored_group(&c.store, &uid, "group.1").unwrap();
        assert!(
            stored
                .capabilities
                .contains(&crate::device::Capability::OnOff)
        );
        assert!(
            stored.capabilities.iter().all(|capability| !matches!(
                capability,
                crate::device::Capability::ColorTemperature(_)
            )),
            "no common physical interval only drops the group temperature capability"
        );
        assert!(c.context.registry.get(&key("one")).is_some());
        assert!(c.context.registry.get(&key("two")).is_some());
    }));
}

#[test]
fn a_rejected_group_does_not_block_an_unrelated_group_or_standalone_update() {
    let rig = Rig::groups_with_members(
        &["one", "two", "three", "four"],
        &[
            GroupSpec {
                did: "group.bad",
                model: "mijia.light.group3",
                members: &["one", "two"],
                label: "Bad",
            },
            GroupSpec {
                did: "group.good",
                model: "mijia.light.group3",
                members: &["three"],
                label: "Good",
            },
        ],
        &[
            ("one", "synthetic.light.ct:2700-3700"),
            ("two", "synthetic.light.ct:3699-6500"),
            ("three", "synthetic.light.ct:2700-6500"),
            ("four", "yeelink.light.light3"),
        ],
    );
    block_on(rig.run(|c| async move {
        c.announce();
        until(|| {
            group_members(&c, "group.good") == Some(vec![key("three")])
                && c.context.status.borrow().pending.iter().any(|pending| {
                    pending.did == "group.bad" && pending.reason.contains("cannot be published")
                })
        })
        .await;
        c.apply_now();
        let snapshot = c.context.registry.snapshot();
        assert_eq!(group_members(&c, "group.good"), Some(vec![key("three")]));
        assert!(
            snapshot
                .applied
                .groups
                .iter()
                .all(|group| group.key != key("group.bad"))
        );
        assert!(
            snapshot
                .applied
                .devices
                .iter()
                .any(|device| device.key() == &key("four"))
        );
        assert!(
            snapshot
                .applied
                .devices
                .iter()
                .any(|device| device.key() == &key("one"))
        );
        assert!(
            snapshot
                .applied
                .devices
                .iter()
                .any(|device| device.key() == &key("two"))
        );
    }));
}
