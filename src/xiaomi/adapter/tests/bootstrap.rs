use super::*;

#[test]
fn restored_topology_is_the_selected_scope() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let tls = TlsFixture::new(false);
    store.xiaomi().replace(&tls.credentials).unwrap();
    store
        .devices()
        .save_scope(&scope(&tls.credentials.uid))
        .unwrap();
    let kept = cached(&tls.credentials.uid, "kept", true);
    store.devices().publish_cached(&kept, "saved name").unwrap();
    let mut other = cached(&tls.credentials.uid, "other-home", false);
    other.home_id = "other".into();
    other.group_id = group_id(&other.uid, &other.home_id);
    store.devices().publish_cached(&other, "other").unwrap();
    store
        .devices()
        .publish_cached(&cached("foreign", "foreign", false), "foreign")
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
    assert_eq!(physical.description().label, "saved name");
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

#[test]
fn unmappable_light_restores_a_reservation_without_controls() {
    let rig = Rig::light("synthetic.light.rgbonly");
    let key = DeviceKey::new("xiaomi", "one");
    let stored = rig.store.devices().load_published(&key).unwrap().unwrap();
    block_on(rig.run(|c| async move {
        c.announce();
        until(|| {
            c.context
                .status
                .borrow()
                .pending
                .iter()
                .any(|pending| pending.did == "one" && pending.reason.contains("on/off"))
        })
        .await;
        assert!(c.context.registry.get(&key).is_none());
        assert!(c.context.live.borrow().devices["one"].unpublished.is_some());
        assert_eq!(
            c.store.devices().load_published(&key).unwrap().unwrap(),
            stored
        );
    }));
}

#[test]
fn committed_standalone_configuration_restores_its_applied_set_at_startup() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let tls = TlsFixture::new(false);
    store.xiaomi().replace(&tls.credentials).unwrap();
    store
        .devices()
        .save_scope(&scope(&tls.credentials.uid))
        .unwrap();
    let kept = cached(&tls.credentials.uid, "kept", true);
    let stored = store.devices().publish_cached(&kept, "saved name").unwrap();
    let registry = DeviceRegistry::new();
    let (device, _receivers) = crate::device::PhysicalDevice::new(stored.description());
    registry.insert(device);
    let scope = scope(&tls.credentials.uid);
    registry
        .stage_configuration(
            crate::device::ConfigurationScope {
                uid: scope.uid,
                home_id: scope.home_id,
                group_id: scope.group_id,
            },
            Vec::new(),
            Vec::new(),
        )
        .unwrap();
    store
        .devices()
        .apply_pending_configuration(&registry, |_| Ok((Vec::new(), Vec::new())))
        .unwrap();
    let (base, _requests) = dynamic_mock_server(1, |_| MockResponse::json(503, "{}"));
    let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    let (_events, receiver) = flume::unbounded();
    let (discovery, _driver) = Discovery::with_events(receiver);
    let (_source, ready) = XiaomiAdapter::new(store, cloud, discovery).unwrap();
    let registry = block_on(poll_once(ready.wait()))
        .expect("cache readiness is synchronous")
        .unwrap();
    let snapshot = registry.snapshot();
    assert_eq!(snapshot.applied.confirmed, Some(1));
    assert_eq!(
        snapshot
            .applied
            .devices
            .iter()
            .map(|device| device.key().clone())
            .collect::<Vec<_>>(),
        vec![DeviceKey::new("xiaomi", "kept")]
    );
    assert!(snapshot.applied.groups.is_empty());
}

#[test]
fn committed_empty_configuration_restores_its_version_at_startup() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let tls = TlsFixture::new(false);
    store.xiaomi().replace(&tls.credentials).unwrap();
    store
        .devices()
        .save_scope(&scope(&tls.credentials.uid))
        .unwrap();
    rusqlite::Connection::open(store.path())
        .unwrap()
        .execute(
            "INSERT INTO group_snapshot VALUES(?1,?2,3)",
            rusqlite::params![tls.credentials.uid, "local"],
        )
        .unwrap();
    let (base, _requests) = dynamic_mock_server(1, |_| MockResponse::json(503, "{}"));
    let cloud = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
    let (_events, receiver) = flume::unbounded();
    let (discovery, _driver) = Discovery::with_events(receiver);
    let (_source, ready) = XiaomiAdapter::new(store, cloud, discovery).unwrap();
    let registry = block_on(poll_once(ready.wait()))
        .expect("cache readiness is synchronous")
        .unwrap();
    let snapshot = registry.snapshot();
    assert_eq!(snapshot.applied.confirmed, Some(3));
    assert!(snapshot.applied.groups.is_empty());
    assert!(snapshot.applied.devices.is_empty());
}
