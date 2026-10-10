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
fn a_confirmed_unsupported_replacement_revokes_even_with_an_admitted_record() {
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
        c.context
            .live
            .borrow_mut()
            .devices
            .get_mut("one")
            .unwrap()
            .blocked_unsupported = true;
        c.context.reconcile_groups().unwrap();
        let snapshot = c.context.registry.snapshot();
        assert!(
            !snapshot
                .composition
                .groups
                .iter()
                .any(|group| group.key == key("group.1"))
        );
        assert!(c.context.registry.get(&key("one")).is_some());
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
                .load_published_group(&uid, "local", &key("group.1"))
                .unwrap()
                .is_none()
        );
    }));
}
