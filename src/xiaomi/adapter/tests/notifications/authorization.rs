use super::*;

#[test]
fn http_pause_keeps_cloud_notifications_and_online_reports_cannot_resume_http() {
    let (rig, listener, peer) = with_cloud_peer(&["one"]);
    block_on(rig.run(|c| async move {
        future::or(peer.run(&listener), async {
            until(|| cloud_active(&c, "one")).await;
            c.clock
                .automatic
                .store(false, std::sync::atomic::Ordering::Relaxed);
            let one = function(&c.context.registry, "one", "outlet:2");
            c.cloud.lock().unwrap().property_status = 503;
            assert!(one.execute(Command::On).await.is_err());
            assert!(c.context.http_paused.get());
            peer.property("one", 2, true);
            until(|| one.snapshot().power() == Some(Power::On)).await;
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
            until(|| one.snapshot().power() == Some(Power::Off)).await;
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
                let one = function(&c.context.registry, "one", "outlet:2");
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
                            super::super::super::cloud::CloudSync::Paused
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
                    function(&control.context.registry, "one", "outlet:2")
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
                    super::super::super::cloud::CloudSync::Paused
                ) && c.context.notifications.borrow().is_none()
            })
            .await;
            assert_eq!(c.cloud.lock().unwrap().tokens, 1);
            assert!(c.cloud.lock().unwrap().property_writes.is_empty());
        })
        .await;
    }));
}
