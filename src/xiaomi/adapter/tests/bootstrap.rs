use super::*;
use crate::storage::tests::fixtures::identity;

#[test]
fn equivalent_startup_recomputation_keeps_versions_and_uses_no_network() {
    let rig = Rig::new(true, &["one"]);
    let before = rig.store.devices().load_configuration().unwrap();
    let app = crate::app::App::open(rig.source.source_handle(), rig.store.clone());
    assert!(app.plan().unwrap().is_none());
    assert_eq!(*app.current().configuration, before);
    assert_eq!(rig.store.devices().load_configuration().unwrap(), before);
    assert!(rig.cloud.lock().unwrap().instance_reads.is_empty());
}

#[test]
fn previously_unsupported_cached_source_can_gain_its_first_function_on_startup() {
    let rig = Rig::new(true, &["one", "two"]);
    let mut previous = rig.store.devices().load_configuration().unwrap();
    let neighbor = previous
        .device(&DeviceKey::new("xiaomi", "two"))
        .unwrap()
        .clone();
    previous.devices.retain(|device| device.key == neighbor.key);
    let fabric = rs_matter::persist::FABRIC_KEYS_START + 1;
    rig.store.matter().put(fabric, b"fabric").unwrap();
    previous.descriptions[0].name = Some("Cached outlet name".into());
    previous.descriptions[0].diagnostic =
        Some("previous rules did not recognize this source".into());
    rig.store
        .devices()
        .commit_configuration(&previous, &[])
        .unwrap();
    let (restored, _) = rig.reopen_adapter();
    let app = crate::app::App::open(restored.source_handle(), rig.store.clone());
    assert_eq!(app.matter_configuration().entries.len(), 1);
    let plan = app.plan().unwrap().unwrap();
    app.apply(app.prepare(plan)).unwrap();
    assert_eq!(app.matter_configuration().entries.len(), 2);
    let saved = rig.store.devices().load_configuration().unwrap();
    assert_eq!(saved.device(&neighbor.key), Some(&neighbor));
    assert_eq!(
        rig.store.matter().get(fabric).unwrap().as_deref(),
        Some(b"fabric".as_slice())
    );
    assert_eq!(
        saved.descriptions[0].instance,
        previous.descriptions[0].instance
    );
    assert!(saved.descriptions[0].diagnostic.is_none());
    assert!(saved.devices[0].native.is_some());
    assert_eq!(saved.devices[0].label, "Cached outlet name");
    assert_eq!(saved.descriptions[0].name, previous.descriptions[0].name);
    assert!(rig.cloud.lock().unwrap().instance_reads.is_empty());
    assert!(app.plan().unwrap().is_none());
}

const RECOGNITION_FAILURE: &str = "cached recognition failed under updated rules";

fn failed_recognition(context: &super::super::state::Context, did: &str) {
    // Inject the state of a parser regression after restoring valid stored facts.
    // The original input and its committed result remain a complete pair.
    {
        let mut live = context.directory.borrow_mut();
        let slot = live.devices.get_mut(did).unwrap();
        let raw = &slot.latest_description.as_ref().unwrap().raw;
        let target = Some((raw.model.clone(), raw.instance_type.clone()));
        slot.description_failed(super::super::directory::DescriptionProblem {
            operation: super::super::directory::DescriptionOperation::RecognizeStored,
            target,
            reason: RECOGNITION_FAILURE.into(),
        });
    }
    context.status_changed.notify(usize::MAX);
}

#[test]
fn failed_recognition_retains_one_source_without_blocking_updates_or_removal() {
    let rig = Rig::new(true, &["one", "two"]);
    let context = &rig.source.context;
    let app = crate::app::App::open(rig.source.source_handle(), rig.store.clone());
    let before = app.current().configuration.clone();
    let retained = context.directory.borrow().devices["one"]
        .latest_description
        .clone()
        .unwrap();
    let work = context.directory.borrow().devices["one"]
        .work
        .clone()
        .unwrap();
    failed_recognition(context, "one");
    context
        .directory
        .borrow_mut()
        .devices
        .get_mut("one")
        .unwrap()
        .cloud_access = Some(crate::xiaomi::mijia::CloudDeviceInfo {
        did: "one".into(),
        model: retained.raw.model.clone(),
        instance_type: retained.raw.instance_type.clone(),
        online: false,
        lan: crate::xiaomi::mijia::LanCredentials::NotIp,
    });
    context.pending_description("one", "new description read failed".into());
    let pending = context.diagnostics().pending;
    let problem = &pending
        .iter()
        .find(|pending| pending.did == "one")
        .unwrap()
        .reason;
    assert!(
        problem.contains(RECOGNITION_FAILURE) && problem.contains("new description read failed"),
        "{problem}"
    );
    let mut changed = SourceFixture::committed(&before, "two");
    let mut extra = changed.raw.instance["services"][1].clone();
    extra["iid"] = json!(42);
    changed.raw.instance["services"]
        .as_array_mut()
        .unwrap()
        .push(extra);
    let candidate = changed.raw.recognize(false).unwrap();
    context
        .directory
        .borrow_mut()
        .devices
        .get_mut("two")
        .unwrap()
        .latest_description = Some(candidate);

    let plan = app.plan().unwrap().unwrap();
    assert_eq!(
        plan.configuration.device(&retained.raw.key),
        before.device(&retained.raw.key)
    );
    assert_eq!(
        plan.configuration
            .device(&DeviceKey::new("xiaomi", "two"))
            .unwrap()
            .functions
            .len(),
        2
    );
    app.apply(app.prepare(plan)).unwrap();
    let after = rig.store.devices().load_configuration().unwrap();
    assert_eq!(
        after.device(&retained.raw.key),
        before.device(&retained.raw.key)
    );
    assert_eq!(
        after
            .descriptions
            .iter()
            .find(|raw| raw.key == retained.raw.key),
        Some(&retained.raw)
    );
    assert!(Rc::ptr_eq(
        &work,
        context.directory.borrow().devices["one"]
            .work
            .as_ref()
            .unwrap()
    ));

    context.resolved_description_target("one");
    let pending = context.diagnostics().pending;
    let problem = &pending
        .iter()
        .find(|pending| pending.did == "one")
        .unwrap()
        .reason;
    assert!(
        problem.contains(RECOGNITION_FAILURE) && !problem.contains("new description read failed"),
        "{problem}"
    );
    assert!(app.plan().unwrap().is_none());
    assert!(!app.pending());
    assert!(app.failure().is_none());
    assert_eq!(
        context.directory.borrow().devices["one"]
            .latest_description
            .as_ref(),
        Some(&retained)
    );
    assert!(rig.cloud.lock().unwrap().instance_reads.is_empty());

    context.remove("one").unwrap();
    let plan = app.plan().unwrap().unwrap();
    app.apply(app.prepare(plan)).unwrap();
    let removed = rig.store.devices().load_configuration().unwrap();
    assert!(removed.device(&retained.raw.key).unwrap().native.is_none());
    assert!(
        !removed
            .descriptions
            .iter()
            .any(|raw| raw.key == retained.raw.key)
    );
    assert!(
        !context
            .diagnostics()
            .pending
            .iter()
            .any(|pending| pending.did == "one")
    );
}

#[test]
fn failed_group_recognition_keeps_its_source_while_member_composition_updates() {
    let rig = repair_rig(0, true);
    block_on(rig.run(|c| async move {
        repair_ready(&c).await;
        let before = c.store.devices().load_configuration().unwrap();
        let key = DeviceKey::new("xiaomi", "group-1");
        let retained = c.context.directory.borrow().devices["group-1"]
            .latest_description
            .clone()
            .unwrap();
        failed_recognition(&c.context, "group-1");
        let mut instance = ls2307_instance();
        instance["services"][1]["properties"][2]["value-range"] = json!([3000, 6000, 1]);
        let candidate = SourceFixture::new("member-b", "devcea.light.ls2307", instance);
        c.context
            .directory
            .borrow_mut()
            .devices
            .get_mut("member-b")
            .unwrap()
            .latest_description = Some(candidate.into());

        let plan = c.publication.plan().unwrap().unwrap();
        assert_eq!(plan.configuration.groups, before.groups);
        assert_eq!(
            plan.configuration.device(&key).unwrap().native,
            before.device(&key).unwrap().native
        );
        assert_eq!(
            plan.derived.served[&key].description.functions()[0].temperature_interval(),
            Some(crate::device::TemperatureInterval::new(3000, 6000).unwrap())
        );
        c.publication.apply(c.publication.prepare(plan)).unwrap();
        assert_eq!(
            c.context.directory.borrow().devices["group-1"]
                .latest_description
                .as_ref(),
            Some(&retained)
        );
        assert_eq!(
            function(&c.context, "group-1", "light:2")
                .description()
                .temperature_interval(),
            Some(crate::device::TemperatureInterval::new(3000, 6000).unwrap())
        );
        assert!(c.context.diagnostics().pending.iter().any(
            |pending| pending.did == "group-1" && pending.reason.contains(RECOGNITION_FAILURE)
        ));
        assert!(c.publication.plan().unwrap().is_none());
    }));
}

#[test]
fn a_new_description_resolves_the_retained_recognition_failure() {
    let rig = Rig::new(true, &["one"]);
    failed_recognition(&rig.source.context, "one");
    block_on(rig.run(|c| async move {
        until(|| {
            c.context.directory.borrow().devices["one"]
                .cloud_access
                .is_some()
        })
        .await;
        assert!(
            c.context.directory.borrow().devices["one"]
                .description_problems
                .iter()
                .any(|problem| {
                    problem.operation
                        == super::super::directory::DescriptionOperation::RecognizeStored
                })
        );
        assert!(
            c.context.directory.borrow().devices["one"]
                .description_problem()
                .unwrap()
                .contains(RECOGNITION_FAILURE)
        );
        assert!(c.cloud.lock().unwrap().instance_reads.is_empty());
        let committed = c.store.devices().load_configuration().unwrap();
        let mut instance = committed.descriptions[0].instance.clone();
        let urn = "urn:miot-spec-v2:device:outlet:0000A002:cuco-v3:3";
        instance["type"] = json!(urn);
        {
            let mut cloud = c.cloud.lock().unwrap();
            cloud.instance_types.insert("one".into(), urn.into());
            cloud.catalog_instances.insert(urn.into(), instance);
        }
        c.context.request_sync();
        until(|| {
            let live = c.context.directory.borrow();
            let slot = &live.devices["one"];
            slot.description_problems.is_empty()
                && slot
                    .latest_description
                    .as_ref()
                    .is_some_and(|description| description.raw.instance_type == urn)
        })
        .await;
        assert!(
            !c.context
                .diagnostics()
                .pending
                .iter()
                .any(|pending| pending.reason.contains(RECOGNITION_FAILURE))
        );
        let current = c.store.devices().load_configuration().unwrap();
        assert_eq!(current.devices[0].unique_id, committed.devices[0].unique_id);
        assert_eq!(current.devices[0].functions, committed.devices[0].functions);
        assert_eq!(c.cloud.lock().unwrap().instance_reads[urn], 1);
    }));
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
    let (restored, _) = rig.reopen_adapter();
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
    let old_work = restored.context.directory.borrow().devices["sensor"]
        .work
        .clone()
        .unwrap();
    let app = crate::app::App::open(restored.source_handle(), rig.store.clone());
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
        .native = Some(sensor_native_without_battery("sensor").native);
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
    let (restored, _) = rig.reopen_adapter();
    let app = crate::app::App::open(restored.source_handle(), rig.store.clone());
    assert_eq!(app.matter_configuration().entries.len(), 1);
    assert!(
        !app.matter_configuration().entries[0]
            .device
            .description()
            .has_battery()
    );
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
    assert_eq!(*app.current().configuration, before);
    assert_eq!(app.matter_configuration().entries.len(), 1);
    assert!(
        !app.matter_configuration().entries[0]
            .device
            .description()
            .has_battery()
    );
    assert!(app.failure().is_some());
    assert!(rig.cloud.lock().unwrap().instance_reads.is_empty());
    db.execute_batch("DROP TRIGGER fail_startup").unwrap();
    let prepared = app.prepare(app.plan().unwrap().unwrap());
    app.apply(prepared).unwrap();
    assert!(
        app.matter_configuration().entries[0]
            .device
            .description()
            .has_battery()
    );
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
    store.xiaomi().fixture_replace(&tls.credentials).unwrap();
    let kept = cached(&tls.credentials.uid, "kept", true);
    let deleted = cached(&tls.credentials.uid, "deleted", false);
    let other = cached(&tls.credentials.uid, "other-home", false);
    let foreign = cached("foreign", "foreign", false);
    let mut kept_record = identity(&kept.physical_description("saved name"), 2);
    kept_record.native = Some(kept.native);
    let configuration = crate::configuration::CommittedConfiguration {
        temperature_bindings: Default::default(),
        relay_channels: Vec::new(),
        split_devices: Vec::new(),
        scope: Some(scope(&tls.credentials.uid)),
        root_version: 1,
        descriptions: vec![kept.raw],
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
    let context = &source.context;
    assert_eq!(context.physical_devices().len(), 1);
    let physical = context.physical(&DeviceKey::new("xiaomi", "kept")).unwrap();
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
            .push(SourceFixture::new(did, model, state.descriptions[0].instance.clone()).raw);
        state.devices.push(device);
        groups.push(group);
    }
    rig.store
        .devices()
        .commit_configuration(&state, &[])
        .unwrap();
    for _ in 0..2 {
        let (source, ready) = rig.reopen_adapter();
        block_on(poll_once(ready.wait())).unwrap().unwrap();
        let context = &source.context;
        assert_eq!(context.physical_devices().len(), 3);
        assert!(context.physical(&single.key).is_some());
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
            assert!(context.physical(group.device().key()).is_some());
            assert!(
                source
                    .context
                    .directory
                    .borrow()
                    .group(&group.device().key().id)
                    .is_none()
            );
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
                    control.context.physical_devices().len() == 3
                        && control
                            .context
                            .physical_devices()
                            .iter()
                            .all(|physical| physical.availability() == Availability::Available)
                })
                .await;
                let context = &control.context;
                assert_eq!(
                    context
                        .physical(&DeviceKey::new("xiaomi", "multi"))
                        .unwrap()
                        .functions()
                        .len(),
                    3
                );
                let one = function(context, "one", "outlet:2");
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
                        let live = control.context.directory.borrow();
                        let work = live.devices[*did].work.as_ref().unwrap();
                        work.observer().is_some()
                    })
                })
                .await;
                peer.notify("two", 2, true);
                peer.notify("multi", 4, true);
                until(|| {
                    function(context, "two", "outlet:2").snapshot().power() == Some(Power::On)
                        && function(context, "multi", "outlet:4").snapshot().power()
                            == Some(Power::On)
                })
                .await;
                assert_eq!(
                    function(context, "multi", "outlet:3").snapshot().power(),
                    Some(Power::Unknown)
                );
                assert_eq!(
                    function(context, "multi", "outlet:9").snapshot().power(),
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
    let function = function(&rig.source.context, "one", "outlet:2");
    drop(rig);
    assert!(weak.upgrade().is_none());
    assert_eq!(
        block_on(function.execute(Command::On)),
        Err(crate::device::CommandError::Cancelled)
    );
}

#[test]
fn invalid_certificate_keeps_account_and_topology_while_corrupt_topology_fails() {
    for corrupt_credentials in [false, true] {
        let Rig {
            directory,
            source,
            store,
            discovery,
            clock,
            cloud,
            uid,
            ..
        } = Rig::new(true, &["one"]);
        let client = source.cloud.clone();
        drop(source);
        drop(store);
        let database = rusqlite::Connection::open(directory.path().join("state.db")).unwrap();
        if corrupt_credentials {
            database
                .execute("UPDATE xiaomi_hub SET certificate_pem='broken'", [])
                .unwrap();
        } else {
            database
                .execute("UPDATE bridge_configuration SET json=json_set(json, '$.devices[0].native.functions', json('[]'))", [])
                .unwrap();
        }
        drop(database);
        let store = crate::storage::Store::open(directory.path()).unwrap();
        let result = TestNetwork::new(None).build(store, client, discovery, clock);
        if corrupt_credentials {
            let (adapter, ready) = result.unwrap();
            block_on(poll_once(ready.wait())).unwrap().unwrap();
            assert_eq!(adapter.context.diagnostics().auth.certificate, None);
            assert!(matches!(
                adapter.context.diagnostics().auth.certificate_update,
                crate::xiaomi::auth::CertificateUpdate::Failed(
                    crate::xiaomi::auth::FailureReason::InvalidCertificate
                )
            ));
            assert_eq!(
                adapter.context.credentials.borrow().record().unwrap().uid,
                uid
            );
            assert!(adapter.context.directory.borrow().scope.is_some());
            assert!(
                adapter
                    .context
                    .directory
                    .borrow()
                    .devices
                    .contains_key("one")
            );
        } else {
            assert!(matches!(result, Err(AdapterError::Storage(_))));
        }
        assert_eq!(cloud.lock().unwrap().homes, 0);
    }
}

#[test]
fn stopped_ha_or_missing_certificate_preserves_topology_and_maintenance() {
    for ha_stopped in [true, false] {
        let rig = Rig::new(true, &["one"]);
        let mut saved = rig.store.xiaomi().load().unwrap().unwrap();
        if ha_stopped {
            saved.ha.stopped = true;
        } else {
            saved.hub.as_mut().unwrap().certificate_pem = None;
        }
        rig.store.xiaomi().fixture_replace(&saved).unwrap();
        let (adapter, ready) = rig.reopen_adapter();
        block_on(poll_once(ready.wait())).unwrap().unwrap();
        assert!(adapter.context.directory.borrow().scope.is_some());
        assert!(
            adapter
                .context
                .directory
                .borrow()
                .devices
                .contains_key("one")
        );
        let report = adapter.context.diagnostics().auth.clone();
        assert_eq!(report.certificate.is_some(), ha_stopped);
        if ha_stopped {
            assert!(matches!(
                report.ha,
                crate::xiaomi::auth::AuthenticationState::SignInRequired(
                    crate::xiaomi::auth::FailureReason::HaAuthorizationStopped
                )
            ));
        }
        let store = rig.store.clone();
        let result = block_on(future::or(
            async { Some(adapter.run(|_| {}).await) },
            async {
                if ha_stopped {
                    async_io::Timer::after(Duration::from_millis(30)).await;
                } else {
                    future::or(
                        until(|| {
                            store
                                .xiaomi()
                                .load()
                                .unwrap()
                                .unwrap()
                                .hub
                                .unwrap()
                                .certificate_pem
                                .is_some()
                        }),
                        async {
                            async_io::Timer::after(Duration::from_secs(2)).await;
                        },
                    )
                    .await;
                }
                None
            },
        ));
        assert!(
            result.is_none(),
            "adapter exited during independent credential maintenance: {result:?}"
        );
        let current = rig.store.xiaomi().load().unwrap().unwrap();
        if ha_stopped {
            assert_eq!(current, saved);
            assert_eq!(rig.cloud.lock().unwrap().certs, 0);
            assert_eq!(rig.cloud.lock().unwrap().tokens, 0);
            assert!(rig.cloud.lock().unwrap().homes > 0);
        } else {
            let original = saved.hub.unwrap();
            let hub = current.hub.unwrap();
            assert_eq!(hub.virtual_did, original.virtual_did);
            assert_eq!(hub.private_key_pem, original.private_key_pem);
            assert!(hub.certificate_pem.is_some());
            assert_eq!(rig.cloud.lock().unwrap().certs, 1);
        }
    }
}
