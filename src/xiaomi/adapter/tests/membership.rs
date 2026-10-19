use super::*;

#[test]
fn invalid_or_missing_local_capabilities_only_restrict_the_applicable_paths() {
    for missing_push in [false, true] {
        let rig = Rig::new(false, &["one", "multi"]);
        {
            let mut cloud = rig.cloud.lock().unwrap();
            cloud.online.insert("multi".into());
            cloud.names.insert("multi".into(), "Cloud strip".into());
        }
        block_on(rig.run(|c| async move {
            c.announce();
            let peer = Rc::new(Peer::default());
            {
                let mut state = peer.state.borrow_mut();
                state
                    .names
                    .insert("multi".into(), "Stale local name".into());
                if missing_push {
                    state.pushless.insert("multi".into());
                } else {
                    state.bad.insert("multi".into());
                }
            }
            let control = &c;
            c.peer(peer, |peer| async move {
                initialized(control, &peer, &["one"]).await;
                until(|| control.context.cloud_candidate("multi", true).is_ok()).await;
                let strip = control
                    .context
                    .registry
                    .get(&DeviceKey::new("xiaomi", "multi"))
                    .unwrap();
                assert_eq!(strip.functions().len(), 3);
                assert_eq!(strip.description().label(), "Cloud strip");
                assert_eq!(
                    control.context.local_candidate("multi", false).is_ok(),
                    missing_push
                );
                assert!(control.context.local_candidate("multi", true).is_err());
                function(&control.context.registry, "multi", "usb")
                    .execute(Command::On)
                    .await
                    .unwrap();
                let writes = control.cloud.lock().unwrap().property_writes.clone();
                if missing_push {
                    assert!(writes.is_empty());
                    assert_eq!(peer.count("rpcReq", "multi", 9), 1);
                } else {
                    assert_eq!(writes.len(), 1);
                    assert_eq!(
                        (&*writes[0].0, writes[0].1, writes[0].2, writes[0].3),
                        ("multi", 9, 1, true)
                    );
                    assert_eq!(peer.count("rpcReq", "multi", 0), 0);
                }
                assert_eq!(peer.count("subscribe", "multi", 0), 0);
                {
                    let mut state = peer.state.borrow_mut();
                    state.bad.clear();
                    state.pushless.clear();
                }
                peer.list_changed(&["multi"]);
                initialized(control, &peer, &["one", "multi"]).await;
                function(&control.context.registry, "multi", "usb")
                    .execute(Command::Off)
                    .await
                    .unwrap();
                assert_eq!(
                    peer.count("rpcReq", "multi", 9),
                    1 + usize::from(missing_push)
                );
                assert_eq!(
                    control.cloud.lock().unwrap().property_writes.len(),
                    writes.len()
                );
                assert_eq!(strip.description().label(), "Cloud strip");
            })
            .await;
        }));
    }
}

#[test]
fn stale_local_metadata_does_not_block_local_control_after_cloud_admission() {
    let rig = Rig::new(false, &["multi"]);
    rig.cloud.lock().unwrap().online.insert("multi".into());
    block_on(rig.run(|mut c| async move {
        c.data
            .iter_mut()
            .find(|record| record.device.key().id == "multi")
            .unwrap()
            .model = "stale.local.model".into();
        c.announce();
        let control = &c;
        c.peer(Rc::new(Peer::default()), |peer| async move {
            until(|| {
                peer.lists() > 0
                    && control.context.cloud_candidate("multi", true).is_ok()
                    && control.context.live.borrow().devices["multi"]
                        .access
                        .is_some()
            })
            .await;
            // Directory metadata disagreement is not an access qualification.
            assert!(control.context.local_candidate("multi", false).is_ok());
            let usb = function(&control.context.registry, "multi", "usb");
            usb.execute(Command::On).await.unwrap();
            assert_eq!(peer.count("rpcReq", "multi", 9), 1);
            assert!(control.cloud.lock().unwrap().property_writes.is_empty());
            usb.execute(Command::Off).await.unwrap();
            assert_eq!(peer.count("rpcReq", "multi", 9), 2);
            assert!(control.context.cloud_candidate("multi", true).is_ok());
        })
        .await;
    }));
}

#[test]
fn supported_descriptions_and_selected_home_membership_remain_required() {
    let rig = Rig::new(false, &["one", "multi"]);
    rig.cloud.lock().unwrap().unsupported.insert("multi".into());
    block_on(rig.run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(Rc::new(Peer::default()), |peer| async move {
            initialized(control, &peer, &["one"]).await;
            until(|| {
                control.context.live.borrow().devices["multi"]
                    .blocked
                    .is_some()
            })
            .await;
            control.context.reconcile("multi").unwrap();
            assert!(
                control
                    .context
                    .status
                    .borrow()
                    .pending
                    .iter()
                    .any(|pending| {
                        pending.did == "multi" && pending.reason.contains("missing")
                    })
            );
            assert_eq!(control.context.registry.list().len(), 1);
            for did in ["multi", "two"] {
                assert!(
                    control
                        .store
                        .devices()
                        .load_configuration()
                        .unwrap()
                        .device(&DeviceKey::new("xiaomi", did))
                        .cloned()
                        .is_none()
                );
            }
            control.cloud.lock().unwrap().unsupported.clear();
            peer.list_changed(&["multi"]);
            initialized(control, &peer, &["one", "multi"]).await;
            assert!(
                control
                    .context
                    .registry
                    .get(&DeviceKey::new("xiaomi", "two"))
                    .is_none()
            );
        })
        .await;
    }));
}

#[test]
fn cloud_members_wait_for_local_discovery_to_select_the_first_home() {
    let rig = Rig::new(false, &["one"]);
    rig.cloud.lock().unwrap().online.insert("one".into());
    block_on(rig.run(|c| async move {
        until(|| c.context.status.borrow().catalog_problem.is_some()).await;
        assert!(c.context.live.borrow().scope.is_none());
        assert!(c.context.registry.list().is_empty());
        assert_eq!(c.cloud.lock().unwrap().details, 0);
        c.announce();
        until(|| {
            c.context
                .registry
                .get(&DeviceKey::new("xiaomi", "one"))
                .is_some()
        })
        .await;
        assert_eq!(
            c.context.live.borrow().scope.as_ref().unwrap().home_id,
            "local"
        );
    }));
}

#[test]
fn five_minute_member_sync_does_not_poll_healthy_channel_power() {
    block_on(Rig::new(true, &["one", "multi"]).run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        let control = &c;
        c.peer(peer, |peer| async move {
            initialized(control, &peer, &["one", "multi"]).await;
            until(|| {
                control
                    .context
                    .live
                    .borrow()
                    .devices
                    .values()
                    .all(|slot| slot.loading.is_none())
                    && control
                        .clock
                        .deadlines
                        .lock()
                        .unwrap()
                        .iter()
                        .any(|deadline| *deadline >= Duration::from_secs(300))
            })
            .await;
            let reads = peer
                .state
                .borrow()
                .seen
                .iter()
                .filter(|(op, _)| op == "get")
                .count();
            let homes = control.cloud.lock().unwrap().homes;
            let lists = peer
                .state
                .borrow()
                .seen
                .iter()
                .filter(|(op, _)| op == "getDevList")
                .count();
            control
                .clock
                .automatic
                .store(false, std::sync::atomic::Ordering::Relaxed);
            let next_sync = control.clock.now() + Duration::from_secs(300);
            control.clock.advance(Duration::from_secs(300));
            until(|| {
                control.cloud.lock().unwrap().homes > homes
                    && control
                        .clock
                        .deadlines
                        .lock()
                        .unwrap()
                        .iter()
                        .any(|deadline| *deadline > next_sync)
            })
            .await;
            control.clock.advance(Duration::from_secs(3));
            peer.notify("one", 2, true);
            until(|| {
                function(&control.context.registry, "one", "main")
                    .snapshot()
                    .power
                    == Power::On
            })
            .await;
            for _ in 0..10 {
                future::yield_now().await;
            }
            assert_eq!(peer.lists(), lists);
            assert_eq!(
                peer.state
                    .borrow()
                    .seen
                    .iter()
                    .filter(|(op, _)| op == "get")
                    .count(),
                reads
            );
        })
        .await;
    }));
}

#[test]
fn an_empty_bridge_adds_removes_and_readds_with_stable_identity_and_name() {
    block_on(Rig::new(false, &[]).run(|mut c| async move {
        c.ready.take().unwrap().wait().await.unwrap();
        let registry = c.context.registry.clone();
        assert!(registry.list().is_empty());
        c.announce();
        let peer = Rc::new(Peer::default());
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| control.context.live.borrow().scope.is_some()).await;
            control.cloud.lock().unwrap().members = vec!["multi".into()];
            peer.list_changed(&["multi"]);
            initialized(control, &peer, &["multi"]).await;
            let key = DeviceKey::new("xiaomi", "multi");
            control.publication.rename(&key, "Kitchen saved").unwrap();
            let before = control
                .store
                .devices()
                .load_configuration()
                .unwrap()
                .device(&key)
                .cloned()
                .unwrap();
            let old = function(&registry, "multi", "outlet_1");
            control.cloud.lock().unwrap().members.clear();
            peer.list_changed(&["multi"]);
            until(|| registry.list().is_empty() && peer.state.borrow().unsubscribes.len() == 4)
                .await;
            assert!(
                control
                    .store
                    .devices()
                    .load_configuration()
                    .unwrap()
                    .device(&key)
                    .unwrap()
                    .native
                    .is_none()
            );
            assert_eq!(
                old.execute(Command::On).await,
                Err(crate::device::CommandError::Cancelled)
            );
            peer.notify("multi", 3, true);
            control.cloud.lock().unwrap().members = vec!["multi".into()];
            peer.list_changed(&["multi"]);
            initialized(control, &peer, &["multi"]).await;
            let after = control
                .store
                .devices()
                .load_configuration()
                .unwrap()
                .device(&key)
                .cloned()
                .unwrap();
            assert_eq!(after.unique_id, before.unique_id);
            assert_eq!(after.endpoint, before.endpoint);
            assert_eq!(after.functions, before.functions);
            assert_eq!(after.label, "Kitchen saved");
            assert_eq!(
                function(&registry, "multi", "outlet_1").snapshot().power,
                Power::Off
            );
        })
        .await;
    }));
}

#[test]
fn incomplete_foreign_or_failed_cloud_sync_cannot_remove_published_devices() {
    for failure in 0..4 {
        block_on(Rig::new(true, &["one"]).run(|c| async move {
            c.announce();
            let peer = Rc::new(Peer::default());
            let control = &c;
            c.peer(peer, |peer| async move {
                initialized(control, &peer, &["one"]).await;
                {
                    let mut cloud = control.cloud.lock().unwrap();
                    cloud.members.clear();
                    match failure {
                        0 => cloud.partial = true,
                        1 => cloud.foreign = true,
                        2 => cloud.status = 503,
                        _ => cloud.members = vec!["one".into()],
                    };
                }
                if failure == 3 {
                    control.cloud.lock().unwrap().missing_home = true;
                }
                peer.list_changed(&["one"]);
                until(|| {
                    control.context.status.borrow().catalog_problem.is_some()
                        || matches!(
                            control
                                .context
                                .status
                                .borrow()
                                .authentication
                                .authentication,
                            crate::xiaomi::auth::AuthenticationState::Unavailable(_)
                        )
                })
                .await;
                assert!(
                    control
                        .context
                        .registry
                        .get(&DeviceKey::new("xiaomi", "one"))
                        .is_some()
                );
                assert!(
                    control
                        .store
                        .devices()
                        .load_configuration()
                        .unwrap()
                        .device(&DeviceKey::new("xiaomi", "one"))
                        .cloned()
                        .is_some()
                );
                assert_eq!(
                    function(&control.context.registry, "one", "main")
                        .snapshot()
                        .availability,
                    Availability::Available
                );
                assert!(peer.state.borrow().unsubscribes.is_empty());
            })
            .await;
        }));
    }
}

#[test]
fn removed_pending_reads_subscriptions_and_commands_cannot_revive_a_device() {
    block_on(Rig::new(true, &["one", "multi"]).run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        peer.state.borrow_mut().omit.insert("one".into());
        peer.state
            .borrow_mut()
            .read_timeout
            .insert(("one".into(), 2, 1));
        peer.state
            .borrow_mut()
            .sub_timeout
            .insert(("one".into(), 2, 1));
        let control = &c;
        c.peer(peer, |peer| async move {
            initialized(control, &peer, &["multi"]).await;
            peer.state.borrow_mut().omit.remove("one");
            peer.list_changed(&["one"]);
            until(|| peer.count("get", "one", 2) == 1 && peer.count("subscribe", "one", 2) == 1)
                .await;
            let one = function(&control.context.registry, "one", "main");
            peer.state.borrow_mut().hold_writes = true;
            let mut command = Box::pin(one.execute(Command::On));
            assert!(poll_once(command.as_mut()).await.is_none());
            until(|| peer.count("rpcReq", "one", 2) == 1).await;
            control.cloud.lock().unwrap().members = vec!["multi".into()];
            peer.list_changed(&["one"]);
            until(|| {
                control
                    .context
                    .registry
                    .get(&DeviceKey::new("xiaomi", "one"))
                    .is_none()
                    && !peer.state.borrow().unsubscribes.is_empty()
            })
            .await;
            assert_eq!(command.await, Err(crate::device::CommandError::Cancelled));
            let subs = std::mem::take(&mut peer.state.borrow_mut().held_subs);
            for sub in subs {
                peer.send(Packet::SubAck(SubAck::new(
                    sub.pkid,
                    vec![SubscribeReasonCode::QoS2],
                )));
            }
            peer.release("one", 2, json!({"value":true})).await;
            peer.release("one", 2, accepted("one", 2, 0)).await;
            peer.notify("one", 2, true);
            peer.notify("multi", 4, true);
            until(|| {
                function(&control.context.registry, "multi", "outlet_2")
                    .snapshot()
                    .power
                    == Power::On
            })
            .await;
            control.clock.advance(Duration::from_secs(10));
            for _ in 0..10 {
                future::yield_now().await;
            }
            assert!(
                control
                    .context
                    .registry
                    .get(&DeviceKey::new("xiaomi", "one"))
                    .is_none()
            );
            assert_eq!(peer.count("get", "one", 2), 1);
            assert_eq!(peer.count("subscribe", "one", 2), 1);
        })
        .await;
    }));
}

#[test]
fn a_complete_cloud_snapshot_remains_valid_across_a_local_reconnect() {
    let rig = Rig::new(true, &["one", "multi"]);
    let (release, gate) = flume::unbounded();
    block_on(rig.run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(Rc::new(Peer::default()), |peer| async move {
            initialized(control, &peer, &["one", "multi"]).await;
            until(|| control.context.cloud_scope_valid.get()).await;
            let homes = {
                let mut cloud = control.cloud.lock().unwrap();
                cloud.members = vec!["multi".into()];
                cloud.home_gate = Some(gate);
                cloud.homes
            };
            control.context.request_sync();
            until(|| control.cloud.lock().unwrap().homes > homes).await;
        })
        .await;
        until(|| {
            matches!(
                c.context.status.borrow().gateway,
                GatewayStatus::Unavailable(_)
            )
        })
        .await;
        c.clock.advance(Duration::from_secs(6));
        c.peer(Rc::new(Peer::default()), |peer| async move {
            initialized(control, &peer, &["one", "multi"]).await;
            release.send(()).unwrap();
            until(|| {
                control
                    .context
                    .registry
                    .get(&DeviceKey::new("xiaomi", "one"))
                    .is_none()
            })
            .await;
            assert!(
                control
                    .context
                    .registry
                    .get(&DeviceKey::new("xiaomi", "multi"))
                    .is_some()
            );
            function(&control.context.registry, "multi", "usb")
                .execute(Command::On)
                .await
                .unwrap();
        })
        .await;
    }));
}

#[test]
fn pending_cloud_descriptions_retry_on_a_later_event_without_blocking_cached_control() {
    let rig = Rig::new(true, &["one"]);
    rig.cloud.lock().unwrap().members = vec!["one".into(), "multi".into()];
    rig.cloud.lock().unwrap().detail_status = 503;
    block_on(rig.run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| {
                control
                    .context
                    .status
                    .borrow()
                    .pending
                    .iter()
                    .any(|pending| pending.did == "multi" && pending.reason.contains("503"))
            })
            .await;
            initialized(control, &peer, &["one"]).await;
            function(&control.context.registry, "one", "main")
                .execute(Command::On)
                .await
                .unwrap();
            assert_eq!(control.context.registry.list().len(), 1);
            assert!(control.cloud.lock().unwrap().details >= 1);
            control.cloud.lock().unwrap().detail_status = 200;
            peer.list_changed(&["multi"]);
            initialized(control, &peer, &["one", "multi"]).await;
            assert!(
                control
                    .context
                    .status
                    .borrow()
                    .pending
                    .iter()
                    .all(|pending| pending.did != "multi")
            );
        })
        .await;
    }));
}
