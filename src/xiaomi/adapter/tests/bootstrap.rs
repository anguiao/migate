use super::*;
use crate::storage::tests::fixtures::identity;

#[test]
fn equivalent_startup_recomputation_keeps_versions_and_uses_no_network() {
    let rig = Rig::new(true, &["one"]);
    let before = rig.store.devices().load_configuration().unwrap();
    let app = crate::app::App::open(rig.source.source_handle(), rig.store.clone()).unwrap();
    assert!(app.plan().unwrap().is_none());
    assert_eq!(app.current().configuration, before);
    assert_eq!(rig.store.devices().load_configuration().unwrap(), before);
    assert!(rig.cloud.lock().unwrap().instance_reads.is_empty());
}

#[test]
fn previously_unsupported_cached_source_can_gain_its_first_function_on_startup() {
    let rig = Rig::new(true, &["one"]);
    let mut previous = rig.store.devices().load_configuration().unwrap();
    previous.devices.clear();
    previous.descriptions[0].diagnostic =
        Some("previous rules did not recognize this source".into());
    rig.store
        .devices()
        .commit_configuration(&previous, &[])
        .unwrap();
    let (restored, _) = XiaomiAdapter::build(
        rig.store.clone(),
        rig.source.cloud.clone(),
        rig.discovery.clone(),
        rig.clock.clone(),
    )
    .unwrap();
    let app = crate::app::App::open(restored.source_handle(), rig.store.clone()).unwrap();
    assert!(app.frozen().is_empty());
    let plan = app.plan().unwrap().unwrap();
    app.apply(app.prepare(plan)).unwrap();
    assert_eq!(app.frozen().len(), 1);
    let saved = rig.store.devices().load_configuration().unwrap();
    assert_eq!(
        saved.descriptions[0].instance,
        previous.descriptions[0].instance
    );
    assert!(saved.descriptions[0].diagnostic.is_none());
    assert!(saved.devices[0].native.is_some());
    assert!(rig.cloud.lock().unwrap().instance_reads.is_empty());
    assert!(app.plan().unwrap().is_none());
}

#[test]
fn failed_local_recomputation_is_reported_and_retried_without_changing_the_view() {
    let rig = Rig::new(true, &["one"]);
    let app = crate::app::App::open(rig.source.source_handle(), rig.store.clone()).unwrap();
    let before = app.current().configuration.clone();
    {
        let mut live = rig.source.context.live.borrow_mut();
        let slot = live.devices.get_mut("one").unwrap();
        slot.group = true;
        slot.recompute_failed = true;
    }
    let error = app.plan().unwrap_err();
    assert!(
        error
            .to_string()
            .contains("recompute cached Xiaomi description")
    );
    app.fail(error.to_string());
    assert_eq!(app.current().configuration, before);
    assert_eq!(rig.store.devices().load_configuration().unwrap(), before);
    rig.source
        .context
        .live
        .borrow_mut()
        .devices
        .get_mut("one")
        .unwrap()
        .group = false;
    assert!(app.plan().unwrap().is_none());
    assert!(app.failure().is_none());
    assert!(!rig.source.context.live.borrow().devices["one"].recompute_failed);
}

#[test]
fn startup_reparses_cached_source_before_committing_new_capabilities() {
    let rig = Rig::sensor_without_battery("sensor");
    let key = DeviceKey::new("xiaomi", "sensor");
    let mut before = rig.store.devices().load_configuration().unwrap();
    before.descriptions[0].instance = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/miaomiaoce-t6.json"
    ))
    .unwrap();
    rig.store
        .devices()
        .commit_configuration(&before, &[])
        .unwrap();
    let (restored, _) = XiaomiAdapter::build(
        rig.store.clone(),
        rig.source.cloud.clone(),
        rig.discovery.clone(),
        rig.clock.clone(),
    )
    .unwrap();
    let identity = before.device(&key).unwrap();
    assert!(
        identity
            .native
            .as_ref()
            .unwrap()
            .device()
            .battery()
            .is_none()
    );
    let old_work = restored.context.live.borrow().devices["sensor"]
        .admitted
        .clone()
        .unwrap();
    let app = crate::app::App::open(restored.source_handle(), rig.store.clone()).unwrap();
    let plan = app
        .plan()
        .unwrap()
        .expect("current rules add cached battery");
    assert!(
        plan.configuration
            .device(&key)
            .unwrap()
            .native
            .as_ref()
            .unwrap()
            .device()
            .battery()
            .is_some()
    );
    assert!(old_work.native.device().battery().is_none());
    let prepared = app.prepare(plan);
    app.apply(prepared).unwrap();
    let after = rig.store.devices().load_configuration().unwrap();
    let changed = after.device(&key).unwrap();
    assert_eq!(changed.unique_id, identity.unique_id);
    assert_eq!(changed.endpoint, identity.endpoint);
    assert_eq!(changed.functions, identity.functions);
    assert!(
        changed
            .native
            .as_ref()
            .unwrap()
            .device()
            .battery()
            .is_some()
    );
    assert!(rig.cloud.lock().unwrap().instance_reads.is_empty());
}

#[test]
fn failed_startup_recomputation_keeps_committed_bindings_and_revocation_overlay() {
    let rig = Rig::sensors(&["sensor", "revoked"]);
    let key = DeviceKey::new("xiaomi", "sensor");
    let revoked = DeviceKey::new("xiaomi", "revoked");
    let mut before = rig.store.devices().load_configuration().unwrap();
    before
        .devices
        .iter_mut()
        .find(|device| device.key == key)
        .unwrap()
        .native = Some(sensor_native_without_battery("sensor"));
    rig.store
        .devices()
        .commit_configuration(&before, &[])
        .unwrap();
    rig.store
        .devices()
        .record_revocations(&[crate::configuration::Revocation {
            key: revoked.clone(),
            reason: "confirmed removal".into(),
        }])
        .unwrap();
    let (restored, _) = XiaomiAdapter::build(
        rig.store.clone(),
        rig.source.cloud.clone(),
        rig.discovery.clone(),
        rig.clock.clone(),
    )
    .unwrap();
    let app = crate::app::App::open(restored.source_handle(), rig.store.clone()).unwrap();
    assert_eq!(app.frozen().len(), 1);
    assert!(!app.frozen()[0].device.description().has_battery());
    let plan = app.plan().unwrap().unwrap();
    assert!(
        plan.configuration
            .device(&key)
            .unwrap()
            .native
            .as_ref()
            .unwrap()
            .device()
            .battery()
            .is_some()
    );
    assert!(
        plan.configuration
            .device(&revoked)
            .unwrap()
            .native
            .is_none()
    );
    let db = rusqlite::Connection::open(rig.store.path()).unwrap();
    db.execute_batch("CREATE TRIGGER fail_startup BEFORE UPDATE ON bridge_configuration BEGIN SELECT RAISE(ABORT, 'injected'); END;").unwrap();
    let prepared = app.prepare(plan);
    assert!(app.apply(prepared).is_err());
    assert_eq!(rig.store.devices().load_configuration().unwrap(), before);
    assert_eq!(app.current().configuration, before);
    assert_eq!(app.frozen().len(), 1);
    assert!(!app.frozen()[0].device.description().has_battery());
    assert!(app.failure().is_some());
    assert!(rig.cloud.lock().unwrap().instance_reads.is_empty());
    db.execute_batch("DROP TRIGGER fail_startup").unwrap();
    let prepared = app.prepare(app.plan().unwrap().unwrap());
    app.apply(prepared).unwrap();
    assert!(app.frozen()[0].device.description().has_battery());
    assert!(
        app.current()
            .configuration
            .device(&revoked)
            .unwrap()
            .native
            .is_none()
    );
}

#[test]
fn restored_topology_is_the_selected_scope_and_published_intersection() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let tls = TlsFixture::new(false);
    store.xiaomi().replace(&tls.credentials).unwrap();
    let kept = cached(&tls.credentials.uid, "kept", true);
    let deleted = cached(&tls.credentials.uid, "deleted", false);
    let other = cached(&tls.credentials.uid, "other-home", false);
    let foreign = cached("foreign", "foreign", false);
    let mut kept_record = identity(&kept.physical_description("saved name"), 2);
    kept_record.native = Some(kept);
    let configuration = crate::configuration::CommittedConfiguration {
        scope: Some(scope(&tls.credentials.uid)),
        root_version: 1,
        descriptions: vec![crate::storage::tests::fixtures::raw_for(
            kept_record.native.as_ref().unwrap(),
        )],
        devices: vec![
            kept_record,
            identity(&deleted.physical_description("deleted"), 6),
            identity(&other.physical_description("other"), 8),
            identity(&foreign.physical_description("foreign"), 10),
        ],
        groups: Vec::new(),
    };
    store
        .devices()
        .commit_configuration(&configuration, &[])
        .unwrap();
    let (base, requests) = dynamic_mock_server(1, |_| MockResponse::json(503, "{}"));
    let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    let (_events, receiver) = flume::unbounded();
    let (discovery, _driver) = Discovery::with_events(receiver);
    let (source, ready) = XiaomiAdapter::new(store, cloud, discovery).unwrap();
    block_on(poll_once(ready.wait()))
        .expect("cache readiness is synchronous")
        .unwrap();
    let registry = &source.context.registry;
    assert_eq!(registry.list().len(), 1);
    let physical = registry.get(&DeviceKey::new("xiaomi", "kept")).unwrap();
    assert_eq!(physical.functions().len(), 3);
    assert_eq!(physical.description().label(), "saved name");
    for function in physical.functions() {
        assert_eq!(function.snapshot().power(), Some(Power::Unknown));
        assert!(matches!(
            function.snapshot().availability,
            Availability::Unavailable(_)
        ));
    }
    assert_eq!(requests.try_iter().count(), 0);
    drop(source);
}

#[test]
fn prepared_accounts_can_deliver_an_empty_registry() {
    block_on(Rig::new(false, &[]).run(|mut c| async move {
        c.ready.take().unwrap().wait().await.unwrap();
        assert!(c.context.registry.list().is_empty());
    }));
}

#[test]
fn cached_sources_are_not_classified_as_groups_by_names_or_models() {
    let rig = Rig::light("xiaomi.light.ceil04");
    let mut state = rig.store.devices().load_configuration().unwrap();
    let single = state.devices[0].clone();
    let mut groups = Vec::new();
    for (endpoint, did, model) in [
        (4, "group.42", "xiaomi.light.ceil04"),
        (6, "43", "mijia.light.group3"),
    ] {
        let native = &rig.data[0];
        let group = crate::xiaomi::capability::NativeDescription::new(
            model.into(),
            native.instance_type().into(),
            super::support::device_with_key(native.device(), did),
        )
        .unwrap();
        let mut device = identity(&group.physical_description("Saved group"), endpoint);
        device.native = Some(group.clone());
        state
            .descriptions
            .push(crate::storage::tests::fixtures::raw_for(&group));
        state.devices.push(device);
        groups.push(group);
    }
    rig.store
        .devices()
        .commit_configuration(&state, &[])
        .unwrap();
    for _ in 0..2 {
        let (source, ready) = XiaomiAdapter::build(
            rig.store.clone(),
            rig.source.cloud.clone(),
            rig.discovery.clone(),
            rig.clock.clone(),
        )
        .unwrap();
        block_on(poll_once(ready.wait())).unwrap().unwrap();
        let registry = &source.context.registry;
        assert_eq!(registry.list().len(), 3);
        assert!(registry.get(&single.key).is_some());
        assert_eq!(
            rig.store
                .devices()
                .load_configuration()
                .unwrap()
                .derive()
                .published
                .len(),
            3
        );
        for group in &groups {
            assert!(registry.get(group.device().key()).is_some());
            assert!(!source.context.live.borrow().devices[&group.device().key().id].group);
            assert_eq!(
                rig.store
                    .devices()
                    .load_configuration()
                    .unwrap()
                    .device(group.device().key())
                    .unwrap()
                    .native
                    .clone(),
                Some(group.clone())
            );
        }
        drop(source);
    }
    assert_eq!(rig.cloud.lock().unwrap().homes, 0);
}

#[test]
fn multiple_devices_publish_before_initial_reads_and_unknown_online_commands_work() {
    block_on(
        Rig::new(false, &["one", "multi", "two"]).run(|c| async move {
            c.announce();
            let peer = Rc::new(Peer::default());
            peer.state.borrow_mut().hold_reads = true;
            let control = &c;
            c.peer(peer, |peer| async move {
                until(|| {
                    control.context.registry.list().len() == 3
                        && control
                            .context
                            .registry
                            .list()
                            .iter()
                            .all(|physical| physical.availability() == Availability::Available)
                })
                .await;
                let registry = &control.context.registry;
                assert_eq!(
                    registry
                        .get(&DeviceKey::new("xiaomi", "multi"))
                        .unwrap()
                        .functions()
                        .len(),
                    3
                );
                let one = function(registry, "one", "outlet:2");
                assert_eq!(one.snapshot().power(), Some(Power::Unknown));
                assert_eq!(
                    one.execute(Command::Toggle).await,
                    Err(crate::device::CommandError::UnknownState)
                );
                assert_eq!(
                    one.execute(Command::On).await.unwrap().power(),
                    Some(Power::On)
                );
                until(|| {
                    ["one", "multi", "two"].iter().all(|did| {
                        let live = control.context.live.borrow();
                        let work = live.devices[*did].admitted.as_ref().unwrap();
                        work.observer().is_some()
                    })
                })
                .await;
                peer.notify("two", 2, true);
                peer.notify("multi", 4, true);
                until(|| {
                    function(registry, "two", "outlet:2").snapshot().power() == Some(Power::On)
                        && function(registry, "multi", "outlet:4").snapshot().power()
                            == Some(Power::On)
                })
                .await;
                assert_eq!(
                    function(registry, "multi", "outlet:3").snapshot().power(),
                    Some(Power::Unknown)
                );
                assert_eq!(
                    function(registry, "multi", "outlet:9").snapshot().power(),
                    Some(Power::Unknown)
                );
            })
            .await;
        }),
    );
}

#[test]
fn cached_build_dropped_before_run_releases_its_context_and_closes_commands() {
    let rig = Rig::new(true, &["one"]);
    let weak = Rc::downgrade(&rig.source.context);
    let function = function(&rig.source.context.registry, "one", "outlet:2");
    drop(rig);
    assert!(weak.upgrade().is_none());
    assert_eq!(
        block_on(function.execute(Command::On)),
        Err(crate::device::CommandError::Cancelled)
    );
}

#[test]
fn corrupt_saved_credentials_or_cached_topology_fail_before_network_requests() {
    for corrupt_credentials in [false, true] {
        let rig = Rig::new(true, &["one"]);
        let database = rusqlite::Connection::open(rig.store.path()).unwrap();
        if corrupt_credentials {
            database
                .execute("UPDATE xiaomi_auth SET certificate_pem='broken'", [])
                .unwrap();
        } else {
            database
                .execute("UPDATE bridge_configuration SET json=json_set(json, '$.devices[0].native.functions', json('[]'))", [])
                .unwrap();
        }
        let result = XiaomiAdapter::build(
            rig.store.clone(),
            rig.source.cloud.clone(),
            rig.discovery.clone(),
            rig.clock.clone(),
        );
        assert!(matches!(result, Err(AdapterError::Storage(_))));
        assert_eq!(rig.cloud.lock().unwrap().homes, 0);
    }
}
