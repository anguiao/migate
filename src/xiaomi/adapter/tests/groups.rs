use super::*;

fn key(did: &str) -> DeviceKey {
    DeviceKey::new("xiaomi", did)
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
    assert_eq!(snapshot.composition.confirmed, Some(1));
    assert_eq!(snapshot.composition.groups.len(), 1);
    let group = &snapshot.composition.groups[0];
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
fn live_confirmed_group_is_published_after_members_are_admitted() {
    let rig = Rig::group(&["one", "two"], "group.1", "mijia.light.group3");
    rig.cloud
        .lock()
        .unwrap()
        .names
        .insert("group.1".into(), "Reading lamp".into());
    block_on(rig.run(|c| async move {
        c.announce();
        until(|| {
            c.context
                .registry
                .snapshot()
                .composition
                .groups
                .iter()
                .any(|group| group.key == key("group.1"))
        })
        .await;
        let snapshot = c.context.registry.snapshot();
        let group = snapshot
            .composition
            .groups
            .iter()
            .find(|group| group.key == key("group.1"))
            .unwrap();
        assert_eq!(group.members, vec![key("one"), key("two")]);
        assert_eq!(group.label, "Reading lamp");
        assert!(snapshot.composition.confirmed.is_some());
        assert!(c.context.registry.get(&key("group.1")).is_none());
        for member in ["one", "two"] {
            assert!(c.context.registry.get(&key(member)).is_some());
        }
        let uid = c
            .context
            .live
            .borrow()
            .credentials
            .as_ref()
            .unwrap()
            .uid
            .clone();
        let stored = c
            .store
            .devices()
            .load_published_group(&uid, "local", &key("group.1"))
            .unwrap()
            .expect("published group");
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
        assert_eq!(snapshot.composition.confirmed, Some(1));
        assert!(
            snapshot
                .composition
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
            until(|| {
                c.context
                    .registry
                    .snapshot()
                    .composition
                    .groups
                    .iter()
                    .any(|group| group.key == key("group.1"))
            })
            .await;
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
            assert_eq!(after.composition, before.composition);
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
        until(|| {
            c.context
                .registry
                .snapshot()
                .composition
                .groups
                .iter()
                .any(|group| group.key == key("group.1"))
        })
        .await;
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
        assert!(
            c.context
                .registry
                .snapshot()
                .composition
                .groups
                .iter()
                .any(|group| group.key == key("group.1"))
        );
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
        assert!(
            c.context
                .registry
                .snapshot()
                .composition
                .groups
                .iter()
                .any(|group| group.key == key("group.1"))
        );
        assert!(c.context.registry.get(&key("two")).is_some());
    }));
}

#[test]
fn confirmed_overlapping_group_candidates_are_revoked_with_diagnostics() {
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
            members: &["two", "three"],
            label: "B",
        },
    ]);
    block_on(rig.run(|c| async move {
        c.announce();
        until(|| {
            let status = c.context.status.borrow();
            ["group.a", "group.b"].iter().all(|did| {
                status
                    .pending
                    .iter()
                    .any(|pending| pending.did == *did && pending.reason.contains("overlap"))
            }) && ["one", "two", "three"]
                .iter()
                .all(|did| c.context.registry.get(&key(did)).is_some())
        })
        .await;
        let snapshot = c.context.registry.snapshot();
        assert!(snapshot.composition.groups.is_empty());
        for member in ["one", "two", "three"] {
            assert!(
                c.context.registry.get(&key(member)).is_some(),
                "{member} stays standalone"
            );
        }
        for group in ["group.a", "group.b"] {
            assert!(c.context.registry.get(&key(group)).is_none());
        }
        assert!(
            c.cloud
                .lock()
                .unwrap()
                .property_reads
                .iter()
                .all(|(did, _, _)| !did.starts_with("group."))
        );
    }));
}

#[test]
fn an_unknown_retained_group_defers_a_new_claim_while_unrelated_groups_advance() {
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
        until(|| c.context.registry.snapshot().composition.groups.len() == 3).await;
        {
            let mut cloud = c.cloud.lock().unwrap();
            cloud.groups.get_mut("group.a").unwrap().missing = true;
            cloud.groups.get_mut("group.b").unwrap().members = vec!["one".into()];
        }
        c.context.request_sync();
        until(|| {
            c.context
                .status
                .borrow()
                .pending
                .iter()
                .any(|pending| pending.did == "group.b" && pending.reason.contains("group.a"))
        })
        .await;
        let snapshot = c.context.registry.snapshot();
        let group = |did: &str| {
            snapshot
                .composition
                .groups
                .iter()
                .find(|group| group.key == key(did))
                .unwrap()
                .members
                .clone()
        };
        assert_eq!(group("group.a"), vec![key("one"), key("two")]);
        assert_eq!(group("group.b"), vec![key("three")]);
        assert_eq!(group("group.c"), vec![key("four")]);
        let pending = c
            .context
            .status
            .borrow()
            .pending
            .iter()
            .map(|pending| format!("{}: {}", pending.did, pending.reason))
            .collect::<Vec<_>>();
        assert!(
            pending
                .iter()
                .any(|reason| reason.contains("group.b") && reason.contains("group.a")),
            "{pending:?}"
        );
    }));
}

#[test]
fn deferred_relations_defer_other_claims_transitively() {
    let rig = Rig::groups(&[
        GroupSpec {
            did: "group.a",
            model: "mijia.light.group3",
            members: &["four"],
            label: "A",
        },
        GroupSpec {
            did: "group.b",
            model: "mijia.light.group3",
            members: &["one"],
            label: "B",
        },
        GroupSpec {
            did: "group.c",
            model: "mijia.light.group3",
            members: &["six"],
            label: "C",
        },
    ]);
    block_on(rig.run(|c| async move {
        c.announce();
        until(|| c.context.registry.snapshot().composition.groups.len() == 3).await;
        {
            let mut cloud = c.cloud.lock().unwrap();
            cloud.groups.get_mut("group.a").unwrap().members = vec!["one".into()];
            cloud.groups.get_mut("group.b").unwrap().missing = true;
            cloud.groups.get_mut("group.c").unwrap().members = vec!["four".into()];
        }
        c.context.request_sync();
        until(|| {
            let pending = c.context.status.borrow().pending.len();
            pending >= 2
        })
        .await;
        let snapshot = c.context.registry.snapshot();
        let group = |did: &str| {
            snapshot
                .composition
                .groups
                .iter()
                .find(|group| group.key == key(did))
                .unwrap()
                .members
                .clone()
        };
        assert_eq!(group("group.a"), vec![key("four")]);
        assert_eq!(group("group.b"), vec![key("one")]);
        assert_eq!(group("group.c"), vec![key("six")]);
        {
            let mut cloud = c.cloud.lock().unwrap();
            cloud.groups.get_mut("group.b").unwrap().missing = false;
            cloud.groups.get_mut("group.b").unwrap().members = Vec::new();
        }
        c.context.request_sync();
        until(|| {
            let snapshot = c.context.registry.snapshot();
            let members = |did: &str| {
                snapshot
                    .composition
                    .groups
                    .iter()
                    .find(|group| group.key == key(did))
                    .map(|group| group.members.clone())
            };
            members("group.a") == Some(vec![key("one")])
                && members("group.c") == Some(vec![key("four")])
        })
        .await;
    }));
}

#[test]
fn a_confirmed_member_deletion_revokes_the_group_even_when_group_reads_fail() {
    let rig = Rig::group(&["one", "two"], "group.1", "mijia.light.group3");
    block_on(rig.run(|c| async move {
        c.announce();
        until(|| {
            c.context
                .registry
                .snapshot()
                .composition
                .groups
                .iter()
                .any(|group| group.key == key("group.1"))
        })
        .await;
        {
            let mut cloud = c.cloud.lock().unwrap();
            cloud.members = vec!["group.1".into(), "two".into()];
            cloud.detail_failures.insert("group.1".into(), 500);
        }
        c.context.request_sync();
        until(|| {
            let snapshot = c.context.registry.snapshot();
            snapshot.composition.confirmed.is_some() && snapshot.composition.groups.is_empty()
        })
        .await;
        assert!(c.context.registry.get(&key("one")).is_none());
        assert!(c.context.registry.get(&key("two")).is_some());
        assert!(!c.context.live.borrow().devices.contains_key("one"));
        let uid = c
            .context
            .live
            .borrow()
            .credentials
            .as_ref()
            .unwrap()
            .uid
            .clone();
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
        assert_eq!(snapshot.composition.confirmed, Some(stored.version));
    }));
}

#[test]
fn compatible_member_binding_update_keeps_the_instance_group_and_endpoint() {
    let rig = Rig::group(&["one", "two"], "group.1", "mijia.light.group3");
    block_on(rig.run(|c| async move {
        c.announce();
        until(|| {
            c.context
                .registry
                .snapshot()
                .composition
                .groups
                .iter()
                .any(|group| group.key == key("group.1"))
        })
        .await;
        let uid = c
            .context
            .live
            .borrow()
            .credentials
            .as_ref()
            .unwrap()
            .uid
            .clone();
        let endpoint = c
            .store
            .devices()
            .load_published_group(&uid, "local", &key("group.1"))
            .unwrap()
            .unwrap()
            .endpoint;
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
            .composition
            .groups
            .iter()
            .find(|group| group.key == key("group.1"))
            .expect("group survives a member binding replacement");
        assert_eq!(group.members, vec![key("one"), key("two")]);
        assert!(snapshot.composition.confirmed.is_some());
        let stored = c
            .store
            .devices()
            .load_published_group(&uid, "local", &key("group.1"))
            .unwrap()
            .unwrap();
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
        until(|| {
            c.context
                .registry
                .snapshot()
                .composition
                .groups
                .iter()
                .any(|group| group.key == key("group.1"))
        })
        .await;
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
        let group_members = |c: &Control, did: &str| {
            c.context
                .registry
                .snapshot()
                .composition
                .groups
                .iter()
                .find(|group| group.key == key(did))
                .map(|group| group.members.clone())
        };

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
        until(|| {
            c.context
                .registry
                .snapshot()
                .composition
                .groups
                .iter()
                .any(|group| group.key == key("group.1"))
        })
        .await;
        let mut incompatible = {
            let uid = c
                .context
                .live
                .borrow()
                .credentials
                .as_ref()
                .unwrap()
                .uid
                .clone();
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
            snapshot.composition.groups.len() == 1
                && c.context.live.borrow().devices["one"].blocked.is_some()
        })
        .await;
        let reads = c.cloud.lock().unwrap().detail_counts["one"];
        c.cloud.lock().unwrap().missing_records.insert("one".into());
        c.context.request_sync();
        until(|| c.cloud.lock().unwrap().detail_counts["one"] > reads).await;
        c.context.reconcile_groups().unwrap();
        let snapshot = c.context.registry.snapshot();
        assert_eq!(snapshot.composition.groups.len(), 1);
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
            snapshot.composition.confirmed.is_some() && snapshot.composition.groups.is_empty()
        })
        .await;
        assert!(c.context.registry.get(&key("one")).is_none());
        assert!(c.context.registry.get(&key("two")).is_some());
        let uid = c
            .context
            .live
            .borrow()
            .credentials
            .as_ref()
            .unwrap()
            .uid
            .clone();
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
            c.context.registry.snapshot().composition.confirmed,
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
        until(|| c.context.registry.snapshot().composition.groups.len() == 2).await;
        let reads = c.cloud.lock().unwrap().detail_counts["group.b"];
        let (release, gate) = flume::unbounded();
        {
            let mut cloud = c.cloud.lock().unwrap();
            cloud.groups.get_mut("group.a").unwrap().members = vec!["one".into()];
            cloud.groups.get_mut("group.b").unwrap().members = vec!["two".into()];
            cloud.detail_gates.insert("group.a".into(), gate);
        }
        c.context.request_sync();
        // Group B finishes while group A is still refreshing: it must not
        // claim a member that A's retained relation still owns.
        until(|| c.cloud.lock().unwrap().detail_counts["group.b"] > reads).await;
        for _ in 0..50 {
            future::yield_now().await;
        }
        let snapshot = c.context.registry.snapshot();
        let group = |did: &str| {
            snapshot
                .composition
                .groups
                .iter()
                .find(|group| group.key == key(did))
                .unwrap()
                .members
                .clone()
        };
        assert_eq!(group("group.a"), vec![key("one"), key("two")]);
        assert_eq!(group("group.b"), vec![key("three")]);
        release.send(()).unwrap();
        until(|| {
            let snapshot = c.context.registry.snapshot();
            let members = |did: &str| {
                snapshot
                    .composition
                    .groups
                    .iter()
                    .find(|group| group.key == key(did))
                    .map(|group| group.members.clone())
            };
            members("group.a") == Some(vec![key("one")])
                && members("group.b") == Some(vec![key("two")])
        })
        .await;
    }));
}

#[test]
fn a_repeated_sync_does_not_duplicate_an_outstanding_group_read() {
    let rig = Rig::group(&["one", "two"], "group.1", "mijia.light.group3");
    block_on(rig.run(|c| async move {
        c.announce();
        until(|| {
            c.context
                .registry
                .snapshot()
                .composition
                .groups
                .iter()
                .any(|group| group.key == key("group.1"))
        })
        .await;
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
        until(|| {
            c.context
                .registry
                .snapshot()
                .composition
                .groups
                .iter()
                .any(|group| group.key == key("group.1"))
        })
        .await;
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
            c.context
                .registry
                .snapshot()
                .composition
                .groups
                .iter()
                .any(|group| group.key == key("group.1")),
            "the last published relation stays authoritative"
        );
    }));
}

#[test]
fn confirmed_overlap_revokes_even_while_a_member_is_pending_and_unrelated_groups_advance() {
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
        GroupSpec {
            did: "group.c",
            model: "mijia.light.group3",
            members: &["three"],
            label: "C",
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
            let status = c.context.status.borrow();
            ["group.a", "group.b"].iter().all(|did| {
                status
                    .pending
                    .iter()
                    .any(|pending| pending.did == *did && pending.reason.contains("overlap"))
            }) && c
                .context
                .registry
                .snapshot()
                .composition
                .groups
                .iter()
                .any(|group| group.key == key("group.c"))
        })
        .await;
        let snapshot = c.context.registry.snapshot();
        assert_eq!(snapshot.composition.groups.len(), 1);
        assert_eq!(snapshot.composition.groups[0].key, key("group.c"));
        assert!(c.context.live.borrow().devices["one"].admitted.is_some());
        assert!(c.context.live.borrow().devices["two"].admitted.is_none());
        release.send(()).unwrap();
        until(|| c.context.live.borrow().devices["two"].admitted.is_some()).await;
        for _ in 0..50 {
            future::yield_now().await;
        }
        let snapshot = c.context.registry.snapshot();
        assert_eq!(snapshot.composition.groups.len(), 1);
        assert_eq!(snapshot.composition.groups[0].key, key("group.c"));
        assert!(c.context.registry.get(&key("one")).is_some());
        assert!(c.context.registry.get(&key("two")).is_some());
    }));
}

#[test]
fn physical_and_virtual_groups_publish_with_the_same_rules() {
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
    rig.cloud
        .lock()
        .unwrap()
        .groups
        .get_mut("group.b")
        .unwrap()
        .physical = false;
    block_on(rig.run(|c| async move {
        c.announce();
        until(|| c.context.registry.snapshot().composition.groups.len() == 2).await;
        let snapshot = c.context.registry.snapshot();
        let members = |did: &str| {
            snapshot
                .composition
                .groups
                .iter()
                .find(|group| group.key == key(did))
                .unwrap()
                .members
                .clone()
        };
        assert_eq!(members("group.a"), vec![key("one"), key("two")]);
        assert_eq!(members("group.b"), vec![key("three")]);
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
            c.context
                .registry
                .snapshot()
                .composition
                .groups
                .iter()
                .any(|group| group.key == key("group.1"))
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
        assert!(c.context.registry.snapshot().composition.groups.is_empty());
        let reads = c.cloud.lock().unwrap().detail_counts["43"];
        c.context.request_sync();
        until(|| c.cloud.lock().unwrap().detail_counts["43"] > reads).await;
        assert!(c.context.registry.snapshot().composition.groups.is_empty());
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
        until(|| {
            c.context
                .registry
                .snapshot()
                .composition
                .groups
                .iter()
                .any(|group| group.key == key("43"))
        })
        .await;
        // Published groups are still refreshed on the normal member interval.
        let reads = c.cloud.lock().unwrap().detail_counts["43"];
        c.context.request_sync();
        until(|| c.cloud.lock().unwrap().detail_counts["43"] > reads).await;
    }));
}

#[test]
fn a_composition_commit_failure_keeps_the_previous_topology_and_members() {
    let rig = Rig::group(&["one", "two"], "group.1", "mijia.light.group3");
    let store = rig.store.clone();
    block_on(rig.run(|c| async move {
        c.announce();
        until(|| {
            c.context
                .registry
                .snapshot()
                .composition
                .groups
                .iter()
                .any(|group| group.key == key("group.1"))
        })
        .await;
        let record = c.context.live.borrow().devices["one"]
            .admitted
            .clone()
            .unwrap();
        let work = c.context.live.borrow().devices["one"].work.clone().unwrap();
        let uid = c
            .context
            .live
            .borrow()
            .credentials
            .as_ref()
            .unwrap()
            .uid
            .clone();
        let endpoint = c
            .store
            .devices()
            .load_published_group(&uid, "local", &key("group.1"))
            .unwrap()
            .unwrap()
            .endpoint;
        rusqlite::Connection::open(store.path())
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER fail_relation_insert BEFORE INSERT ON group_relation
                 WHEN NEW.device_id='group.1'
                 BEGIN SELECT RAISE(ABORT,'composition failed'); END;",
            )
            .unwrap();
        {
            let mut live = c.context.live.borrow_mut();
            let meta = live
                .devices
                .get_mut("group.1")
                .unwrap()
                .group
                .as_mut()
                .unwrap();
            let crate::xiaomi::cloud::CloudGroup::Confirmed { members, .. } =
                meta.relation.as_mut().unwrap()
            else {
                panic!("expected confirmed relation");
            };
            *members = vec!["one".into()];
        }
        let error = c.context.reconcile_groups().unwrap_err();
        assert!(matches!(error, AdapterError::Storage(_)));
        let snapshot = c.context.registry.snapshot();
        let group = snapshot
            .composition
            .groups
            .iter()
            .find(|group| group.key == key("group.1"))
            .expect("previous relation stays published");
        assert_eq!(group.members, vec![key("one"), key("two")]);
        assert_eq!(snapshot.composition.confirmed, Some(1));
        let live = c.context.live.borrow();
        assert!(Rc::ptr_eq(
            live.devices["one"].admitted.as_ref().unwrap(),
            &record
        ));
        assert!(Rc::ptr_eq(
            live.devices["one"].work.as_ref().unwrap(),
            &work
        ));
        assert!(!record.life.is_closed());
        drop(live);
        let stored = c
            .store
            .devices()
            .load_published_group(&uid, "local", &key("group.1"))
            .unwrap()
            .unwrap();
        assert_eq!(stored.members, vec![key("one"), key("two")]);
        assert_eq!(stored.endpoint, endpoint);
    }));
}

#[test]
fn offline_members_do_not_block_admission_and_failures_stay_independent() {
    let rig = Rig::group(&["one", "two"], "group.1", "mijia.light.group3");
    rig.cloud.lock().unwrap().online.insert("one".into());
    block_on(rig.run(|c| async move {
        c.announce();
        until(|| {
            c.context
                .registry
                .snapshot()
                .composition
                .groups
                .iter()
                .any(|group| group.key == key("group.1"))
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
                    && control
                        .context
                        .registry
                        .snapshot()
                        .composition
                        .groups
                        .iter()
                        .any(|group| group.key == key("group.1"))
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
                    && control
                        .context
                        .registry
                        .snapshot()
                        .composition
                        .groups
                        .iter()
                        .any(|group| group.key == key("group.1"))
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
fn a_confirmed_overlap_with_an_unsupported_group_blocks_the_supported_group() {
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
            let status = c.context.status.borrow();
            status
                .pending
                .iter()
                .any(|pending| pending.did == "group.a" && pending.reason.contains("overlap"))
                && status.pending.iter().any(|pending| {
                    pending.did == "group.b" && pending.reason.contains("supported single light")
                })
                && c.context.registry.get(&key("one")).is_some()
        })
        .await;
        assert!(
            c.context.registry.snapshot().composition.groups.is_empty(),
            "raw confirmed membership overlap outranks the unsupported sibling"
        );
        assert!(c.context.registry.get(&key("multi")).is_some());
    }));
}

#[test]
fn empty_and_nested_group_confirmations_are_rejected_with_reasons() {
    let rig = Rig::groups(&[
        GroupSpec {
            did: "group.empty",
            model: "mijia.light.group3",
            members: &[],
            label: "Empty",
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
            status.pending.iter().any(|pending| {
                pending.did == "group.empty" && pending.reason.contains("no confirmed members")
            }) && status.pending.iter().any(|pending| {
                pending.did == "group.outer" && pending.reason.contains("contains group")
            }) && c
                .context
                .registry
                .snapshot()
                .composition
                .groups
                .iter()
                .any(|group| group.key == key("group.inner"))
        })
        .await;
        let snapshot = c.context.registry.snapshot();
        assert_eq!(snapshot.composition.groups.len(), 1);
        assert_eq!(snapshot.composition.groups[0].key, key("group.inner"));
        assert_eq!(snapshot.composition.groups[0].members, vec![key("two")]);
        for did in ["group.empty", "group.outer"] {
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
            c.context.registry.snapshot().composition.groups.is_empty(),
            "an unavailable member description is pending, not a partial group"
        );
        c.cloud.lock().unwrap().missing_records.remove("two");
        c.context.request_sync();
        until(|| {
            c.context
                .registry
                .snapshot()
                .composition
                .groups
                .iter()
                .any(|group| group.key == key("group.1"))
        })
        .await;
        assert!(c.context.registry.get(&key("one")).is_some());
        assert!(c.context.registry.get(&key("two")).is_some());
    }));
}

#[test]
fn a_failed_confirmed_deletion_commit_keeps_the_relation_and_member_runtime() {
    let rig = Rig::group(&["one", "two"], "group.1", "mijia.light.group3");
    let store = rig.store.clone();
    block_on(rig.run(|c| async move {
        c.announce();
        until(|| {
            c.context
                .registry
                .snapshot()
                .composition
                .groups
                .iter()
                .any(|group| group.key == key("group.1"))
        })
        .await;
        let record = c.context.live.borrow().devices["one"]
            .admitted
            .clone()
            .unwrap();
        let work = c.context.live.borrow().devices["one"].work.clone().unwrap();
        rusqlite::Connection::open(store.path())
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER fail_relation_delete BEFORE DELETE ON group_relation
                 WHEN OLD.device_id='group.1'
                 BEGIN SELECT RAISE(ABORT,'deletion failed'); END;",
            )
            .unwrap();
        let error = c
            .context
            .reconcile_groups_removed(&[key("one")])
            .unwrap_err();
        assert!(matches!(error, AdapterError::Storage(_)));
        let snapshot = c.context.registry.snapshot();
        let group = snapshot
            .composition
            .groups
            .iter()
            .find(|group| group.key == key("group.1"))
            .expect("deletion rollback keeps the relation published");
        assert_eq!(group.members, vec![key("one"), key("two")]);
        assert_eq!(snapshot.composition.confirmed, Some(1));
        let live = c.context.live.borrow();
        assert!(live.devices.contains_key("one"));
        assert!(Rc::ptr_eq(
            live.devices["one"].admitted.as_ref().unwrap(),
            &record
        ));
        assert!(Rc::ptr_eq(
            live.devices["one"].work.as_ref().unwrap(),
            &work
        ));
        assert!(!record.life.is_closed());
        drop(live);
        let uid = c
            .context
            .live
            .borrow()
            .credentials
            .as_ref()
            .unwrap()
            .uid
            .clone();
        let stored = c
            .store
            .devices()
            .load_published_group(&uid, "local", &key("group.1"))
            .unwrap()
            .unwrap();
        assert_eq!(stored.members, vec![key("one"), key("two")]);
        assert!(
            c.store
                .devices()
                .load_published(&key("one"))
                .unwrap()
                .is_some()
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
            c.context
                .registry
                .snapshot()
                .composition
                .groups
                .iter()
                .any(|group| group.key == key("group.1"))
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
                .composition
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
        assert!(
            c.context
                .registry
                .snapshot()
                .composition
                .confirmed
                .is_some()
        );
    }));
}
