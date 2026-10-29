use super::*;

#[test]
fn cloud_notifications_activate_after_subscription_and_update_cloud_only_device() {
    let rig = Rig::new(true, &["one"]);
    rig.cloud.lock().unwrap().online.insert("one".into());
    let listener = Async::<TcpListener>::bind(([127, 0, 0, 1], 0)).unwrap();
    rig.source.context.notification_connector.set(
        super::super::super::notifications::NotificationConnector::Plaintext(
            listener.get_ref().local_addr().unwrap(),
        ),
    );
    block_on(rig.run(|c| async move {
        let mut broker = CloudBroker::accept(&listener).await;
        let mut property = false;
        let mut online = false;
        while !property || !online {
            if let Packet::Subscribe(subscribe) = broker.next().await {
                property |= subscribe.filters[0].path.contains("properties_changed");
                online |= subscribe.filters[0].path.ends_with("state/#");
                broker.ack(subscribe).await;
            }
        }
        until(|| {
            cloud_active(&c, "one")
                && function(&c.context.registry, "one", "outlet:2")
                    .snapshot()
                    .power()
                    == Some(Power::Off)
        })
        .await;
        broker.property("one", 2, true).await;
        until(|| {
            function(&c.context.registry, "one", "outlet:2")
                .snapshot()
                .power()
                == Some(Power::On)
        })
        .await;
        assert_eq!(
            function(&c.context.registry, "one", "outlet:2")
                .snapshot()
                .availability,
            Availability::Available
        );
    }));
}

#[test]
fn partial_cloud_preparation_keeps_local_and_preparing_qos2_cannot_cross_activation() {
    let (rig, listener, cloud) = with_cloud_peer(&["multi"]);
    let held = "device/multi/up/properties_changed/9/1";
    cloud.state.borrow_mut().hold.insert(held.into());
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            c.announce();
            let control = &c;
            let cloud = &cloud;
            c.peer(Rc::new(Peer::default()), |local| async move {
                initialized(control, &local, &["multi"]).await;
                until(|| local_active(control, "multi")).await;
                control
                    .clock
                    .automatic
                    .store(false, std::sync::atomic::Ordering::Relaxed);
                async_io::Timer::after(Duration::from_millis(20)).await;
                let paths = [
                    "device/multi/up/properties_changed/2/1",
                    "device/multi/up/properties_changed/3/1",
                    "device/multi/up/properties_changed/4/1",
                    held,
                ];
                let counts = paths.map(|path| cloud.count(path));
                let handle = control
                    .context
                    .live
                    .borrow()
                    .session
                    .as_ref()
                    .unwrap()
                    .handle
                    .clone();
                control.context.local_outcome(
                    &handle,
                    &crate::xiaomi::central::RequestOutcome::<()> {
                        result: Err(crate::xiaomi::central::CentralError::Timeout),
                        stage: crate::xiaomi::central::RequestStage::MayHaveBeenSent,
                        timeout: Some(crate::xiaomi::central::RequestTimeout::Response),
                    },
                );
                until(|| {
                    cloud
                        .state
                        .borrow()
                        .held
                        .iter()
                        .any(|subscribe| subscribe.filters[0].path == held)
                })
                .await;
                assert!(local_active(control, "multi"));
                let outlet = function(&control.context.registry, "multi", "outlet:3");
                cloud.qos2("multi", 3, true, 41);
                until(|| cloud.received(41, false)).await;
                local.notify("multi", 3, true);
                until(|| outlet.snapshot().power() == Some(Power::On)).await;
                cloud.state.borrow_mut().hold.remove(held);
                cloud.release(held);
                until(|| cloud_active(control, "multi")).await;
                until(|| outlet.snapshot().power() == Some(Power::Off)).await;
                cloud.send(Packet::PubRel(PubRel::new(41)));
                until(|| cloud.received(41, true)).await;
                assert_eq!(outlet.snapshot().power(), Some(Power::Off));
                cloud.property("multi", 3, true);
                until(|| outlet.snapshot().power() == Some(Power::On)).await;
                for (path, count) in paths.into_iter().zip(counts) {
                    assert_eq!(cloud.count(path), count + 1);
                }
            })
            .await;
        })
        .await;
    }));
}

#[test]
fn local_cloud_local_delivery_epochs_do_not_reactivate_an_old_pubrel() {
    let (rig, listener, cloud) = with_cloud_peer(&["one"]);
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            c.announce();
            let control = &c;
            let cloud = &cloud;
            c.peer(Rc::new(Peer::default()), |local| async move {
                initialized(control, &local, &["one"]).await;
                until(|| local_active(control, "one")).await;
                control
                    .context
                    .priority
                    .set(super::super::super::routing::PathPriority::CloudFirst);
                control.context.changed.notify(usize::MAX);
                until(|| cloud_active(control, "one")).await;
                cloud.qos2("one", 2, true, 45);
                until(|| cloud.received(45, false)).await;
                control
                    .context
                    .priority
                    .set(super::super::super::routing::PathPriority::LocalFirst);
                control.context.changed.notify(usize::MAX);
                until(|| local_active(control, "one")).await;
                local.notify("one", 2, false);
                until(|| {
                    function(&control.context.registry, "one", "outlet:2")
                        .snapshot()
                        .power()
                        == Some(Power::Off)
                })
                .await;
                control
                    .context
                    .priority
                    .set(super::super::super::routing::PathPriority::CloudFirst);
                control.context.changed.notify(usize::MAX);
                until(|| cloud_active(control, "one")).await;
                cloud.send(Packet::PubRel(PubRel::new(45)));
                until(|| cloud.received(45, true)).await;
                assert_eq!(
                    function(&control.context.registry, "one", "outlet:2")
                        .snapshot()
                        .power(),
                    Some(Power::Off)
                );
                cloud.property("one", 2, true);
                until(|| {
                    function(&control.context.registry, "one", "outlet:2")
                        .snapshot()
                        .power()
                        == Some(Power::On)
                })
                .await;
            })
            .await;
        })
        .await;
    }));
}

#[test]
fn exhausted_local_preparation_uses_cloud_and_diagnostic_wakes_do_not_restart_it() {
    let (rig, listener, cloud) = with_cloud_peer(&["multi"]);
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            // Establish the shared cloud connection before opening the local
            // retry window; its real attachment change may otherwise reopen it.
            until(|| cloud_active(&c, "multi")).await;
            c.announce();
            let control = &c;
            let cloud = &cloud;
            let local = Rc::new(Peer::default());
            local
                .state
                .borrow_mut()
                .sub_reject
                .insert(("multi".into(), 9, 1));
            c.peer(local, |local| async move {
                until(|| {
                    local.count("subscribe", "multi", 9) == 4 && cloud_active(control, "multi")
                })
                .await;
                control
                    .clock
                    .automatic
                    .store(false, std::sync::atomic::Ordering::Relaxed);
                let count = local.count("subscribe", "multi", 9);
                for _ in 0..20 {
                    control.context.changed.notify(usize::MAX);
                    future::yield_now().await;
                }
                assert_eq!(local.count("subscribe", "multi", 9), count);
                cloud.property("multi", 9, true);
                until(|| {
                    function(&control.context.registry, "multi", "outlet:9")
                        .snapshot()
                        .power()
                        == Some(Power::On)
                })
                .await;
                assert!(
                    control.context.live.borrow().devices["multi"]
                        .admitted
                        .as_ref()
                        .unwrap()
                        .test_local_exhausted()
                );
                assert_eq!(local.count("subscribe", "multi", 3), 1);
                // A real information update opens one new bounded preparation window.
                local.state.borrow_mut().sub_reject.clear();
                local.list_changed(&["multi"]);
                let due = control.clock.now() + Duration::from_millis(10);
                until(|| control.clock.deadlines.lock().unwrap().contains(&due)).await;
                control.clock.advance(Duration::from_millis(10));
                until(|| local_active(control, "multi")).await;
                assert_eq!(local.count("subscribe", "multi", 9), count + 1);
            })
            .await;
        })
        .await;
    }));
}

#[test]
fn new_and_restored_pushless_devices_read_and_control_without_local_notifications() {
    for restored in [true, false] {
        block_on(Rig::new(restored, &["one"]).run(|c| async move {
            c.announce();
            let local = Rc::new(Peer::default());
            local.state.borrow_mut().pushless.insert("one".into());
            let control = &c;
            c.peer(local, |local| async move {
                until(|| {
                    control
                        .context
                        .registry
                        .get(&DeviceKey::new("xiaomi", "one"))
                        .is_some_and(|physical| {
                            physical.functions()[0].snapshot().power() == Some(Power::Off)
                        })
                })
                .await;
                assert_eq!(local.count("get", "one", 2), 1);
                assert_eq!(local.count("subscribe", "one", 2), 0);
                function(&control.context.registry, "one", "outlet:2")
                    .execute(Command::On)
                    .await
                    .unwrap();
                assert_eq!(local.count("rpcReq", "one", 2), 1);
                assert!(control.context.local_candidate("one", true).is_err());
                assert!(
                    !control.context.live.borrow().devices["one"]
                        .admitted
                        .as_ref()
                        .is_some_and(|work| work.test_local_attached())
                );
            })
            .await;
        }));
    }
}

#[test]
fn real_source_handoff_and_cloud_read_preserve_inflight_write_rules_and_fifo() {
    for notify_after_send in [false, true] {
        let (mut rig, listener, cloud) = with_cloud_peer(&["one"]);
        rig.source.request_timeout = Duration::from_secs(2);
        block_on(rig.run(|c| async move {
            future::or(cloud.run(&listener), async {
                c.announce();
                let control = &c;
                let cloud = &cloud;
                c.peer(Rc::new(Peer::default()), |local| async move {
                    initialized(control, &local, &["one"]).await;
                    until(|| local_active(control, "one")).await;
                    control
                        .clock
                        .automatic
                        .store(false, std::sync::atomic::Ordering::Relaxed);
                    let one = function(&control.context.registry, "one", "outlet:2");
                    local.state.borrow_mut().hold_writes = true;
                    let mut first = Box::pin(one.execute(Command::On));
                    assert!(poll_once(first.as_mut()).await.is_none());
                    until(|| local.count("rpcReq", "one", 2) == 1).await;
                    let mut second = Box::pin(one.execute(Command::Toggle));
                    assert!(poll_once(second.as_mut()).await.is_none());
                    let reads = control.cloud.lock().unwrap().property_reads.len();
                    let handle = control
                        .context
                        .live
                        .borrow()
                        .session
                        .as_ref()
                        .unwrap()
                        .handle
                        .clone();
                    control.context.local_outcome(
                        &handle,
                        &crate::xiaomi::central::RequestOutcome::<()> {
                            result: Err(crate::xiaomi::central::CentralError::Timeout),
                            stage: crate::xiaomi::central::RequestStage::MayHaveBeenSent,
                            timeout: Some(crate::xiaomi::central::RequestTimeout::Response),
                        },
                    );
                    until(|| {
                        cloud_active(control, "one")
                            && control.cloud.lock().unwrap().property_reads.len() > reads
                    })
                    .await;
                    let work = control.context.live.borrow().devices["one"]
                        .admitted
                        .clone()
                        .unwrap();
                    until(|| work.refresh.borrow().snapshot().inflight.is_empty()).await;
                    assert_eq!(one.snapshot().power(), Some(Power::Off));
                    if notify_after_send {
                        cloud.property("one", 2, true);
                        until(|| one.snapshot().power() == Some(Power::On)).await;
                        cloud.property("one", 2, false);
                        until(|| one.snapshot().power() == Some(Power::Off)).await;
                    }
                    local.release("one", 2, accepted("one", 2, 0)).await;
                    assert_eq!(
                        first.await.unwrap().power(),
                        Some(if notify_after_send {
                            Power::Off
                        } else {
                            Power::On
                        })
                    );
                    assert_eq!(
                        second.await.unwrap().power(),
                        Some(if notify_after_send {
                            Power::On
                        } else {
                            Power::Off
                        })
                    );
                    assert_eq!(local.count("rpcReq", "one", 2), 1);
                    let cloud = control.cloud.lock().unwrap();
                    assert_eq!(cloud.property_writes.len(), 1);
                    assert_eq!(cloud.property_writes[0].3, notify_after_send);
                })
                .await;
            })
            .await;
        }));
    }
}

#[test]
fn losing_cloud_connection_reopens_the_exhausted_local_fallback_window() {
    let (rig, listener, cloud) = with_cloud_peer(&["one"]);
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            let local = Rc::new(Peer::default());
            local
                .state
                .borrow_mut()
                .sub_reject
                .insert(("one".into(), 2, 1));
            c.announce();
            let control = &c;
            let cloud = &cloud;
            c.peer(local, |local| async move {
                until(|| {
                    local.count("subscribe", "one", 2) == 4
                        && cloud_active(control, "one")
                        && control.context.live.borrow().devices["one"]
                            .admitted
                            .as_ref()
                            .unwrap()
                            .test_local_exhausted()
                })
                .await;
                assert_eq!(
                    control.context.priority(),
                    super::super::super::routing::PathPriority::LocalFirst
                );
                control
                    .clock
                    .automatic
                    .store(false, std::sync::atomic::Ordering::Relaxed);
                async_io::Timer::after(Duration::from_millis(20)).await;
                local.state.borrow_mut().sub_reject.clear();

                cloud.send(Packet::Disconnect(Disconnect::new()));
                until(|| control.context.notifications.borrow().is_none()).await;
                async_io::Timer::after(Duration::from_millis(20)).await;
                assert!(
                    !control.context.live.borrow().devices["one"]
                        .admitted
                        .as_ref()
                        .unwrap()
                        .test_local_exhausted(),
                    "removing the cloud candidate must reopen one local fallback window"
                );
                let due = control.clock.now() + Duration::from_millis(10);
                advance_to(&control.clock, due).await;
                until(|| local_active(control, "one")).await;
                assert_eq!(local.count("subscribe", "one", 2), 5);
                for _ in 0..20 {
                    control.context.changed.notify(usize::MAX);
                    future::yield_now().await;
                }
                async_io::Timer::after(Duration::from_millis(20)).await;
                assert_eq!(local.count("subscribe", "one", 2), 5);
            })
            .await;
        })
        .await;
    }));
}

#[test]
fn losing_local_connection_reopens_cloud_without_another_priority_change() {
    let (rig, listener, cloud) = with_cloud_peer(&["one"]);
    let property = "device/one/up/properties_changed/2/1";
    cloud
        .state
        .borrow_mut()
        .reject
        .insert(property.into(), SubscribeReasonCode::QuotaExceeded);
    let (release, gate) = flume::bounded(1);
    rig.cloud.lock().unwrap().home_gate = Some(gate);
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            c.announce();
            let control = &c;
            let cloud = &cloud;
            c.peer(Rc::new(Peer::default()), |local| async move {
                initialized(control, &local, &["one"]).await;
                until(|| local_active(control, "one")).await;
                control.cloud.lock().unwrap().home_gate = None;
                release.send(()).unwrap();
                until(|| cloud.count("device/one/state/#") == 1).await;

                local.state.borrow_mut().hold_writes = true;
                let one = function(&control.context.registry, "one", "outlet:2");
                let command = one.execute(Command::On);
                assert_eq!(command.await, Err(crate::device::CommandError::Timeout));
                until(|| {
                    cloud.count(property) == 4
                        && local_active(control, "one")
                        && control.context.live.borrow().devices["one"]
                            .admitted
                            .as_ref()
                            .unwrap().test_cloud_exhausted()
                })
                .await;
                assert_eq!(
                    control.context.priority(),
                    super::super::super::routing::PathPriority::CloudFirst
                );
                control
                    .clock
                    .automatic
                    .store(false, std::sync::atomic::Ordering::Relaxed);
                async_io::Timer::after(Duration::from_millis(20)).await;
                cloud.state.borrow_mut().reject.clear();
            })
            .await;

            until(|| c.context.live.borrow().session.is_none()).await;
            async_io::Timer::after(Duration::from_millis(20)).await;
            assert_eq!(
                c.context.priority(),
                super::super::super::routing::PathPriority::CloudFirst
            );
            assert!(
                !c.context.live.borrow().devices["one"]
                    .admitted
                    .as_ref()
                    .unwrap().test_cloud_exhausted(),
                "removing local must reopen the eligible cloud source even when priority is unchanged"
            );
            until(|| cloud_active(&c, "one")).await;
            assert_eq!(cloud.count(property), 5);
            for _ in 0..20 {
                c.context.changed.notify(usize::MAX);
                future::yield_now().await;
            }
            async_io::Timer::after(Duration::from_millis(20)).await;
            assert_eq!(cloud.count(property), 5);
        })
        .await;
    }));
}

#[test]
fn changed_cloud_online_conditions_reopen_local_windows_but_same_values_do_not() {
    let (rig, listener, cloud) = with_cloud_peer(&["one"]);
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            let local = Rc::new(Peer::default());
            local
                .state
                .borrow_mut()
                .sub_reject
                .insert(("one".into(), 2, 1));
            c.announce();
            let control = &c;
            let cloud = &cloud;
            c.peer(local, |local| async move {
                until(|| {
                    local.count("subscribe", "one", 2) == 4
                        && cloud_active(control, "one")
                        && control.context.live.borrow().devices["one"]
                            .admitted
                            .as_ref()
                            .unwrap()
                            .test_local_exhausted()
                })
                .await;
                control
                    .clock
                    .automatic
                    .store(false, std::sync::atomic::Ordering::Relaxed);
                for _ in 0..20 {
                    cloud.online("one", true);
                    control.context.changed.notify(usize::MAX);
                    future::yield_now().await;
                }
                async_io::Timer::after(Duration::from_millis(20)).await;
                assert_eq!(local.count("subscribe", "one", 2), 4);
                assert!(
                    control.context.live.borrow().devices["one"]
                        .admitted
                        .as_ref()
                        .unwrap()
                        .test_local_exhausted()
                );

                cloud.online("one", false);
                until(|| {
                    !control.context.live.borrow().devices["one"]
                        .cloud_access
                        .as_ref()
                        .unwrap()
                        .online
                })
                .await;
                async_io::Timer::after(Duration::from_millis(20)).await;
                assert!(
                    !control.context.live.borrow().devices["one"]
                        .admitted
                        .as_ref()
                        .unwrap()
                        .test_local_exhausted(),
                    "a real cloud online change must reopen the still-eligible local source"
                );
                for attempt in 5..=8 {
                    let spacing = if attempt == 5 { 10 } else { 100 };
                    let due = control.clock.now() + Duration::from_millis(spacing);
                    advance_to(&control.clock, due).await;
                    until(|| local.count("subscribe", "one", 2) == attempt).await;
                }
                until(|| {
                    control.context.live.borrow().devices["one"]
                        .admitted
                        .as_ref()
                        .unwrap()
                        .test_local_exhausted()
                })
                .await;
                for _ in 0..20 {
                    cloud.online("one", false);
                    control.context.changed.notify(usize::MAX);
                    future::yield_now().await;
                }
                async_io::Timer::after(Duration::from_millis(20)).await;
                assert_eq!(local.count("subscribe", "one", 2), 8);
                assert!(
                    control.context.live.borrow().devices["one"]
                        .admitted
                        .as_ref()
                        .unwrap()
                        .test_local_exhausted()
                );

                local.state.borrow_mut().sub_reject.clear();
                cloud.online("one", true);
                until(|| {
                    control.context.live.borrow().devices["one"]
                        .cloud_access
                        .as_ref()
                        .unwrap()
                        .online
                })
                .await;
                async_io::Timer::after(Duration::from_millis(20)).await;
                assert!(
                    !control.context.live.borrow().devices["one"]
                        .admitted
                        .as_ref()
                        .unwrap()
                        .test_local_exhausted()
                );
                let due = control.clock.now() + Duration::from_millis(10);
                advance_to(&control.clock, due).await;
                until(|| local_active(control, "one")).await;
                assert_eq!(local.count("subscribe", "one", 2), 9);
            })
            .await;
        })
        .await;
    }));
}
