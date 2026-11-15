use super::*;
use crate::{device::RelayMode, xiaomi::capability::PropertyBinding};

fn binding(siid: u32, piid: u32) -> PropertyBinding {
    PropertyBinding { siid, piid }
}

fn panel_peer(mode: RelayMode) -> Rc<Peer> {
    let peer = Rc::new(Peer::default());
    let mode_value = match mode {
        RelayMode::Wired => 0,
        RelayMode::Wireless => 1,
    };
    for siid in [2, 3, 4] {
        peer.value("one", binding(siid, 1), json!(false));
        peer.value("one", binding(siid, 2), json!(mode_value));
    }
    peer
}

fn rig() -> Rig {
    Rig::profile_light(
        "zimi.switch.dhkg05",
        serde_json::from_str(include_str!(
            "../../../../tests/fixtures/miot/zimi-dhkg05.json"
        ))
        .unwrap(),
    )
}

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
                        control.context.status.borrow().catalog_problem.is_some()
                            && relay.snapshot().power() == Some(Power::Off)
                            && control.publication.frozen().len() == 3
                    })
                    .await;
                    assert!(!control.context.live.borrow().devices["one"].confirmed);
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
                    until(|| {
                        control.context.live.borrow().devices["one"]
                            .access
                            .is_none()
                    })
                    .await;
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
                let work = control.context.live.borrow().devices["one"].admitted.clone().unwrap();
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
            let status = crate::terminal::AuthStatus::new(control.context.status.borrow().auth.clone(), "/bin/migate".into(), "/data".into());
            let terminal = crate::terminal::bridge::Context::committed(control.publication.clone(), &status);
            assert!(terminal.handle_line("off one relay:3", 0).await.unwrap().unwrap().contains("command accepted"));
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
            assert_eq!(control.store.devices().load_configuration().unwrap(), control.publication.current().configuration);
        }).await;
    })).unwrap()).unwrap().join().unwrap();
}

#[test]
fn unpublished_relays_are_read_and_controlled_per_channel_without_optimism() {
    block_on(rig().run(|c| async move {
        c.announce();
        let peer = panel_peer(RelayMode::Wireless);
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| {
                control
                    .context
                    .registry
                    .get(&DeviceKey::new("xiaomi", "one"))
                    .is_some_and(|device| {
                        device.availability() == Availability::Available
                            && [2, 3, 4].iter().all(|siid| {
                                device.function(&format!("relay:{siid}")).is_some_and(|f| {
                                    f.snapshot().power() == Some(Power::Off)
                                        && f.snapshot().relay_mode() == Some(RelayMode::Wireless)
                                })
                            })
                    })
            })
            .await;
            let key = DeviceKey::new("xiaomi", "one");
            assert!(!control.context.graph.borrow().published.contains(&key));
            let one = function(&control.context.registry, "one", "relay:2");
            let two = function(&control.context.registry, "one", "relay:3");
            let three = function(&control.context.registry, "one", "relay:4");
            until(|| {
                control.context.live.borrow().devices["one"]
                    .admitted
                    .as_ref()
                    .unwrap()
                    .function_observer("relay:3")
                    .is_some()
            })
            .await;
            let mut changes = two.subscribe();
            let status = crate::terminal::AuthStatus::new(
                control.context.status.borrow().auth.clone(),
                "/bin/migate".into(),
                "/data".into(),
            );
            let terminal =
                crate::terminal::bridge::Context::committed(control.publication.clone(), &status);
            let result = terminal
                .handle_line("on one relay:3", 0)
                .await
                .unwrap()
                .unwrap();
            assert!(result.contains("command accepted"), "{result}");
            assert_eq!(two.snapshot().power(), Some(Power::Off));
            assert_eq!(peer.count("rpcReq", "one", 3), 1);
            assert_eq!(peer.count("rpcReq", "one", 2), 0);
            assert_eq!(peer.count("rpcReq", "one", 4), 0);
            assert!(
                peer.state
                    .borrow()
                    .seen
                    .iter()
                    .any(|(operation, value)| operation == "rpcReq"
                        && value["did"] == "one"
                        && value["rpc"]["params"][0]["siid"] == 3
                        && value["rpc"]["params"][0]["piid"] == 1
                        && value["rpc"]["params"][0]["value"] == true)
            );
            peer.notify_value("one", binding(3, 1), json!(true), false);
            assert_eq!(changes.changed().await.power(), Some(Power::On));
            assert_eq!(one.snapshot().power(), Some(Power::Off));
            assert_eq!(three.snapshot().power(), Some(Power::Off));
            let result = terminal
                .handle_line("off one relay:3", 0)
                .await
                .unwrap()
                .unwrap();
            assert!(result.contains("command accepted"), "{result}");
            assert_eq!(two.snapshot().power(), Some(Power::On));
            assert_eq!(peer.count("rpcReq", "one", 3), 2);
        })
        .await;
    }));
}

#[test]
fn modes_remain_observable_when_unpublished_and_survive_failed_reads_and_retirement() {
    block_on(rig().run(|c| async move {
        c.announce();
        let peer = panel_peer(RelayMode::Wireless);
        let control = &c;
        c.peer(peer, |peer| async move {
            let relay = function(&control.context.registry, "one", "relay:2");
            let work = control.context.live.borrow().devices["one"]
                .admitted
                .clone()
                .unwrap();
            until(|| {
                relay.snapshot().relay_mode() == Some(RelayMode::Wireless)
                    && work.function_observer("relay:2").is_some()
                    && work.source_binding_ready(
                        &work.function_observer("relay:2").unwrap(),
                        binding(2, 2),
                    )
            })
            .await;
            let mut changes = relay.subscribe();
            let source_signal = control.context.configuration_changed.listen();
            peer.notify_value("one", binding(2, 2), json!(0), false);
            source_signal.await;
            assert_eq!(changes.changed().await.relay_mode(), Some(RelayMode::Wired));
            assert_eq!(relay.snapshot().power(), Some(Power::Off));

            peer.state
                .borrow_mut()
                .read_fail
                .insert(("one".into(), 2, 2));
            work.refresh_property("relay:2", binding(2, 2));
            until(|| {
                work.refresh
                    .borrow()
                    .property_problem("relay:2", binding(2, 2))
                    .is_some()
            })
            .await;
            assert_eq!(relay.snapshot().relay_mode(), Some(RelayMode::Wired));

            peer.state
                .borrow_mut()
                .read_fail
                .remove(&("one".into(), 2, 2));
            peer.value("one", binding(2, 2), json!(1));
            control.clock.advance(Duration::from_secs(61));
            until(|| relay.snapshot().relay_mode() == Some(RelayMode::Wireless)).await;

            let old_source = work.function_observer("relay:2").unwrap();
            force_unavailable(&control.context, "one", "access revoked");
            control.context.property_value_event(
                &old_source,
                "one",
                binding(2, 2),
                &json!(0),
                None,
                false,
            );
            control.context.property_value_event(
                &old_source,
                "one",
                binding(2, 1),
                &json!(true),
                None,
                false,
            );
            assert_eq!(relay.snapshot().relay_mode(), Some(RelayMode::Wireless));
            assert_eq!(relay.snapshot().power(), Some(Power::Off));
            assert!(relay.execute(Command::On).await.is_err());
            assert_eq!(peer.count("rpcReq", "one", 2), 0);
        })
        .await;
    }));
}

#[test]
fn relay_power_subscriptions_are_independent_when_another_channel_is_rejected() {
    block_on(rig().run(|c| async move {
        c.announce();
        let peer = panel_peer(RelayMode::Wired);
        peer.state
            .borrow_mut()
            .sub_reject
            .insert(("one".into(), 2, 1));
        let control = &c;
        c.peer(peer, |peer| async move {
            let relay = function(&control.context.registry, "one", "relay:3");
            let other = function(&control.context.registry, "one", "relay:4");
            let work = control.context.live.borrow().devices["one"]
                .admitted
                .clone()
                .unwrap();
            until(|| {
                work.function_observer("relay:3").is_some()
                    && relay.snapshot().power() == Some(Power::Off)
                    && other.snapshot().power() == Some(Power::Off)
            })
            .await;
            peer.notify_value("one", binding(3, 1), json!(true), false);
            until(|| relay.snapshot().power() == Some(Power::On)).await;
            assert!(work.function_observer("relay:2").is_none());
            assert_eq!(other.snapshot().power(), Some(Power::Off));
        })
        .await;
    }));
}

#[test]
fn mode_notification_stays_live_when_every_power_subscription_is_rejected() {
    block_on(rig().run(|c| async move {
        c.announce();
        let peer = panel_peer(RelayMode::Wireless);
        for siid in [2, 3, 4] {
            peer.state
                .borrow_mut()
                .sub_reject
                .insert(("one".into(), siid, 1));
        }
        let control = &c;
        c.peer(peer, |peer| async move {
            let relay = function(&control.context.registry, "one", "relay:2");
            let work = control.context.live.borrow().devices["one"]
                .admitted
                .clone()
                .unwrap();
            until(|| {
                work.test_local_exhausted()
                    && work.test_local_subscribed_contains(binding(2, 2))
                    && relay.snapshot().relay_mode() == Some(RelayMode::Wireless)
            })
            .await;
            assert!(work.function_observer("relay:2").is_none());
            peer.notify_value("one", binding(2, 2), json!(0), false);
            until(|| relay.snapshot().relay_mode() == Some(RelayMode::Wired)).await;
            assert_eq!(relay.snapshot().power(), Some(Power::Off));
        })
        .await;
    }));
}

#[test]
fn rejected_mode_subscription_does_not_block_power_feedback() {
    block_on(rig().run(|c| async move {
        c.announce();
        let peer = panel_peer(RelayMode::Wired);
        peer.state
            .borrow_mut()
            .sub_reject
            .insert(("one".into(), 3, 2));
        let control = &c;
        c.peer(peer, |peer| async move {
            let relay = function(&control.context.registry, "one", "relay:3");
            let work = control.context.live.borrow().devices["one"]
                .admitted
                .clone()
                .unwrap();
            until(|| {
                work.test_local_exhausted()
                    && work.function_observer("relay:3").is_some()
                    && relay.snapshot().power() == Some(Power::Off)
            })
            .await;
            let status = crate::terminal::AuthStatus::new(
                control.context.status.borrow().auth.clone(),
                "/bin/migate".into(),
                "/data".into(),
            );
            status.update_adapter(control.context.diagnostics());
            let terminal =
                crate::terminal::bridge::Context::committed(control.publication.clone(), &status);
            let details = terminal
                .handle_line("status one", 0)
                .await
                .unwrap()
                .unwrap();
            let mode = details
                .lines()
                .find(|line| line.contains("Mode: property 3.2"))
                .unwrap();
            assert!(mode.contains("hub subscription not ready"), "{mode}");
            let power = details
                .lines()
                .find(|line| line.contains("Power: property 3.1"))
                .unwrap();
            assert!(power.contains("hub subscription ready"), "{power}");
            peer.notify_value("one", binding(3, 1), json!(true), false);
            until(|| relay.snapshot().power() == Some(Power::On)).await;
        })
        .await;
    }));
}

#[test]
fn retained_and_stale_modes_do_not_replace_confirmed_feedback() {
    block_on(rig().run(|c| async move {
        c.announce();
        let peer = panel_peer(RelayMode::Wireless);
        peer.state
            .borrow_mut()
            .read_timeout
            .insert(("one".into(), 2, 2));
        let control = &c;
        c.peer(peer, |peer| async move {
            let relay = function(&control.context.registry, "one", "relay:2");
            let work = control.context.live.borrow().devices["one"]
                .admitted
                .clone()
                .unwrap();
            until(|| {
                work.function_observer("relay:2").is_some()
                    && work.source_binding_ready(
                        &work.function_observer("relay:2").unwrap(),
                        binding(2, 2),
                    )
                    && peer.state.borrow().held.iter().any(|(request, _)| {
                        let value = decode_mips(&request.payload).unwrap().payload;
                        value["siid"] == 2 && value["piid"] == 2
                    })
            })
            .await;
            let source = work.function_observer("relay:2").unwrap();
            control.context.property_value_event(
                &source,
                "one",
                binding(2, 2),
                &json!(0),
                None,
                true,
            );
            assert_eq!(relay.snapshot().relay_mode(), None);
            control.context.property_value_event(
                &source,
                "one",
                binding(2, 2),
                &json!(0),
                None,
                false,
            );
            assert_eq!(relay.snapshot().relay_mode(), Some(RelayMode::Wired));
            control.context.property_value_event(
                &source,
                "one",
                binding(2, 2),
                &json!(true),
                None,
                false,
            );
            control.context.property_value_event(
                &source,
                "one",
                binding(2, 2),
                &json!(9),
                None,
                false,
            );
            assert_eq!(relay.snapshot().relay_mode(), Some(RelayMode::Wired));
            peer.release_property("one", binding(2, 2), json!({"value":1}))
                .await;
            until(|| !work.refresh.borrow().contains("relay:2", binding(2, 2))).await;
            assert_eq!(relay.snapshot().relay_mode(), Some(RelayMode::Wired));
        })
        .await;
    }));
}

#[test]
fn mode_without_notify_is_polled_but_never_treated_as_a_power_notification() {
    let mut instance: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/zimi-dhkg05.json"
    ))
    .unwrap();
    let left = instance["services"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|service| service["iid"] == 2)
        .unwrap();
    left["properties"][1]["access"] = json!(["read", "write"]);
    let rig = Rig::profile_light("zimi.switch.dhkg05", instance);
    block_on(rig.run(|c| async move {
        c.announce();
        let peer = panel_peer(RelayMode::Wireless);
        let control = &c;
        c.peer(peer, |peer| async move {
            let relay = function(&control.context.registry, "one", "relay:2");
            let work = control.context.live.borrow().devices["one"]
                .admitted
                .clone()
                .unwrap();
            until(|| {
                relay.snapshot().relay_mode() == Some(RelayMode::Wireless)
                    && relay.snapshot().power() == Some(Power::Off)
                    && work.function_observer("relay:2").is_some()
            })
            .await;
            assert!(!work.test_local_subscribed_contains(binding(2, 2)));
            let source = work.function_observer("relay:2").unwrap();
            control.context.property_value_event(
                &source,
                "one",
                binding(2, 2),
                &json!(true),
                None,
                false,
            );
            control.context.property_value_event(
                &source,
                "one",
                binding(2, 2),
                &json!(0),
                None,
                false,
            );
            assert_eq!(relay.snapshot().power(), Some(Power::Off));
            assert_eq!(relay.snapshot().relay_mode(), Some(RelayMode::Wireless));
            peer.value("one", binding(2, 2), json!(0));
            control.clock.advance(Duration::from_secs(61));
            until(|| relay.snapshot().relay_mode() == Some(RelayMode::Wired)).await;
        })
        .await;
    }));
}

#[test]
fn cloud_relay_reads_writes_and_notifications_use_parent_did_and_current_values() {
    let (rig, listener, peer) = with_cloud_rig(rig());
    {
        let mut cloud = rig.cloud.lock().unwrap();
        for siid in [2, 3, 4] {
            cloud
                .raw_values
                .insert(("one".into(), siid, 1), json!(false));
            cloud.raw_values.insert(("one".into(), siid, 2), json!(1));
        }
    }
    block_on(rig.run(|c| async move {
        future::or(peer.run(&listener), async {
            let relay = function(&c.context.registry, "one", "relay:4");
            until(|| {
                relay.snapshot().relay_mode() == Some(RelayMode::Wireless)
                    && relay.snapshot().power() == Some(Power::Off)
                    && c.context.live.borrow().devices["one"]
                        .admitted
                        .as_ref()
                        .unwrap()
                        .function_observer("relay:4")
                        .is_some()
            })
            .await;
            relay.execute(Command::On).await.unwrap();
            assert_eq!(relay.snapshot().power(), Some(Power::Off));
            assert!(c.cloud.lock().unwrap().property_writes.iter().any(
                |(did, siid, piid, value, _)| did == "one" && *siid == 4 && *piid == 1 && *value
            ));
            peer.property_at("one", 4, 1, json!(true));
            until(|| relay.snapshot().power() == Some(Power::On)).await;
            peer.property_at("one", 4, 2, json!(0));
            until(|| relay.snapshot().relay_mode() == Some(RelayMode::Wired)).await;
            assert!(
                c.cloud
                    .lock()
                    .unwrap()
                    .property_reads
                    .iter()
                    .any(|(did, siid, piid)| did == "one" && *siid == 4 && *piid == 2)
            );
            let work = c.context.live.borrow().devices["one"]
                .admitted
                .clone()
                .unwrap();
            assert_eq!(work.observation_source("relay:4"), Some(PathKind::Cloud));
            assert_eq!(
                work.property_source("relay:4", binding(4, 1)),
                Some(PathKind::Cloud)
            );
            let old_source = work.function_observer("relay:4").unwrap();
            c.context
                .live
                .borrow_mut()
                .devices
                .get_mut("one")
                .unwrap()
                .cloud_forbidden = true;
            c.context.update_device_availability("one");
            c.context.changed.notify(usize::MAX);
            c.context.property_value_event(
                &old_source,
                "one",
                binding(4, 1),
                &json!(false),
                None,
                false,
            );
            c.context.property_value_event(
                &old_source,
                "one",
                binding(4, 2),
                &json!(1),
                None,
                false,
            );
            assert_eq!(relay.snapshot().power(), Some(Power::On));
            assert_eq!(relay.snapshot().relay_mode(), Some(RelayMode::Wired));
            assert!(relay.execute(Command::Off).await.is_err());
            assert_eq!(c.cloud.lock().unwrap().property_writes.len(), 1);
        })
        .await;
    }));
}

#[test]
fn relay_mode_is_reread_when_its_usable_path_changes_without_device_unavailability() {
    let (rig, listener, cloud_peer) = with_cloud_rig(rig());
    {
        let mut cloud = rig.cloud.lock().unwrap();
        for siid in [2, 3, 4] {
            cloud
                .raw_values
                .insert(("one".into(), siid, 1), json!(false));
            cloud.raw_values.insert(("one".into(), siid, 2), json!(0));
        }
    }
    block_on(rig.run(|c| async move {
        future::or(cloud_peer.run(&listener), async {
            c.announce();
            let peer = panel_peer(RelayMode::Wireless);
            let control = &c;
            c.peer(peer, |_| async move {
                let relay = function(&control.context.registry, "one", "relay:2");
                let work = control.context.live.borrow().devices["one"]
                    .admitted
                    .clone()
                    .unwrap();
                until(|| {
                    relay.snapshot().relay_mode() == Some(RelayMode::Wireless)
                        && work.test_observer_is_local()
                        && relay.snapshot().availability == Availability::Available
                })
                .await;
                let handle = control
                    .context
                    .live
                    .borrow()
                    .session
                    .as_ref()
                    .unwrap()
                    .handle
                    .clone();
                local_device_offline(&control.context, &handle, "one");
                assert_eq!(relay.snapshot().availability, Availability::Available);
                until(|| relay.snapshot().relay_mode() == Some(RelayMode::Wired)).await;
                assert!(
                    control
                        .cloud
                        .lock()
                        .unwrap()
                        .property_reads
                        .iter()
                        .any(|(did, siid, piid)| did == "one" && *siid == 2 && *piid == 2)
                );
            })
            .await;
        })
        .await;
    }));
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
                    let status = crate::terminal::AuthStatus::new(
                        control.context.status.borrow().auth.clone(),
                        "/bin/migate".into(),
                        "/data".into(),
                    );
                    let terminal = crate::terminal::bridge::Context::committed(
                        control.publication.clone(),
                        &status,
                    );
                    let result = terminal
                        .handle_line("map-type one relay:3 light", 0)
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
                    assert_eq!(control.store.devices().load_configuration().unwrap(), after);
                    assert!(control.publication.plan().unwrap().unwrap().rebuild);
                    let list = terminal.handle_line("list", 0).await.unwrap().unwrap();
                    assert!(list.contains("mode wired; mapping light"), "{list}");
                    let details = terminal
                        .handle_line("status one", 0)
                        .await
                        .unwrap()
                        .unwrap();
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
                            .handle_line("map-type one relay:3 switch", 0)
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

#[test]
fn relay_power_read_reports_its_real_source_in_terminal_status() {
    let rig = rig();
    let status = Rc::new(crate::terminal::AuthStatus::new(
        rig.source.context.status.borrow().auth.clone(),
        "/bin/migate".into(),
        "/data".into(),
    ));
    let report = status.clone();
    block_on(rig.run_with_status(
        move |next| report.update_adapter(next),
        move |c| async move {
            c.announce();
            let peer = panel_peer(RelayMode::Wired);
            let control = &c;
            c.peer(peer, |_| async move {
                until(|| {
                    let live = control.context.live.borrow();
                    let work = live.devices["one"].admitted.as_ref().unwrap();
                    let relay = work.physical().function("relay:3").unwrap();
                    relay.snapshot().power() == Some(Power::Off)
                        && work.function_observer("relay:3").is_some()
                        && work.property_source("relay:3", binding(3, 2)) == Some(PathKind::Local)
                })
                .await;
                async_io::Timer::after(Duration::from_millis(40)).await;
                let terminal = crate::terminal::bridge::Context::committed(
                    control.publication.clone(),
                    &status,
                );
                let details = terminal
                    .handle_line("status one", 0)
                    .await
                    .unwrap()
                    .unwrap();
                let relay = details
                    .split("  relay:3 (")
                    .nth(1)
                    .unwrap()
                    .split("  relay:4 (")
                    .next()
                    .unwrap();
                assert!(
                    relay.contains("Power: property 3.1; observation source hub"),
                    "{relay}"
                );
                assert!(
                    relay.contains("notification source hub; observation source hub"),
                    "{relay}"
                );
            })
            .await;
        },
    ));
}

#[test]
fn relay_power_live_notification_records_source_without_accepting_retained_or_invalid_as_fresh() {
    let rig = rig();
    let status = Rc::new(crate::terminal::AuthStatus::new(
        rig.source.context.status.borrow().auth.clone(),
        "/bin/migate".into(),
        "/data".into(),
    ));
    let report = status.clone();
    block_on(rig.run_with_status(
        move |next| report.update_adapter(next),
        move |c| async move {
            c.announce();
            let peer = panel_peer(RelayMode::Wired);
            peer.state
                .borrow_mut()
                .read_timeout
                .insert(("one".into(), 2, 1));
            let control = &c;
            c.peer(peer, |peer| async move {
                let work = control.context.live.borrow().devices["one"]
                    .admitted
                    .clone()
                    .unwrap();
                let relay = work.physical().function("relay:2").unwrap();
                until(|| {
                    relay.snapshot().relay_mode() == Some(RelayMode::Wired)
                        && work.function_observer("relay:2").is_some()
                })
                .await;
                assert_eq!(relay.snapshot().power(), Some(Power::Unknown));
                assert_eq!(work.observation_source("relay:2"), None);
                assert_eq!(work.property_source("relay:2", binding(2, 1)), None);
                let source = work.function_observer("relay:2").unwrap();
                control.context.property_value_event(
                    &source,
                    "one",
                    binding(2, 1),
                    &json!(true),
                    None,
                    true,
                );
                assert_eq!(relay.snapshot().power(), Some(Power::On));
                assert_eq!(work.property_source("relay:2", binding(2, 1)), None);
                control.context.property_value_event(
                    &source,
                    "one",
                    binding(2, 1),
                    &json!("bad"),
                    None,
                    false,
                );
                assert_eq!(work.observation_source("relay:2"), None);
                let stale = work.observations.read_stamp_input(
                    "relay:2",
                    crate::xiaomi::capability::ReadInput::Property(binding(2, 1)),
                );
                peer.notify_value("one", binding(2, 1), json!(false), false);
                until(|| relay.snapshot().power() == Some(Power::Off)).await;
                work.observe_read_at(
                    "relay:2",
                    binding(2, 1),
                    &stale,
                    &json!(true),
                    Observation {
                        source: PathKind::Cloud,
                        source_time: None,
                        received_at: 0,
                    },
                )
                .unwrap();
                assert_eq!(relay.snapshot().power(), Some(Power::Off));
                assert_eq!(
                    work.property_source("relay:2", binding(2, 1)),
                    Some(PathKind::Local)
                );
                async_io::Timer::after(Duration::from_millis(40)).await;
                let terminal = crate::terminal::bridge::Context::committed(
                    control.publication.clone(),
                    &status,
                );
                let details = terminal
                    .handle_line("status one", 0)
                    .await
                    .unwrap()
                    .unwrap();
                let section = details
                    .split("  relay:2 (")
                    .nth(1)
                    .unwrap()
                    .split("  relay:3 (")
                    .next()
                    .unwrap();
                assert!(
                    section.contains("Power: property 2.1; observation source hub"),
                    "{section}"
                );
                assert!(
                    section.contains("notification source hub; observation source hub"),
                    "{section}"
                );
                control.context.property_value_event(
                    &source,
                    "one",
                    binding(2, 1),
                    &json!(true),
                    None,
                    true,
                );
                assert_eq!(relay.snapshot().power(), Some(Power::Off));
                assert_eq!(work.observation_source("relay:2"), Some(PathKind::Local));
            })
            .await;
        },
    ));
}
