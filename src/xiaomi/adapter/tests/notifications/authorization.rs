use super::*;

#[test]
fn restored_home_receives_ha_notifications_after_mijia_rejection() {
    let (rig, listener, peer) = with_cloud_peer(&["one"]);
    let path = rig.source.context.mijia_path().unwrap();
    rig.source.context.cloud_error(
        &path,
        None,
        &crate::xiaomi::cloud::CloudError::new(
            "test",
            crate::xiaomi::cloud::CloudErrorKind::Unauthorized,
        ),
    );
    block_on(rig.run(|c| async move {
        future::or(peer.run(&listener), async {
            until(|| cloud_active(&c, "one")).await;
            let one = function(&c.context.registry, "one", "outlet:2");
            peer.property("one", 2, true);
            until(|| one.snapshot().power() == Some(Power::On)).await;
            assert_eq!(c.cloud.lock().unwrap().homes, 0);
            assert!(c.context.cloud_candidate("one", true).is_err());
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
                let handle = c
                    .context
                    .notifications
                    .borrow()
                    .as_ref()
                    .unwrap()
                    .handle
                    .clone();
                c.clock
                    .automatic
                    .store(false, std::sync::atomic::Ordering::Relaxed);
                if scope_failure {
                    c.context.lan_scope_invalid.set(true);
                    c.context.cloud_scope_valid.set(false);
                    c.context.update_all_availability();
                    c.context.changed.notify(usize::MAX);
                } else {
                    c.cloud.lock().unwrap().property_code = -704_012_904;
                    assert!(one.execute(Command::On).await.is_err());
                }
                until(|| !cloud_active(&c, "one")).await;
                let snapshot = one.snapshot();
                if scope_failure {
                    c.context.property_value_event(
                        &super::super::super::notifications::NotificationSource::Cloud(handle),
                        "one",
                        crate::xiaomi::capability::PropertyBinding { siid: 2, piid: 1 },
                        &json!(true),
                        None,
                        false,
                    );
                } else {
                    peer.property("one", 2, true);
                    peer.online("one", true);
                }
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
        let old = rig.source.context.ha_key().unwrap();
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
                                response_timeout_id: None,
                            },
                        );
                    }
                    until(|| {
                        matches!(
                            *control.context.ha_sync.borrow(),
                            super::super::super::credentials::HaSync::Paused
                        )
                    })
                    .await;
                    assert_eq!(control.cloud.lock().unwrap().tokens, 1);
                    until(|| control.context.notifications.borrow().is_none()).await;
                    assert!(matches!(
                        control.context.status.borrow().auth.ha,
                        crate::xiaomi::auth::AuthenticationState::SignInRequired(_)
                    ));
                    function(&control.context.registry, "one", "outlet:2")
                        .execute(Command::On)
                        .await
                        .unwrap();
                    if rejection == 1 {
                        assert_eq!(control.cloud.lock().unwrap().property_writes.len(), 1);
                    } else {
                        assert_eq!(local.count("rpcReq", "one", 2), 1);
                    }
                    until(|| control.context.cloud_candidate("one", true).is_ok()).await;
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
                    assert_eq!(
                        control.cloud.lock().unwrap().property_writes.len(),
                        usize::from(rejection == 1)
                    );
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
                    *c.context.ha_sync.borrow(),
                    super::super::super::credentials::HaSync::Paused
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
fn identical_new_ha_generation_retires_old_connection_and_its_events() {
    let (rig, listener, peer) = with_cloud_peer(&["one"]);
    block_on(rig.run(|c| async move {
        future::or(peer.run(&listener), async {
            until(|| cloud_active(&c, "one")).await;
            let old_key = c.context.ha_key().unwrap();
            let old_handle = c
                .context
                .notifications
                .borrow()
                .as_ref()
                .unwrap()
                .handle
                .clone();
            let record = c.store.xiaomi().load().unwrap().unwrap();
            c.store
                .xiaomi()
                .fixture_update_tokens(&record.ha_authorization().unwrap().tokens)
                .unwrap();
            c.context.reload_credentials();
            c.context
                .token_rejected(
                    &old_key,
                    crate::xiaomi::auth::FailureReason::CloudNotification(
                        crate::xiaomi::cloud::NotificationError::Unauthorized,
                    ),
                )
                .unwrap();
            assert!(!c.store.xiaomi().load().unwrap().unwrap().ha.stopped);
            assert_eq!(c.cloud.lock().unwrap().tokens, 0);
            until(|| peer.state.borrow().connects.len() == 2 && cloud_active(&c, "one")).await;
            assert_eq!(
                peer.state.borrow().connects[0],
                peer.state.borrow().connects[1]
            );
            let one = function(&c.context.registry, "one", "outlet:2");
            let before = one.snapshot();
            c.context.property_value_event(
                &super::super::super::notifications::NotificationSource::Cloud(old_handle),
                "one",
                crate::xiaomi::capability::PropertyBinding { siid: 2, piid: 1 },
                &json!(true),
                None,
                false,
            );
            assert_eq!(one.snapshot(), before);
            peer.property("one", 2, true);
            until(|| one.snapshot().power() == Some(Power::On)).await;
        })
        .await;
    }));
}

#[test]
fn refreshed_credentials_reject_old_notification() {
    let (rig, listener, peer) = with_cloud_peer(&["one"]);
    block_on(rig.run(|c| async move {
        future::or(peer.run(&listener), async {
            until(|| cloud_active(&c, "one")).await;
            let one = function(&c.context.registry, "one", "outlet:2");
            until(|| one.snapshot().power() == Some(Power::Off)).await;
            let old_handle = c
                .context
                .notifications
                .borrow()
                .as_ref()
                .unwrap()
                .handle
                .clone();
            let record = c.store.xiaomi().load().unwrap().unwrap();
            c.store
                .xiaomi()
                .fixture_update_tokens(&record.ha_authorization().unwrap().tokens)
                .unwrap();
            c.context.reload_credentials();
            c.context.property_value_event(
                &super::super::super::notifications::NotificationSource::Cloud(old_handle),
                "one",
                crate::xiaomi::capability::PropertyBinding { siid: 2, piid: 1 },
                &json!(true),
                None,
                false,
            );
            assert_eq!(one.snapshot().power(), Some(Power::Off));
        })
        .await;
    }));
}

#[test]
fn delayed_unauthorized_connack_cannot_stop_identical_new_generation() {
    let (rig, listener, peer) = with_cloud_peer(&["one"]);
    let (release, gate) = flume::bounded(1);
    {
        let mut state = peer.state.borrow_mut();
        state.connacks.push_back(ConnectReturnCode::NotAuthorized);
        state.held_connacks.push_back(gate);
    }
    block_on(rig.run(|c| async move {
        future::or(peer.run(&listener), async {
            until(|| peer.state.borrow().connects.len() == 1).await;
            let record = c.store.xiaomi().load().unwrap().unwrap();
            c.store
                .xiaomi()
                .fixture_update_tokens(&record.ha_authorization().unwrap().tokens)
                .unwrap();
            c.context.reload_credentials();
            release.send(()).unwrap();
            until(|| peer.state.borrow().connects.len() == 2 && cloud_active(&c, "one")).await;
            assert_eq!(
                peer.state.borrow().connects[0],
                peer.state.borrow().connects[1]
            );
            assert_eq!(c.cloud.lock().unwrap().tokens, 0);
            assert!(!c.store.xiaomi().load().unwrap().unwrap().ha.stopped);
        })
        .await;
    }));
}

#[test]
fn delayed_unauthorized_suback_cannot_stop_identical_new_generation() {
    let (rig, listener, peer) = with_cloud_peer(&["one"]);
    let path = "device/one/up/properties_changed/2/1";
    peer.state.borrow_mut().hold.insert(path.into());
    block_on(rig.run(|c| async move {
        future::or(peer.run(&listener), async {
            until(|| {
                peer.state
                    .borrow()
                    .held
                    .iter()
                    .any(|s| s.filters[0].path == path)
            })
            .await;
            let record = c.store.xiaomi().load().unwrap().unwrap();
            c.store
                .xiaomi()
                .fixture_update_tokens(&record.ha_authorization().unwrap().tokens)
                .unwrap();
            c.context.reload_credentials();
            let subscribe = {
                let mut state = peer.state.borrow_mut();
                state.hold.remove(path);
                let index = state
                    .held
                    .iter()
                    .position(|s| s.filters[0].path == path)
                    .unwrap();
                state.held.remove(index)
            };
            peer.send(Packet::SubAck(SubAck::new(
                subscribe.pkid,
                vec![SubscribeReasonCode::NotAuthorized],
            )));
            until(|| {
                peer.state.borrow().sent.iter().any(
                    |packet| matches!(packet, Packet::SubAck(ack) if ack.pkid == subscribe.pkid),
                ) || c.context.notifications.borrow().is_none()
            })
            .await;
            c.context.reload_credentials();
            until(|| peer.state.borrow().connects.len() == 2 && cloud_active(&c, "one")).await;
            assert_eq!(c.cloud.lock().unwrap().tokens, 0);
            assert!(!c.store.xiaomi().load().unwrap().unwrap().ha.stopped);
        })
        .await;
    }));
}
