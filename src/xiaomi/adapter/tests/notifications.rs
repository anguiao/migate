use super::*;

#[test]
fn cloud_notifications_activate_after_subscription_and_update_cloud_only_device() {
    let rig = Rig::new(true, &["one"]);
    rig.cloud.lock().unwrap().online.insert("one".into());
    let listener = Async::<TcpListener>::bind(([127, 0, 0, 1], 0)).unwrap();
    rig.source.context.notification_connector.set(
        super::super::notifications::NotificationConnector::Plaintext(
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
                && function(&c.context.registry, "one", "main")
                    .snapshot()
                    .power
                    == Power::Off
        })
        .await;
        broker.property("one", 2, true).await;
        until(|| {
            function(&c.context.registry, "one", "main")
                .snapshot()
                .power
                == Power::On
        })
        .await;
        assert_eq!(
            function(&c.context.registry, "one", "main")
                .snapshot()
                .availability,
            Availability::Available
        );
    }));
}

fn local_active(c: &Control, did: &str) -> bool {
    c.context.live.borrow().devices[did]
        .admitted
        .as_ref()
        .is_some_and(|work| work.test_observer_is_local())
}

fn startup_paths(members: &[&str]) -> Vec<String> {
    members
        .iter()
        .flat_map(|did| {
            [
                format!("device/{did}/up/properties_changed/2/1"),
                format!("device/{did}/state/#"),
            ]
        })
        .collect()
}

async fn release_startup_subscriptions(peer: &CloudPeer, count: usize) {
    let mut acknowledged = 0;
    while acknowledged < count {
        until(|| !peer.state.borrow().held.is_empty()).await;
        let held = std::mem::take(&mut peer.state.borrow_mut().held);
        assert!(peer.state.borrow().peak_held <= 16);
        acknowledged += held.len();
        for subscribe in held {
            peer.send(Packet::SubAck(SubAck::new(
                subscribe.pkid,
                vec![SubscribeReasonCode::QoS2],
            )));
        }
    }
    assert_eq!(acknowledged, count);
}

#[test]
fn many_devices_share_sixteen_cloud_startup_handshakes_without_queue_full_retries() {
    let members = (0..80)
        .map(|index| format!("device{index:02}"))
        .collect::<Vec<_>>();
    let members = members.iter().map(String::as_str).collect::<Vec<_>>();
    let paths = startup_paths(&members);
    let (rig, listener, peer) = with_cloud_peer(&members);
    rig.clock
        .automatic
        .store(false, std::sync::atomic::Ordering::Relaxed);
    peer.state.borrow_mut().hold.extend(paths.iter().cloned());
    block_on(rig.run(|c| async move {
        future::or(peer.run(&listener), async {
            until(|| peer.state.borrow().held.len() >= 16).await;
            for _ in 0..20 {
                future::yield_now().await;
            }
            assert_eq!(peer.state.borrow().held.len(), 16);
            release_startup_subscriptions(&peer, paths.len()).await;
            until(|| members.iter().all(|did| cloud_active(&c, did))).await;
            for path in &paths {
                assert_eq!(peer.count(path), 1, "{path}");
            }
            assert_eq!(c.clock.now(), Duration::ZERO);
        })
        .await;
    }));
}

#[test]
fn waiting_cloud_startup_subscriptions_are_revoked_and_replaced_before_sending() {
    let members = (0..80)
        .map(|index| format!("device{index:02}"))
        .collect::<Vec<_>>();
    let members = members.iter().map(String::as_str).collect::<Vec<_>>();
    let paths = startup_paths(&members);
    let (rig, listener, peer) = with_cloud_peer(&members);
    rig.clock
        .automatic
        .store(false, std::sync::atomic::Ordering::Relaxed);
    peer.state.borrow_mut().hold.extend(paths.iter().cloned());
    block_on(rig.run(|c| async move {
        future::or(peer.run(&listener), async {
            until(|| peer.state.borrow().held.len() == 16).await;
            assert_eq!(peer.count("device/device79/up/properties_changed/2/1"), 0);
            force_unavailable(&c.context, "device79", "test removal while queued");
            let replaced = c.context.live.borrow().devices["device78"]
                .admitted
                .clone()
                .unwrap();
            replaced.test_retire_cloud_preparation();
            c.context.changed.notify(usize::MAX);
            release_startup_subscriptions(&peer, paths.len() - 2).await;
            until(|| members[..79].iter().all(|did| cloud_active(&c, did))).await;
            assert_eq!(peer.count("device/device79/up/properties_changed/2/1"), 0);
            assert_eq!(peer.count("device/device79/state/#"), 0);
            assert_eq!(peer.count("device/device78/up/properties_changed/2/1"), 1);
            assert!(!replaced.test_cloud_preparing());
        })
        .await;
    }));
}

#[test]
fn reconnect_discards_inflight_and_waiting_startup_subscription_budget() {
    let members = (0..80)
        .map(|index| format!("device{index:02}"))
        .collect::<Vec<_>>();
    let members = members.iter().map(String::as_str).collect::<Vec<_>>();
    let paths = startup_paths(&members);
    let (rig, listener, peer) = with_cloud_peer(&members);
    rig.clock
        .automatic
        .store(false, std::sync::atomic::Ordering::Relaxed);
    peer.state.borrow_mut().hold.extend(paths.iter().cloned());
    block_on(rig.run(|c| async move {
        future::or(peer.run(&listener), async {
            until(|| peer.state.borrow().held.len() == 16).await;
            peer.send(Packet::Disconnect(Disconnect::new()));
            until(|| c.context.notifications.borrow().is_none()).await;
            let due = c.clock.now() + Duration::from_secs(10);
            until(|| c.clock.deadlines.lock().unwrap().contains(&due)).await;
            peer.state.borrow_mut().held.clear();
            peer.state.borrow_mut().seen.clear();
            c.clock.advance(Duration::from_secs(10));
            until(|| peer.state.borrow().connects.len() == 2).await;
            until(|| peer.state.borrow().held.len() == 16).await;
            release_startup_subscriptions(&peer, paths.len()).await;
            until(|| members.iter().all(|did| cloud_active(&c, did))).await;
            for path in &paths {
                assert_eq!(peer.count(path), 1, "{path}");
            }
        })
        .await;
    }));
}

#[test]
fn failed_cloud_handshakes_release_capacity_during_the_retry_delay() {
    let members = (0..20)
        .map(|index| format!("device{index:02}"))
        .collect::<Vec<_>>();
    let members = members.iter().map(String::as_str).collect::<Vec<_>>();
    let paths = startup_paths(&members);
    let (rig, listener, peer) = with_cloud_peer(&members);
    rig.clock
        .automatic
        .store(false, std::sync::atomic::Ordering::Relaxed);
    for did in &members[..16] {
        peer.state.borrow_mut().reject.insert(
            format!("device/{did}/up/properties_changed/2/1"),
            SubscribeReasonCode::QuotaExceeded,
        );
    }
    block_on(rig.run(|c| async move {
        future::or(peer.run(&listener), async {
            until(|| paths.iter().all(|path| peer.count(path) == 1)).await;
            until(|| members[16..].iter().all(|did| cloud_active(&c, did))).await;
            until(|| {
                c.clock
                    .deadlines
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|deadline| **deadline == Duration::from_secs(1))
                    .count()
                    == 16
            })
            .await;
            assert_eq!(c.clock.now(), Duration::ZERO);
            assert!(members[..16].iter().all(|did| !cloud_active(&c, did)));
            peer.state.borrow_mut().reject.clear();
            c.clock.advance(Duration::from_secs(1));
            until(|| members.iter().all(|did| cloud_active(&c, did))).await;
            for (index, did) in members.iter().enumerate() {
                assert_eq!(peer.count(&format!("device/{did}/state/#")), 1);
                assert_eq!(
                    peer.count(&format!("device/{did}/up/properties_changed/2/1")),
                    if index < 16 { 2 } else { 1 }
                );
            }
        })
        .await;
    }));
}

#[test]
fn http_pause_keeps_cloud_notifications_and_online_reports_cannot_resume_http() {
    let (rig, listener, peer) = with_cloud_peer(&["one"]);
    block_on(rig.run(|c| async move {
        future::or(peer.run(&listener), async {
            until(|| cloud_active(&c, "one")).await;
            c.clock
                .automatic
                .store(false, std::sync::atomic::Ordering::Relaxed);
            let one = function(&c.context.registry, "one", "main");
            c.cloud.lock().unwrap().property_status = 503;
            assert!(one.execute(Command::On).await.is_err());
            assert!(c.context.http_paused.get());
            peer.property("one", 2, true);
            until(|| one.snapshot().power == Power::On).await;
            peer.online("one", false);
            until(|| {
                !c.context.live.borrow().devices["one"]
                    .cloud_access
                    .as_ref()
                    .unwrap()
                    .online
            })
            .await;
            assert!(c.context.http_paused.get());
            peer.online("one", true);
            until(|| {
                c.context.live.borrow().devices["one"]
                    .cloud_access
                    .as_ref()
                    .unwrap()
                    .online
                    && cloud_active(&c, "one")
            })
            .await;
            peer.property("one", 2, false);
            until(|| one.snapshot().power == Power::Off).await;
            assert!(c.context.http_paused.get());
            assert!(matches!(
                one.snapshot().availability,
                Availability::Unavailable(_)
            ));
            assert_eq!(c.cloud.lock().unwrap().property_writes.len(), 1);
        })
        .await;
    }));
}

#[test]
fn cloud_forbidden_and_scope_failure_revoke_properties_without_synthesizing_device_info() {
    for scope_failure in [false, true] {
        let (rig, listener, peer) = with_cloud_peer(&["one"]);
        block_on(rig.run(|c| async move {
            future::or(peer.run(&listener), async {
                until(|| cloud_active(&c, "one")).await;
                let one = function(&c.context.registry, "one", "main");
                c.clock
                    .automatic
                    .store(false, std::sync::atomic::Ordering::Relaxed);
                if scope_failure {
                    c.context.cloud_scope_valid.set(false);
                    c.context.update_all_availability();
                    c.context.changed.notify(usize::MAX);
                } else {
                    c.cloud.lock().unwrap().property_code = -704_012_904;
                    assert!(one.execute(Command::On).await.is_err());
                }
                until(|| !cloud_active(&c, "one")).await;
                let snapshot = one.snapshot();
                peer.property("one", 2, true);
                peer.online("one", true);
                async_io::Timer::after(Duration::from_millis(20)).await;
                assert_eq!(one.snapshot(), snapshot);
                if !scope_failure {
                    assert!(c.context.live.borrow().devices["one"].cloud_forbidden);
                    c.context
                        .live
                        .borrow_mut()
                        .devices
                        .get_mut("one")
                        .unwrap()
                        .cloud_access = None;
                    peer.online("one", true);
                    async_io::Timer::after(Duration::from_millis(20)).await;
                    assert!(
                        c.context.live.borrow().devices["one"]
                            .cloud_access
                            .is_none()
                    );
                }
            })
            .await;
        }));
    }
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
                let outlet = function(&control.context.registry, "multi", "outlet_1");
                cloud.qos2("multi", 3, true, 41);
                until(|| cloud.received(41, false)).await;
                local.notify("multi", 3, true);
                until(|| outlet.snapshot().power == Power::On).await;
                cloud.state.borrow_mut().hold.remove(held);
                cloud.release(held);
                until(|| cloud_active(control, "multi")).await;
                until(|| outlet.snapshot().power == Power::Off).await;
                cloud.send(Packet::PubRel(PubRel::new(41)));
                until(|| cloud.received(41, true)).await;
                assert_eq!(outlet.snapshot().power, Power::Off);
                cloud.property("multi", 3, true);
                until(|| outlet.snapshot().power == Power::On).await;
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
                    .set(super::super::routing::PathPriority::CloudFirst);
                control.context.changed.notify(usize::MAX);
                until(|| cloud_active(control, "one")).await;
                cloud.qos2("one", 2, true, 45);
                until(|| cloud.received(45, false)).await;
                control
                    .context
                    .priority
                    .set(super::super::routing::PathPriority::LocalFirst);
                control.context.changed.notify(usize::MAX);
                until(|| local_active(control, "one")).await;
                local.notify("one", 2, false);
                until(|| {
                    function(&control.context.registry, "one", "main")
                        .snapshot()
                        .power
                        == Power::Off
                })
                .await;
                control
                    .context
                    .priority
                    .set(super::super::routing::PathPriority::CloudFirst);
                control.context.changed.notify(usize::MAX);
                until(|| cloud_active(control, "one")).await;
                cloud.send(Packet::PubRel(PubRel::new(45)));
                until(|| cloud.received(45, true)).await;
                assert_eq!(
                    function(&control.context.registry, "one", "main")
                        .snapshot()
                        .power,
                    Power::Off
                );
                cloud.property("one", 2, true);
                until(|| {
                    function(&control.context.registry, "one", "main")
                        .snapshot()
                        .power
                        == Power::On
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
                    function(&control.context.registry, "multi", "usb")
                        .snapshot()
                        .power
                        == Power::On
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
                            physical.functions()[0].snapshot().power == Power::Off
                        })
                })
                .await;
                assert_eq!(local.count("get", "one", 2), 1);
                assert_eq!(local.count("subscribe", "one", 2), 0);
                function(&control.context.registry, "one", "main")
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
fn mqtt_network_retry_caps_at_six_hundred_seconds_and_new_token_resets_it_immediately() {
    let (rig, listener, peer) = with_cloud_peer(&["one"]);
    rig.clock
        .automatic
        .store(false, std::sync::atomic::Ordering::Relaxed);
    peer.state
        .borrow_mut()
        .connacks
        .extend(std::iter::repeat_n(ConnectReturnCode::ServerUnavailable, 9));
    block_on(rig.run(|c| async move {
        future::or(peer.run(&listener), async {
            until(|| peer.state.borrow().connects.len() == 1).await;
            let mut cursor = 0;
            for (index, seconds) in [10, 20, 40, 80, 160, 320, 600].into_iter().enumerate() {
                let due = c.clock.now() + Duration::from_secs(seconds);
                until(|| c.clock.deadlines.lock().unwrap()[cursor..].contains(&due)).await;
                cursor = c.clock.deadlines.lock().unwrap().len();
                c.clock.advance(Duration::from_secs(seconds));
                until(|| peer.state.borrow().connects.len() == index + 2).await;
                assert!(!c.context.http_paused.get());
            }
            let capped = c.clock.now() + Duration::from_secs(600);
            until(|| c.clock.deadlines.lock().unwrap()[cursor..].contains(&capped)).await;
            let mut record = c.store.xiaomi().load().unwrap().unwrap();
            record.tokens.access_token = "replacement-token".into();
            c.store.xiaomi().replace(&record).unwrap();
            c.context.live.borrow_mut().credentials = Some(record);
            cursor = c.clock.deadlines.lock().unwrap().len();
            c.context.credentials_changed.notify(usize::MAX);
            until(|| peer.state.borrow().connects.len() == 9).await;
            assert_eq!(peer.state.borrow().connects[8], "replacement-token");
            let due = c.clock.now() + Duration::from_secs(10);
            until(|| c.clock.deadlines.lock().unwrap()[cursor..].contains(&due)).await;
            c.clock.advance(Duration::from_secs(10));
            until(|| cloud_active(&c, "one")).await;
            let old = c
                .context
                .notifications
                .borrow()
                .as_ref()
                .unwrap()
                .handle
                .clone();
            cursor = c.clock.deadlines.lock().unwrap().len();
            peer.send(Packet::Disconnect(Disconnect::new()));
            until(|| c.context.notifications.borrow().is_none()).await;
            let due = c.clock.now() + Duration::from_secs(10);
            until(|| c.clock.deadlines.lock().unwrap()[cursor..].contains(&due)).await;
            c.clock.advance(Duration::from_secs(10));
            until(|| cloud_active(&c, "one")).await;
            let one = function(&c.context.registry, "one", "main");
            until(|| one.snapshot().power == Power::Off).await;
            c.context.property_value_event(
                &super::super::notifications::NotificationSource::Cloud(old),
                "one",
                crate::xiaomi::capability::PropertyBinding { siid: 2, piid: 1 },
                &json!(true),
                false,
            );
            assert_eq!(one.snapshot().power, Power::Off);
            assert_eq!(c.cloud.lock().unwrap().tokens, 0);
        })
        .await;
    }));
}

#[test]
fn connect_and_suback_authorization_share_refresh_and_preserve_local_control() {
    for rejection in 0..3 {
        let connect_rejection = rejection == 0;
        let (rig, listener, cloud) = with_cloud_peer(&["one"]);
        let (release, gate) = flume::bounded(1);
        rig.cloud.lock().unwrap().home_gate = Some(gate);
        if connect_rejection {
            cloud.state.borrow_mut().connacks.extend([
                ConnectReturnCode::NotAuthorized,
                ConnectReturnCode::NotAuthorized,
            ]);
        } else {
            cloud.state.borrow_mut().reject.insert(
                if rejection == 1 {
                    "device/one/up/properties_changed/2/1"
                } else {
                    "device/one/state/#"
                }
                .into(),
                SubscribeReasonCode::NotAuthorized,
            );
        }
        block_on(rig.run(|c| async move {
            future::or(cloud.run(&listener), async {
                c.announce();
                let control = &c;
                let cloud = &cloud;
                c.peer(Rc::new(Peer::default()), |local| async move {
                    initialized(control, &local, &["one"]).await;
                    control.cloud.lock().unwrap().home_gate = None;
                    release.send(()).unwrap();
                    if rejection == 1 {
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
                    }
                    until(|| {
                        matches!(
                            *control.context.cloud_sync.borrow(),
                            super::super::cloud::CloudSync::Paused
                        )
                    })
                    .await;
                    assert_eq!(control.cloud.lock().unwrap().tokens, 1);
                    until(|| control.context.notifications.borrow().is_none()).await;
                    assert!(matches!(
                        control
                            .context
                            .status
                            .borrow()
                            .authentication
                            .authentication,
                        crate::xiaomi::auth::AuthenticationState::SignInRequired(_)
                    ));
                    function(&control.context.registry, "one", "main")
                        .execute(Command::On)
                        .await
                        .unwrap();
                    assert_eq!(local.count("rpcReq", "one", 2), 1);
                    let old = cloud.state.borrow().connects[0].clone();
                    control
                        .context
                        .token_rejected(
                            &old,
                            crate::xiaomi::auth::FailureReason::CloudNotification(
                                crate::xiaomi::cloud::NotificationError::Unauthorized,
                            ),
                        )
                        .unwrap();
                    assert_eq!(control.cloud.lock().unwrap().tokens, 1);
                    assert!(control.cloud.lock().unwrap().property_writes.is_empty());
                })
                .await;
            })
            .await;
        }));
    }
}

#[test]
fn driver_authorization_rejection_rebuilds_with_fresh_token_then_pauses_on_second_rejection() {
    let (rig, listener, peer) = with_cloud_peer(&["one"]);
    block_on(rig.run(|c| async move {
        future::or(peer.run(&listener), async {
            until(|| cloud_active(&c, "one")).await;
            let mut disconnect = Disconnect::new();
            disconnect.reason_code = DisconnectReasonCode::NotAuthorized;
            peer.send(Packet::Disconnect(disconnect.clone()));
            until(|| peer.state.borrow().connects.len() == 2 && cloud_active(&c, "one")).await;
            assert_eq!(peer.state.borrow().connects[1], "fresh-access");
            assert_eq!(c.cloud.lock().unwrap().tokens, 1);
            peer.send(Packet::Disconnect(disconnect));
            until(|| {
                matches!(
                    *c.context.cloud_sync.borrow(),
                    super::super::cloud::CloudSync::Paused
                ) && c.context.notifications.borrow().is_none()
            })
            .await;
            assert_eq!(c.cloud.lock().unwrap().tokens, 1);
            assert!(c.cloud.lock().unwrap().property_writes.is_empty());
        })
        .await;
    }));
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
                    let one = function(&control.context.registry, "one", "main");
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
                    assert_eq!(one.snapshot().power, Power::Off);
                    if notify_after_send {
                        cloud.property("one", 2, true);
                        until(|| one.snapshot().power == Power::On).await;
                        cloud.property("one", 2, false);
                        until(|| one.snapshot().power == Power::Off).await;
                    }
                    local.release("one", 2, accepted("one", 2, 0)).await;
                    assert_eq!(
                        first.await.unwrap().power,
                        if notify_after_send {
                            Power::Off
                        } else {
                            Power::On
                        }
                    );
                    assert_eq!(
                        second.await.unwrap().power,
                        if notify_after_send {
                            Power::On
                        } else {
                            Power::Off
                        }
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
fn a_real_cloud_online_recovery_reopens_one_exhausted_subscription_window() {
    let (rig, listener, peer) = with_cloud_peer(&["one"]);
    let property = "device/one/up/properties_changed/2/1";
    peer.state
        .borrow_mut()
        .reject
        .insert(property.into(), SubscribeReasonCode::QuotaExceeded);
    block_on(rig.run(|c| async move {
        future::or(peer.run(&listener), async {
            until(|| {
                peer.count(property) == 4
                    && c.context.live.borrow().devices["one"]
                        .admitted
                        .as_ref()
                        .unwrap()
                        .test_cloud_exhausted()
            })
            .await;
            c.clock
                .automatic
                .store(false, std::sync::atomic::Ordering::Relaxed);
            peer.state.borrow_mut().reject.remove(property);
            peer.online("one", true);
            async_io::Timer::after(Duration::from_millis(20)).await;
            assert_eq!(peer.count(property), 4);
            peer.online("one", false);
            until(|| {
                !c.context.live.borrow().devices["one"]
                    .cloud_access
                    .as_ref()
                    .unwrap()
                    .online
            })
            .await;
            peer.online("one", true);
            until(|| cloud_active(&c, "one")).await;
            assert_eq!(peer.count(property), 5);
            assert!(!c.context.http_paused.get());
        })
        .await;
    }));
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
                    super::super::routing::PathPriority::LocalFirst
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
                let one = function(&control.context.registry, "one", "main");
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
                    super::super::routing::PathPriority::CloudFirst
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
                super::super::routing::PathPriority::CloudFirst
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

#[test]
fn held_online_subscription_cannot_retry_after_device_access_is_revoked() {
    let (rig, listener, peer) = with_cloud_peer(&["one"]);
    let online = "device/one/state/#";
    let property = "device/one/up/properties_changed/2/1";
    peer.state.borrow_mut().hold.insert(online.into());
    block_on(rig.run(|c| async move {
        future::or(peer.run(&listener), async {
            until(|| {
                cloud_active(&c, "one")
                    && peer.count(online) == 1
                    && peer
                        .state
                        .borrow()
                        .held
                        .iter()
                        .any(|subscribe| subscribe.filters[0].path == online)
            })
            .await;
            assert_eq!(peer.count(property), 1);
            c.clock
                .automatic
                .store(false, std::sync::atomic::Ordering::Relaxed);
            let token_requests = c.cloud.lock().unwrap().tokens;
            c.cloud.lock().unwrap().property_code = -704_012_904;
            let one = function(&c.context.registry, "one", "main");
            assert_eq!(
                one.execute(Command::On).await,
                Err(crate::device::CommandError::Rejected(
                    "cloud code -704012904".into()
                ))
            );
            until(|| {
                c.context.live.borrow().devices["one"].cloud_forbidden
                    && !cloud_active(&c, "one")
                    && peer.state.borrow().seen.iter().any(|packet| {
                        matches!(packet, Packet::Unsubscribe(unsubscribe)
                            if unsubscribe.filters.iter().any(|filter| filter == online))
                    })
            })
            .await;

            // A late ACK belongs to the cancelled preparation, even on the same connection.
            peer.release(online);
            async_io::Timer::after(Duration::from_millis(20)).await;
            c.clock.advance(Duration::from_secs(1));
            for _ in 0..20 {
                c.context.changed.notify(usize::MAX);
                future::yield_now().await;
            }
            async_io::Timer::after(Duration::from_millis(20)).await;
            assert_eq!(
                peer.count(online),
                1,
                "a retired online preparation must not subscribe the forbidden device again"
            );
            assert_eq!(peer.count(property), 1);
            assert_eq!(c.cloud.lock().unwrap().tokens, token_requests);
            assert!(c.context.notifications.borrow().is_some());
            assert!(
                c.context.live.borrow().devices["one"]
                    .admitted
                    .as_ref()
                    .unwrap()
                    .test_cloud_subscribed_empty()
            );
        })
        .await;
    }));
}

#[test]
fn same_value_notification_invalidates_an_inflight_read() {
    block_on(Rig::new(true, &["one", "two"]).run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        let control = &c;
        c.peer(peer, |peer| async move {
            initialized(control, &peer, &["one", "two"]).await;
            let one = function(&control.context.registry, "one", "main");
            let two = function(&control.context.registry, "two", "main");
            peer.state.borrow_mut().hold_reads = true;
            let reads = peer.count("get", "one", 2);
            control.context.live.borrow().devices["one"]
                .admitted
                .as_ref()
                .unwrap()
                .refresh();
            until(|| peer.count("get", "one", 2) == reads + 1).await;
            let before = one.snapshot();
            peer.notify("one", 2, false);
            peer.notify("two", 2, true);
            until(|| two.snapshot().power == Power::On).await;
            assert_eq!(one.snapshot(), before);
            peer.release("one", 2, json!({"value":true})).await;
            async_io::Timer::after(Duration::from_millis(20)).await;
            assert_eq!(one.snapshot().power, Power::Off);
        })
        .await;
    }));
}

#[test]
fn notification_after_write_preserves_state_and_advances_the_channel_fifo() {
    block_on(Rig::new(true, &["one", "two"]).run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        let control = &c;
        c.peer(peer, |peer| async move {
            initialized(control, &peer, &["one", "two"]).await;
            let one = function(&control.context.registry, "one", "main");
            let two = function(&control.context.registry, "two", "main");
            peer.state.borrow_mut().hold_writes = true;
            let mut first = Box::pin(one.execute(Command::On));
            let mut second = Box::pin(one.execute(Command::Toggle));
            assert!(poll_once(first.as_mut()).await.is_none());
            assert!(poll_once(second.as_mut()).await.is_none());
            until(|| peer.count("rpcReq", "one", 2) == 1).await;
            peer.notify("one", 2, false);
            peer.notify("two", 2, true);
            until(|| two.snapshot().power == Power::On).await;
            peer.release("one", 2, accepted("one", 2, 0)).await;
            assert_eq!(first.await.unwrap().power, Power::Off);
            until(|| peer.count("rpcReq", "one", 2) == 2 && !peer.state.borrow().held.is_empty())
                .await;
            assert_eq!(
                peer.state
                    .borrow()
                    .seen
                    .iter()
                    .rev()
                    .find(|(operation, _)| operation == "rpcReq")
                    .unwrap()
                    .1["rpc"]["params"][0]["value"],
                true
            );
            peer.release("one", 2, accepted("one", 2, 0)).await;
            assert_eq!(second.await.unwrap().power, Power::On);
        })
        .await;
    }));
}

#[test]
fn virtual_groups_follow_typed_members_and_new_relations_without_group_reads() {
    let rig = Rig::lights(&[
        ("one", "xiaomi.light.ceil04"),
        ("two", "xiaomi.light.ceil04"),
    ]);
    {
        let mut cloud = rig.cloud.lock().unwrap();
        let instance = super::support::group_instance();
        cloud
            .catalog_instances
            .insert(instance["type"].as_str().unwrap().into(), instance);
        cloud.catalog_groups = vec![
            super::support::group_record("record-1", "group-1", &["one"], false),
            super::support::group_record("record-2", "group-2", &["two"], false),
        ];
        cloud.online.extend(["one", "two"].map(str::to_owned));
        for did in ["one", "two"] {
            for (piid, value) in [(1, json!(false)), (2, json!(50)), (3, json!(2700))] {
                cloud.raw_values.insert((did.into(), 2, piid), value);
            }
        }
        // Deliberately stale group values must never overwrite member feedback.
        cloud
            .raw_values
            .insert(("group-1".into(), 2, 1), json!(true));
        cloud
            .raw_values
            .insert(("group-2".into(), 2, 1), json!(true));
    }
    let listener = Async::<TcpListener>::bind(([127, 0, 0, 1], 0)).unwrap();
    rig.source.context.notification_connector.set(
        super::super::notifications::NotificationConnector::Plaintext(
            listener.get_ref().local_addr().unwrap(),
        ),
    );
    let peer = Rc::new(CloudPeer::default());
    block_on(rig.run(|c| async move {
        let peer_run = peer.clone();
        future::or(peer_run.run(&listener), async {
            c.announce();
            until(|| {
                ["group-1", "group-2"].iter().all(|did| {
                    c.context
                        .registry
                        .get(&DeviceKey::new("xiaomi", *did))
                        .is_some()
                })
            })
            .await;
            until(|| cloud_active(&c, "one") && cloud_active(&c, "two")).await;
            let first = function(&c.context.registry, "group-1", "main");
            let second = function(&c.context.registry, "group-2", "main");
            until(|| first.snapshot().power == Power::Off && second.snapshot().power == Power::Off)
                .await;

            c.cloud
                .lock()
                .unwrap()
                .raw_values
                .insert(("one".into(), 2, 1), json!(true));
            peer.property("one", 2, true);
            until(|| first.snapshot().power == Power::On).await;
            assert_eq!(second.snapshot().power, Power::Off);
            for (piid, value) in [(2, json!(70)), (3, json!(4000))] {
                c.cloud
                    .lock()
                    .unwrap()
                    .raw_values
                    .insert(("one".into(), 2, piid), value.clone());
                peer.send(Packet::Publish(Publish::new(
                    format!("device/one/up/properties_changed/2/{piid}"),
                    QoS::AtMostOnce,
                    json!({"params":{"did":"one","siid":2,"piid":piid,"value":value}}).to_string(),
                )));
            }
            until(|| {
                first.snapshot().lighting.is_some_and(|light| {
                    light.brightness_percent == Some(70.0) && light.temperature_kelvin == Some(4000)
                })
            })
            .await;

            // Adding an already observed member applies its current state;
            // no new event or duplicate subscription is needed.
            let subscriptions = peer.count("device/one/up/properties_changed/2/1");
            c.cloud.lock().unwrap().catalog_groups[1] =
                super::support::group_record("record-2", "group-2", &["two", "one"], false);
            c.context.request_sync();
            until(|| second.snapshot().power == Power::On).await;
            assert_eq!(
                peer.count("device/one/up/properties_changed/2/1"),
                subscriptions
            );
            c.cloud
                .lock()
                .unwrap()
                .raw_values
                .insert(("one".into(), 2, 1), json!(false));
            peer.property("one", 2, false);
            until(|| first.snapshot().power == Power::Off && second.snapshot().power == Power::Off)
                .await;

            // Losing a member's actual source invalidates both groups instead
            // of treating the unavailable member as off.
            peer.online("one", false);
            until(|| {
                first.snapshot().power == Power::Unknown
                    && second.snapshot().power == Power::Unknown
            })
            .await;
            let cleared = first.snapshot().lighting.unwrap();
            assert_eq!(cleared.brightness_percent, None);
            assert_eq!(cleared.temperature_kelvin, None);
            assert_eq!(cleared.color, None);
            assert_eq!(cleared.mode, None);
            c.cloud
                .lock()
                .unwrap()
                .raw_values
                .insert(("one".into(), 2, 1), json!(true));
            peer.online("one", true);
            until(|| first.snapshot().power == Power::On && second.snapshot().power == Power::On)
                .await;

            assert!(
                c.cloud
                    .lock()
                    .unwrap()
                    .property_reads
                    .iter()
                    .all(|(did, _, _)| !did.starts_with("group-"))
            );
            assert!(!peer.state.borrow().seen.iter().any(|packet| matches!(packet,
                Packet::Subscribe(subscribe) if subscribe.filters.iter().any(|filter|
                    (filter.path.contains("group-") && filter.path.contains("properties_changed"))
                        || filter.path.ends_with("properties_changed/#")
                )
            )));
        })
        .await;
    }));
}

#[test]
fn virtual_group_reconnect_reads_members_even_when_property_subscriptions_fail() {
    let rig = super::support::repair_rig(0, false);
    let listener = Async::<TcpListener>::bind(([127, 0, 0, 1], 0)).unwrap();
    rig.source.context.notification_connector.set(
        super::super::notifications::NotificationConnector::Plaintext(
            listener.get_ref().local_addr().unwrap(),
        ),
    );
    let peer = Rc::new(CloudPeer::default());
    let property = "device/member-a/up/properties_changed/2/1";
    peer.state
        .borrow_mut()
        .reject
        .insert(property.into(), SubscribeReasonCode::QuotaExceeded);
    block_on(rig.run(|c| async move {
        future::or(peer.run(&listener), async {
            super::support::repair_ready(&c).await;
            let member = c.context.live.borrow().devices["member-a"]
                .admitted
                .clone()
                .unwrap();
            until(|| member.test_cloud_exhausted()).await;
            c.clock
                .automatic
                .store(false, std::sync::atomic::Ordering::Relaxed);
            assert!(member.observer().is_none());
            let group = function(&c.context.registry, "group-1", "main");
            assert_eq!(group.snapshot().power, Power::Off);
            let key = ("member-a".to_string(), 2, 1);
            let reads = || {
                c.cloud
                    .lock()
                    .unwrap()
                    .property_reads
                    .iter()
                    .filter(|read| **read == key)
                    .count()
            };
            let before = reads();
            let subscriptions = peer.count(property);
            c.cloud
                .lock()
                .unwrap()
                .raw_values
                .insert(key.clone(), json!(true));
            peer.send(Packet::Disconnect(Disconnect::new()));
            until(|| c.context.notifications.borrow().is_none()).await;
            let due = c.clock.now() + Duration::from_secs(10);
            until(|| c.clock.deadlines.lock().unwrap().contains(&due)).await;
            c.clock.advance(Duration::from_secs(10));
            until(|| peer.state.borrow().connects.len() == 2).await;
            c.clock
                .automatic
                .store(true, std::sync::atomic::Ordering::Relaxed);
            until(|| peer.count(property) > subscriptions && member.test_cloud_exhausted()).await;
            until(|| reads() > before && group.snapshot().power == Power::On).await;
            assert!(member.observer().is_none());
            assert_eq!(group.snapshot().availability, Availability::Available);
            assert!(
                c.context
                    .diagnostics()
                    .devices
                    .iter()
                    .any(|device| device.did == "group-1" && device.sync_limited_reason.is_some())
            );
            assert!(
                c.cloud
                    .lock()
                    .unwrap()
                    .property_reads
                    .iter()
                    .all(|(did, _, _)| did != "group-1")
            );
        })
        .await;
    }));
}
