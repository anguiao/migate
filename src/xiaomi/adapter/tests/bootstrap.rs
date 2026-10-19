use super::*;
use crate::storage::tests::fixtures::{commit, record};

#[test]
fn restored_topology_is_the_selected_scope_and_published_intersection() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let tls = TlsFixture::new(false);
    store.xiaomi().replace(&tls.credentials).unwrap();
    let kept = cached(&tls.credentials.uid, "kept", true);
    let deleted = cached(&tls.credentials.uid, "deleted", false);
    let mut other = cached(&tls.credentials.uid, "other-home", false);
    other.home_id = "other".into();
    other.group_id = group_id(&other.uid, &other.home_id);
    let foreign = cached("foreign", "foreign", false);
    let mut deleted_record = record(&deleted.physical_description("deleted"), 6);
    deleted_record.published = false;
    let records = vec![
        record(&kept.physical_description("saved name"), 2),
        deleted_record,
        record(&other.physical_description("other"), 7),
        record(&foreign.physical_description("foreign"), 8),
    ];
    for cached in [other, foreign] {
        let scope = CachedScope {
            uid: cached.uid.clone(),
            home_id: cached.home_id.clone(),
            group_id: cached.group_id.clone(),
        };
        store
            .devices()
            .apply_publication(&commit(scope, vec![cached], records.clone()))
            .unwrap();
    }
    store
        .devices()
        .apply_publication(&commit(
            scope(&tls.credentials.uid),
            vec![kept, deleted],
            records,
        ))
        .unwrap();
    let (base, requests) = dynamic_mock_server(1, |_| MockResponse::json(503, "{}"));
    let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    let (_events, receiver) = flume::unbounded();
    let (discovery, _driver) = Discovery::with_events(receiver);
    let (source, ready) = XiaomiAdapter::new(store, cloud, discovery).unwrap();
    let registry = block_on(poll_once(ready.wait()))
        .expect("cache readiness is synchronous")
        .unwrap();
    assert_eq!(registry.list().len(), 1);
    let physical = registry.get(&DeviceKey::new("xiaomi", "kept")).unwrap();
    assert_eq!(physical.functions().len(), 3);
    assert_eq!(physical.description().label(), "saved name");
    for function in physical.functions() {
        assert_eq!(function.snapshot().power, Power::Unknown);
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
        let registry = c.ready.take().unwrap().wait().await.unwrap();
        assert!(registry.list().is_empty());
    }));
}

#[test]
fn cached_sources_are_not_classified_as_groups_by_names_or_models() {
    let rig = Rig::light("xiaomi.light.ceil04");
    let scope = scope(&rig.data[0].uid);
    let mut state = rig.store.devices().load_publication(Some(&scope)).unwrap();
    let single = state.records[0].clone();
    let mut groups = Vec::new();
    for (endpoint, did, model) in [
        (3, "group.42", "xiaomi.light.ceil04"),
        (4, "43", "mijia.light.group3"),
    ] {
        let mut group = rig.data[0].clone();
        group.device = super::support::device_with_key(&group.device, did);
        group.model = model.into();
        state
            .records
            .push(record(&group.physical_description("Saved group"), endpoint));
        state.cached.push(group.clone());
        groups.push(group);
    }
    rig.store
        .devices()
        .apply_publication(&commit(scope, state.cached, state.records))
        .unwrap();
    for _ in 0..2 {
        let (source, ready) = XiaomiAdapter::build(
            rig.store.clone(),
            rig.source.cloud.clone(),
            rig.discovery.clone(),
            rig.clock.clone(),
        )
        .unwrap();
        let registry = block_on(poll_once(ready.wait())).unwrap().unwrap();
        assert_eq!(registry.list().len(), 3);
        assert!(registry.get(&single.key).is_some());
        assert_eq!(
            rig.store
                .devices()
                .load_publication(None)
                .unwrap()
                .records
                .iter()
                .filter(|record| record.published)
                .count(),
            3
        );
        for group in &groups {
            assert!(registry.get(group.device.key()).is_some());
            assert!(
                source.context.live.borrow().devices[&group.device.key().id]
                    .group
                    .is_none()
            );
            assert_eq!(
                rig.store
                    .devices()
                    .load_parsed(&group.uid, &group.home_id, group.device.key())
                    .unwrap(),
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
                let one = function(registry, "one", "main");
                assert_eq!(one.snapshot().power, Power::Unknown);
                assert_eq!(
                    one.execute(Command::Toggle).await,
                    Err(crate::device::CommandError::UnknownState)
                );
                assert_eq!(one.execute(Command::On).await.unwrap().power, Power::On);
                until(|| {
                    ["one", "multi", "two"].iter().all(|did| {
                        let live = control.context.live.borrow();
                        let work = live.devices[*did].work.as_ref().unwrap();
                        work.active.borrow().is_some()
                    })
                })
                .await;
                peer.notify("two", 2, true);
                peer.notify("multi", 4, true);
                until(|| {
                    function(registry, "two", "main").snapshot().power == Power::On
                        && function(registry, "multi", "outlet_2").snapshot().power == Power::On
                })
                .await;
                assert_eq!(
                    function(registry, "multi", "outlet_1").snapshot().power,
                    Power::Unknown
                );
                assert_eq!(
                    function(registry, "multi", "usb").snapshot().power,
                    Power::Unknown
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
    let function = function(&rig.source.context.registry, "one", "main");
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
                .execute("UPDATE device_cache SET functions='broken'", [])
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
