use super::*;

#[test]
fn a_bad_candidate_remains_pending_without_blocking_good_devices() {
    block_on(Rig::new(false, &["one", "multi"]).run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        peer.state.borrow_mut().bad.insert("multi".into());
        let control = &c;
        c.peer(peer, |peer| async move {
            initialized(control, &peer, &["one"]).await;
            until(|| {
                control
                    .context
                    .status
                    .borrow()
                    .pending
                    .iter()
                    .any(|pending| pending.did == "multi")
            })
            .await;
            assert_eq!(control.context.registry.list().len(), 1);
            assert!(
                control
                    .store
                    .devices()
                    .load_published(&DeviceKey::new("xiaomi", "multi"))
                    .unwrap()
                    .is_none()
            );
            peer.state.borrow_mut().bad.clear();
            peer.list_changed(&["multi"]);
            initialized(control, &peer, &["one", "multi"]).await;
        })
        .await;
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
        let registry = c.ready.take().unwrap().wait().await.unwrap();
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
            control
                .store
                .devices()
                .set_device_label(&key, "Kitchen saved")
                .unwrap();
            let before = control
                .store
                .devices()
                .load_published(&key)
                .unwrap()
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
                    .load_published(&key)
                    .unwrap()
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
                .load_published(&key)
                .unwrap()
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
                    let mut scope = control.context.live.borrow().scope.clone().unwrap();
                    scope.home_id = "missing".into();
                    control.context.live.borrow_mut().scope = Some(scope);
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
                        .load_published(&DeviceKey::new("xiaomi", "one"))
                        .unwrap()
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
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.members = vec!["multi".into()];
        cloud.home_gate = Some(gate);
    }
    block_on(rig.run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(Rc::new(Peer::default()), |peer| async move {
            initialized(control, &peer, &["one", "multi"]).await;
            until(|| control.cloud.lock().unwrap().homes == 1).await;
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
