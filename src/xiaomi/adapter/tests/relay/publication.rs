use super::*;

#[test]
fn home_relay_uses_current_hub_access_before_cloud_directory_recovers() {
    use crate::matter::tests::support::PublishedRelayModel;

    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            let rig = rig();
            rig.cloud.lock().unwrap().home_status = 503;
            block_on(rig.run(|c| async move {
                c.announce();
                let control = &c;
                c.peer(panel_peer(RelayMode::Wired), |peer| async move {
                    let relay = function(&control.context.registry, "one", "relay:2");
                    until(|| {
                        control.context.diagnostics().catalog_problem.is_some()
                            && relay.snapshot().power() == Some(Power::Off)
                            && control.publication.frozen().len() == 3
                    })
                    .await;
                    assert!(!control.context.directory.borrow().devices["one"].confirmed);
                    let key = relay.function_key();
                    let endpoint = control
                        .publication
                        .current()
                        .configuration
                        .relay_channels
                        .iter()
                        .find(|channel| &channel.key == key)
                        .unwrap()
                        .endpoint;
                    let model = PublishedRelayModel::new(&control.publication, &control.store);
                    model.invoke(endpoint, 1).await.unwrap();
                    assert_eq!(peer.count("rpcReq", "one", 2), 1);
                    assert!(
                        control
                            .publication
                            .map_relay_type(key, crate::configuration::RelayType::Light)
                            .is_err()
                    );

                    peer.state.borrow_mut().omit.insert("one".into());
                    peer.list_changed(&["one"]);
                    until(|| control.context.hub.borrow().access("one").is_none()).await;
                    assert!(model.invoke(endpoint, 0).await.is_err());
                    assert_eq!(peer.count("rpcReq", "one", 2), 1);

                    {
                        let mut cloud = control.cloud.lock().unwrap();
                        cloud.home_status = 200;
                        cloud.other_home = true;
                    }
                    peer.state.borrow_mut().omit.remove("one");
                    peer.list_changed(&["one"]);
                    control.clock.advance(Duration::from_secs(31));
                    control.context.request_sync();
                    until(|| {
                        control.context.lan_scope_invalid.get()
                            && control.context.local_candidate("one", false).is_ok()
                    })
                    .await;
                    assert!(model.invoke(endpoint, 0).await.is_err());
                    assert_eq!(peer.count("rpcReq", "one", 2), 1);
                })
                .await;
            }));
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn source_to_matter_relays_keep_identity_and_reject_stale_home_commands() {
    use crate::matter::tests::support::PublishedRelayModel;
    std::thread::Builder::new().stack_size(16 * 1024 * 1024).spawn(|| block_on(rig().run_result_without_publication(|c| async move {
        c.announce();
        let peer = panel_peer(RelayMode::Wired);
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| {
                let work = control.context.directory.borrow().devices["one"].admitted.clone().unwrap();
                [2, 3, 4].iter().all(|siid| {
                    let id = format!("relay:{siid}");
                    let function = work.physical().function(&id).unwrap();
                    function.snapshot().relay_mode() == Some(RelayMode::Wired)
                        && function.snapshot().power() == Some(Power::Off)
                        && work.function_observer(&id).is_some_and(|observer| {
                            work.source_binding_ready(&observer, binding(*siid, 2))
                                && work.source_binding_ready(&observer, binding(*siid, 1))
                        })
                })
            }).await;
            let plan = control.publication.plan().unwrap().unwrap();
            control.publication.apply(control.publication.prepare(plan)).unwrap();
            let entries = control.publication.frozen();
            let mut endpoints = entries.iter().map(|entry| entry.identity.endpoint).collect::<Vec<_>>();
            endpoints.sort();
            assert_eq!(endpoints.len(), 3);
            let endpoint = |id: &str| entries.iter().find(|entry| entry.identity.relay.as_ref().unwrap().function_id == id).unwrap().identity.endpoint;
            let model = PublishedRelayModel::new(&control.publication, &control.store);
            model.check_topology(&endpoints);
            let ep2 = endpoint("relay:2");
            let ep3 = endpoint("relay:3");
            let ep4 = endpoint("relay:4");
            assert!(!model.power(ep2).await.unwrap());
            assert!(!model.power(ep3).await.unwrap());
            let original_id = model.unique_id(ep3).await;
            assert_ne!(model.unique_id(ep2).await, original_id);
            let other_label = model.label(ep2).await;
            model.write_label(ep3, "Middle relay").await.unwrap();
            assert_eq!(model.label(ep3).await, "Middle relay");
            assert_eq!(model.label(ep2).await, other_label);
            assert!(model.write_label(ep3, &"x".repeat(33)).await.is_err());
            model.invoke(ep3, 1).await.unwrap();
            assert_eq!(peer.count("rpcReq", "one", 3), 1);
            assert_eq!(peer.count("rpcReq", "one", 2), 0);
            assert_eq!(peer.count("rpcReq", "one", 4), 0);
            assert!(!model.power(ep3).await.unwrap());
            peer.notify_value("one", binding(3, 1), json!(true), false);
            until(|| function(&control.context.registry, "one", "relay:3").snapshot().power() == Some(Power::On)).await;
            assert!(model.power(ep3).await.unwrap());
            assert!(!model.power(ep4).await.unwrap());

            // The Home request is already queued behind an explicit terminal
            // request when a new mode arrives, before publication can commit.
            peer.state.borrow_mut().hold_writes = true;
            let relay = function(&control.context.registry, "one", "relay:3");
            let mut first = Box::pin(relay.execute(Command::On));
            assert!(poll_once(first.as_mut()).await.is_none());
            until(|| peer.count("rpcReq", "one", 3) == 2).await;
            let mut queued = Box::pin(model.invoke(ep3, 0));
            assert!(poll_once(queued.as_mut()).await.is_none());
            let database = rusqlite::Connection::open(control.store.path()).unwrap();
            database.execute_batch("CREATE TRIGGER fail_relay_save BEFORE UPDATE ON bridge_configuration BEGIN SELECT RAISE(ABORT, 'injected'); END;").unwrap();
            assert!(model.write_label(ep3, "Unsaved").await.is_err());
            assert_eq!(model.label(ep3).await, "Middle relay");
            peer.notify_value("one", binding(3, 2), json!(1), false);
            until(|| function(&control.context.registry, "one", "relay:3").snapshot().relay_mode() == Some(RelayMode::Wireless)).await;
            let plan = control.publication.plan().unwrap().unwrap();
            assert!(control.publication.apply(control.publication.prepare(plan)).is_err());
            assert_eq!(control.publication.frozen().len(), 3);
            peer.state.borrow_mut().hold_writes = false;
            peer.release("one", 3, accepted("one", 3, 0)).await;
            first.await.unwrap();
            assert!(queued.await.is_err());
            assert!(model.invoke(ep3, 0).await.is_err());
            assert_eq!(peer.count("rpcReq", "one", 3), 2);
            database.execute_batch("DROP TRIGGER fail_relay_save").unwrap();
            let plan = control.publication.plan().unwrap().unwrap();
            control.publication.apply(control.publication.prepare(plan)).unwrap();
            assert_eq!(control.publication.frozen().len(), 2);
            assert!(model.invoke(ep3, 0).await.is_err());
            let terminal = control.terminal();
            assert!(terminal.handle_line("off one relay:3").await.unwrap().unwrap().contains("command accepted"));
            assert_eq!(peer.count("rpcReq", "one", 3), 3);

            for siid in [2, 4] { peer.notify_value("one", binding(siid, 2), json!(1), false); }
            until(|| [2, 3, 4].iter().all(|siid|
                function(&control.context.registry, "one", &format!("relay:{siid}"))
                    .snapshot().relay_mode() == Some(RelayMode::Wireless))).await;
            let plan = control.publication.plan().unwrap().unwrap();
            control.publication.apply(control.publication.prepare(plan)).unwrap();
            assert!(control.publication.frozen().is_empty());
            for siid in [2, 3, 4] { peer.notify_value("one", binding(siid, 2), json!(0), false); }
            until(|| [2, 3, 4].iter().all(|siid|
                function(&control.context.registry, "one", &format!("relay:{siid}"))
                    .snapshot().relay_mode() == Some(RelayMode::Wired))).await;
            let plan = control.publication.plan().unwrap().unwrap();
            control.publication.apply(control.publication.prepare(plan)).unwrap();
            let restored = control.publication.frozen();
            assert_eq!(restored.len(), 3);
            assert!(restored.iter().any(|entry| entry.identity.endpoint == ep3));
            let model = PublishedRelayModel::new(&control.publication, &control.store);
            assert_eq!(model.label(ep3).await, "Middle relay");
            assert_eq!(model.unique_id(ep3).await, original_id);
            assert_eq!(peer.count("rpcReq", "one", 2), 0);
            assert_eq!(peer.count("rpcReq", "one", 3), 3);
            assert_eq!(peer.count("rpcReq", "one", 4), 0);
            assert_eq!(control.store.devices().load_configuration().unwrap(), *control.publication.current().configuration);
        }).await;
    })).unwrap()).unwrap().join().unwrap();
}

#[test]
fn terminal_maps_a_relay_to_light_and_persists_its_identity() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            block_on(rig().run_result_without_publication(|c| async move {
                c.announce();
                let peer = panel_peer(RelayMode::Wired);
                let control = &c;
                c.peer(peer, |peer| async move {
                    until(|| {
                        [2, 3, 4].iter().all(|siid| {
                            function(&control.context.registry, "one", &format!("relay:{siid}"))
                                .snapshot()
                                .relay_mode()
                                == Some(RelayMode::Wired)
                        })
                    })
                    .await;
                    let plan = control.publication.plan().unwrap().unwrap();
                    control
                        .publication
                        .apply(control.publication.prepare(plan))
                        .unwrap();
                    let before = control.publication.current().configuration.clone();
                    let terminal = control.terminal();
                    let result = terminal
                        .handle_line("map-type one relay:3 light")
                        .await
                        .unwrap()
                        .unwrap();
                    assert!(result.contains("mapped to light"), "{result}");
                    let after = control.publication.current().configuration.clone();
                    let find = |config: &crate::configuration::CommittedConfiguration| {
                        config
                            .relay_channels
                            .iter()
                            .find(|channel| channel.key.id == "relay:3")
                            .unwrap()
                            .clone()
                    };
                    let original = find(&before);
                    let mapped = find(&after);
                    assert_eq!(mapped.device_type, crate::configuration::RelayType::Light);
                    assert_eq!(mapped.endpoint, original.endpoint);
                    assert_eq!(mapped.unique_id, original.unique_id);
                    assert_eq!(mapped.label, original.label);
                    assert_eq!(
                        mapped.configuration_version,
                        original.configuration_version + 1
                    );
                    assert_eq!(after.root_version, before.root_version + 1);
                    assert_eq!(
                        control.store.devices().load_configuration().unwrap(),
                        *after
                    );
                    assert!(control.publication.plan().unwrap().unwrap().rebuild);
                    let list = terminal.handle_line("list").await.unwrap().unwrap();
                    assert!(list.contains("mode wired; mapping light"), "{list}");
                    let details = terminal.handle_line("status one").await.unwrap().unwrap();
                    assert!(details.contains("on 3.1 (read/write/notify)"), "{details}");
                    assert!(details.contains("mode 3.2 (read/notify)"), "{details}");
                    assert!(
                        details.contains(&format!("unique ID {}", original.unique_id)),
                        "{details}"
                    );
                    let plan = control.publication.plan().unwrap().unwrap();
                    control
                        .publication
                        .apply(control.publication.prepare(plan))
                        .unwrap();
                    let model = crate::matter::tests::support::PublishedRelayModel::new(
                        &control.publication,
                        &control.store,
                    );
                    let other = after
                        .relay_channels
                        .iter()
                        .find(|channel| channel.key.id == "relay:2")
                        .unwrap();
                    model.check_relay_type(other.endpoint, false);
                    model.check_relay_type(mapped.endpoint, true);
                    assert_eq!(model.device_types(other.endpoint).await, [0x0103, 0x0013]);
                    assert_eq!(model.device_types(mapped.endpoint).await, [0x0100, 0x0013]);
                    assert_eq!(
                        model.configuration_version(mapped.endpoint).await,
                        original.configuration_version + 1
                    );
                    assert_eq!(model.unique_id(mapped.endpoint).await, original.unique_id);
                    assert_eq!(model.label(mapped.endpoint).await, original.label);
                    model.invoke(other.endpoint, 1).await.unwrap();
                    model.invoke(mapped.endpoint, 1).await.unwrap();
                    assert_eq!(peer.count("rpcReq", "one", 2), 1);
                    assert_eq!(peer.count("rpcReq", "one", 3), 1);
                    assert_eq!(peer.count("rpcReq", "one", 4), 0);
                    peer.notify_value("one", binding(3, 1), json!(true), false);
                    until(|| {
                        function(&control.context.registry, "one", "relay:3")
                            .snapshot()
                            .power()
                            == Some(Power::On)
                    })
                    .await;
                    assert!(model.power(mapped.endpoint).await.unwrap());
                    assert!(!model.power(other.endpoint).await.unwrap());
                    let reopened = Store::open(control.store.path().parent().unwrap()).unwrap();
                    let source =
                        crate::xiaomi::adapter::tests::app_source::AppSource::new(&reopened, None);
                    source.confirm_all();
                    let restored = Rc::new(
                        crate::app::App::open(source.handle.clone(), reopened.clone()).unwrap(),
                    );
                    let restored_model = crate::matter::tests::support::PublishedRelayModel::new(
                        &restored, &reopened,
                    );
                    restored_model.check_relay_type(mapped.endpoint, true);
                    assert_eq!(
                        restored_model.unique_id(mapped.endpoint).await,
                        original.unique_id
                    );
                    assert!(
                        terminal
                            .handle_line("map-type one relay:3 switch")
                            .await
                            .unwrap()
                            .unwrap()
                            .contains("mapped to switch")
                    );
                    let plan = control.publication.plan().unwrap().unwrap();
                    control
                        .publication
                        .apply(control.publication.prepare(plan))
                        .unwrap();
                    let switched_back = crate::matter::tests::support::PublishedRelayModel::new(
                        &control.publication,
                        &control.store,
                    );
                    switched_back.check_relay_type(mapped.endpoint, false);
                    assert_eq!(
                        switched_back.unique_id(mapped.endpoint).await,
                        original.unique_id
                    );
                    assert_eq!(peer.count("rpcReq", "one", 3), 1);
                })
                .await;
            }))
            .unwrap();
        })
        .unwrap()
        .join()
        .unwrap();
}
