use super::*;

fn token_deadlines(rig: &Rig, refresh: i64, expiry: i64) {
    let mut record = rig.store.xiaomi().load().unwrap().unwrap();
    record.ha.authorization.tokens.refresh_at = rig.clock.wall() + refresh;
    record.ha.authorization.tokens.expires_at = rig.clock.wall() + expiry;
    rig.store.xiaomi().fixture_replace(&record).unwrap();
    rig.source.context.adopt_credentials(record);
}

#[test]
fn mqtt_and_token_refresh_work_without_an_ha_identity_probe() {
    let (rig, listener, peer) = with_cloud_peer(&["one"]);
    rig.clock
        .automatic
        .store(false, std::sync::atomic::Ordering::Relaxed);
    token_deadlines(&rig, 5, 100);
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.home_status = 503;
        cloud.cert_status = 503;
    }
    block_on(rig.run(|c| async move {
        future::or(peer.run(&listener), async {
            until(|| cloud_active(&c, "one") && c.cloud.lock().unwrap().homes > 0).await;
            assert!(matches!(
                c.context.diagnostics().auth.ha,
                crate::xiaomi::auth::AuthenticationState::Authenticated
            ));
            assert!(c.context.cloud_candidate("one", true).is_err());
            assert_eq!(c.cloud.lock().unwrap().ha_homes, 0);
            c.clock.advance(Duration::from_secs(5));
            until(|| {
                c.cloud.lock().unwrap().tokens == 1
                    && matches!(
                        c.context.diagnostics().auth.ha,
                        crate::xiaomi::auth::AuthenticationState::Authenticated
                    )
            })
            .await;
            assert_eq!(c.cloud.lock().unwrap().ha_homes, 0);
        })
        .await;
    }));
}

#[test]
fn ha_rejection_and_refresh_do_not_request_an_extra_mijia_directory_round() {
    let rig = Rig::new(true, &["one"]);
    rig.cloud.lock().unwrap().online.insert("one".into());
    block_on(rig.run(|c| async move {
        until(|| {
            c.context.cloud_candidate("one", true).is_ok() && c.cloud.lock().unwrap().catalogs > 0
        })
        .await;
        c.clock
            .automatic
            .store(false, std::sync::atomic::Ordering::Relaxed);
        let before = c.cloud.lock().unwrap().homes;
        let key = c.context.ha_key().unwrap();
        c.context
            .token_rejected(
                &key,
                crate::xiaomi::auth::FailureReason::CloudNotification(
                    crate::xiaomi::cloud::NotificationError::Unauthorized,
                ),
            )
            .unwrap();
        until(|| c.cloud.lock().unwrap().tokens == 1).await;
        until(|| {
            matches!(
                c.context.diagnostics().auth.ha,
                crate::xiaomi::auth::AuthenticationState::Authenticated
            )
        })
        .await;
        async_io::Timer::after(Duration::from_millis(20)).await;
        assert_eq!(c.cloud.lock().unwrap().homes, before);
    }));
}

#[test]
fn saved_hub_control_works_with_stopped_ha_authorization() {
    let rig = Rig::new(true, &["one"]);
    let mut record = rig.store.xiaomi().load().unwrap().unwrap();
    record.ha.stopped = true;
    rig.store.xiaomi().fixture_replace(&record).unwrap();
    rig.source.context.adopt_credentials(record);
    block_on(rig.run(|control| async move {
        control.announce();
        let peer = Rc::new(Peer::default());
        let current = &control;
        control
            .peer(peer, |peer| async move {
                initialized(current, &peer, &["one"]).await;
                function(&current.context, "one", "outlet:2")
                    .execute(Command::On)
                    .await
                    .unwrap();
                assert!(current.cloud.lock().unwrap().homes > 0);
                assert_eq!(current.cloud.lock().unwrap().tokens, 0);
            })
            .await;
    }));
}

#[test]
fn token_deadline_is_polled_while_initial_mijia_directory_is_pending() {
    let rig = Rig::new(true, &["one"]);
    rig.clock
        .automatic
        .store(false, std::sync::atomic::Ordering::Relaxed);
    token_deadlines(&rig, 5, 20);
    let (_home_release, home) = flume::bounded(1);
    let (_cert_release, certificate) = flume::bounded(1);
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.home_gate = Some(home);
        cloud.cert_gate = Some(certificate);
    }
    block_on(rig.run(|c| async move {
        until(|| {
            let cloud = c.cloud.lock().unwrap();
            cloud.homes == 1 && cloud.certs == 1
        })
        .await;
        c.clock.advance(Duration::from_secs(4));
        async_io::Timer::after(Duration::from_millis(20)).await;
        assert_eq!(c.cloud.lock().unwrap().tokens, 0);
        c.clock.advance(Duration::from_secs(1));
        until(|| {
            c.context
                .credentials
                .borrow()
                .record()
                .unwrap()
                .ha
                .authorization
                .tokens
                .access_token
                == "fresh-access"
        })
        .await;
        assert_eq!(c.cloud.lock().unwrap().tokens, 1);
        assert_eq!(
            c.context
                .credentials
                .borrow()
                .record()
                .unwrap()
                .ha
                .authorization
                .tokens
                .access_token,
            "fresh-access"
        );
    }));
}

#[test]
fn saved_refresh_is_adopted_before_slow_directory_and_certificate_finish() {
    let (rig, listener, peer) = with_cloud_peer(&["one"]);
    rig.clock
        .automatic
        .store(false, std::sync::atomic::Ordering::Relaxed);
    token_deadlines(&rig, 5, 20);
    let (cert_release, certificate) = flume::bounded(1);
    let (new_cert_release, new_certificate) = flume::bounded(1);
    rig.cloud.lock().unwrap().cert_gate = Some(certificate);
    let (_home_release, home) = flume::bounded(1);
    block_on(rig.run(|c| async move {
        future::or(peer.run(&listener), async {
            until(|| cloud_active(&c, "one") && c.cloud.lock().unwrap().certs == 1).await;
            c.cloud.lock().unwrap().home_gate = Some(home);
            c.context.request_sync();
            until(|| c.cloud.lock().unwrap().homes >= 2).await;
            c.cloud.lock().unwrap().cert_gate = Some(new_certificate);
            c.clock.advance(Duration::from_secs(5));
            until(|| peer.state.borrow().connects.len() == 2 && cloud_active(&c, "one")).await;
            assert_eq!(peer.state.borrow().connects[1], "fresh-access");
            assert_eq!(
                c.context
                    .credentials
                    .borrow()
                    .record()
                    .unwrap()
                    .ha
                    .authorization
                    .tokens
                    .access_token,
                "fresh-access"
            );
            function(&c.context, "one", "outlet:2")
                .execute(Command::On)
                .await
                .unwrap();
            assert!(
                c.cloud.lock().unwrap().property_writes[0]
                    .4
                    .contains("serviceToken=device-session")
            );
            assert_eq!(c.cloud.lock().unwrap().tokens, 1);
            c.cloud.lock().unwrap().cert_gate = None;
            cert_release.send(()).unwrap();
            new_cert_release.send(()).unwrap();
            until(|| {
                matches!(
                    c.context.diagnostics().auth.certificate_update,
                    crate::xiaomi::auth::CertificateUpdate::Updated
                )
            })
            .await;
            assert_eq!(
                c.store
                    .xiaomi()
                    .load()
                    .unwrap()
                    .unwrap()
                    .ha
                    .authorization
                    .tokens
                    .access_token,
                "fresh-access"
            );
            assert_eq!(
                c.context
                    .credentials
                    .borrow()
                    .record()
                    .unwrap()
                    .ha
                    .authorization
                    .tokens
                    .access_token,
                "fresh-access"
            );
            assert_eq!(c.cloud.lock().unwrap().property_writes.len(), 1);
        })
        .await;
    }));
}

#[test]
fn expiry_wakes_availability_and_closes_mqtt_while_refresh_hangs() {
    let (rig, listener, peer) = with_cloud_peer(&["one"]);
    rig.clock
        .automatic
        .store(false, std::sync::atomic::Ordering::Relaxed);
    token_deadlines(&rig, 5, 10);
    let (release, token) = flume::bounded(1);
    rig.cloud.lock().unwrap().token_gate = Some(token);
    block_on(rig.run(|c| async move {
        future::or(peer.run(&listener), async {
            until(|| cloud_active(&c, "one")).await;
            let one = function(&c.context, "one", "outlet:2");
            c.clock.advance(Duration::from_secs(5));
            until(|| c.cloud.lock().unwrap().tokens == 1).await;
            assert_eq!(
                format!("{:?}", c.context.diagnostics().auth.ha),
                "Refreshing"
            );
            assert_eq!(one.snapshot().availability, Availability::Available);
            one.execute(Command::On).await.unwrap();
            c.clock.advance(Duration::from_secs(5));
            until(|| c.context.notifications.borrow().is_none()).await;
            assert_eq!(one.snapshot().availability, Availability::Available);
            assert!(!c.context.token_usable());
            one.execute(Command::Off).await.unwrap();
            assert_eq!(c.cloud.lock().unwrap().property_writes.len(), 2);
            assert_eq!(peer.state.borrow().connects.len(), 1);
            c.cloud.lock().unwrap().token_gate = None;
            release.send(()).unwrap();
            until(|| cloud_active(&c, "one")).await;
            assert_eq!(peer.state.borrow().connects[1], "fresh-access");
            assert_eq!(one.snapshot().availability, Availability::Available);
            assert_eq!(c.cloud.lock().unwrap().property_writes.len(), 2);
        })
        .await;
    }));
}

#[test]
fn temporary_refresh_failure_keeps_valid_old_token_but_expiry_keeps_local_control() {
    let rig = Rig::new(true, &["one"]);
    rig.cloud.lock().unwrap().online.insert("one".into());
    block_on(rig.run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(Rc::new(Peer::default()), |peer| async move {
            initialized(control, &peer, &["one"]).await;
            control
                .clock
                .automatic
                .store(false, std::sync::atomic::Ordering::Relaxed);
            let mut record = control.store.xiaomi().load().unwrap().unwrap();
            record.ha.authorization.tokens.refresh_at = control.clock.wall() + 5;
            record.ha.authorization.tokens.expires_at = control.clock.wall() + 10;
            control.store.xiaomi().fixture_replace(&record).unwrap();
            control.context.adopt_credentials(record);
            control.cloud.lock().unwrap().token_status = 503;
            control.clock.advance(Duration::from_secs(5));
            until(|| control.cloud.lock().unwrap().tokens == 1).await;
            until(|| {
                matches!(
                    control.context.diagnostics().auth.ha,
                    crate::xiaomi::auth::AuthenticationState::Unavailable(_)
                )
            })
            .await;
            assert!(control.context.cloud_candidate("one", true).is_ok());
            assert!(!control.context.credentials.borrow().http_paused());
            control.clock.advance(Duration::from_secs(5));
            until(|| !control.context.token_usable()).await;
            assert!(matches!(
                control.context.control_path("one"),
                Some(super::super::routing::ControlPath::Local(_))
            ));
            function(&control.context, "one", "outlet:2")
                .execute(Command::On)
                .await
                .unwrap();
            assert_eq!(peer.count("rpcReq", "one", 2), 1);
            assert!(control.cloud.lock().unwrap().property_writes.is_empty());
        })
        .await;
    }));
}

#[test]
fn explicit_signin_failure_pauses_cloud_but_preserves_cached_local_commands() {
    let rig = Rig::new(true, &["one"]);
    rig.cloud.lock().unwrap().status = 401;
    block_on(rig.run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        let control = &c;
        c.peer(peer, |peer| async move {
            initialized(control, &peer, &["one"]).await;
            until(|| {
                matches!(
                    control.context.diagnostics().auth.ha,
                    crate::xiaomi::auth::AuthenticationState::SignInRequired(_)
                )
            })
            .await;
            let before = control.cloud.lock().unwrap().homes;
            control.cloud.lock().unwrap().status = 200;
            control.clock.advance(Duration::from_secs(600));
            peer.list_changed(&["one"]);
            function(&control.context, "one", "outlet:2")
                .execute(Command::On)
                .await
                .unwrap();
            for _ in 0..10 {
                future::yield_now().await;
            }
            assert_eq!(control.cloud.lock().unwrap().homes, before);
            assert!(matches!(
                control.context.diagnostics().auth.ha,
                crate::xiaomi::auth::AuthenticationState::SignInRequired(_)
            ));
        })
        .await;
    }));
}

#[test]
fn temporary_refresh_failure_and_late_unauthorized_do_not_stop_ha() {
    let rig = Rig::new(true, &["one"]);
    rig.enable_recovery();
    rig.clock
        .automatic
        .store(false, std::sync::atomic::Ordering::Relaxed);
    rig.cloud.lock().unwrap().token_lifetime = 10;
    block_on(rig.run(|c| async move {
        until(|| c.context.token_usable()).await;
        let old = c.context.ha_key().unwrap();
        c.context
            .token_rejected(
                &old,
                crate::xiaomi::auth::FailureReason::CloudNotification(
                    crate::xiaomi::cloud::NotificationError::Unauthorized,
                ),
            )
            .unwrap();
        until(|| c.cloud.lock().unwrap().tokens == 1 && c.context.token_usable()).await;
        let current = c.context.ha_key().unwrap();
        c.cloud.lock().unwrap().token_status = 503;
        c.clock.advance(Duration::from_secs(7));
        until(|| {
            c.cloud.lock().unwrap().tokens == 2
                && matches!(
                    c.context.diagnostics().auth.ha,
                    crate::xiaomi::auth::AuthenticationState::Unavailable(_)
                )
        })
        .await;
        c.context
            .token_rejected(
                &current,
                crate::xiaomi::auth::FailureReason::CloudNotification(
                    crate::xiaomi::cloud::NotificationError::Unauthorized,
                ),
            )
            .unwrap();
        assert!(!c.store.xiaomi().load().unwrap().unwrap().ha.stopped);
        let record = c.store.xiaomi().load().unwrap().unwrap();
        c.store
            .xiaomi()
            .update_recovery(&record.recovery.credentials, &record.mijia)
            .unwrap();
        c.context.reload_credentials();
        c.cloud.lock().unwrap().token_status = 200;
        c.clock.advance(Duration::from_millis(999));
        async_io::Timer::after(Duration::from_millis(20)).await;
        assert_eq!(c.cloud.lock().unwrap().tokens, 2);
        c.clock.advance(Duration::from_millis(1));
        until(|| c.cloud.lock().unwrap().tokens == 3 && c.context.token_usable()).await;
        assert!(!c.store.xiaomi().load().unwrap().unwrap().ha.stopped);
    }));
}

#[test]
fn stopped_ha_stays_stopped_after_adapter_restart_while_mijia_and_hub_work() {
    let mut rig = Rig::new(true, &["one"]);
    rig.cloud.lock().unwrap().online.insert("one".into());
    rig.store.xiaomi().stop_ha_refresh().unwrap();
    (rig.source, rig.ready) = rig.reopen_adapter();
    block_on(rig.run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(Rc::new(Peer::default()), |peer| async move {
            initialized(control, &peer, &["one"]).await;
            assert!(matches!(
                control.context.diagnostics().auth.ha,
                crate::xiaomi::auth::AuthenticationState::SignInRequired(_)
            ));
            assert!(!control.context.token_usable());
            until(|| control.context.cloud_candidate("one", true).is_ok()).await;
            function(&control.context, "one", "outlet:2")
                .execute(Command::On)
                .await
                .unwrap();
            assert_eq!(peer.count("rpcReq", "one", 2), 1);
            let homes = control.cloud.lock().unwrap().homes;
            control.clock.advance(Duration::from_secs(600));
            until(|| control.cloud.lock().unwrap().homes > homes).await;
            assert_eq!(control.cloud.lock().unwrap().ha_homes, 0);
            assert_eq!(control.cloud.lock().unwrap().tokens, 0);
            assert_eq!(control.cloud.lock().unwrap().certs, 0);
        })
        .await;
    }));
}

#[test]
fn successful_certificate_does_not_hide_failed_or_inflight_token_refresh() {
    for pending in [false, true] {
        let rig = Rig::new(true, &["one"]);
        rig.clock
            .automatic
            .store(false, std::sync::atomic::Ordering::Relaxed);
        token_deadlines(&rig, 0, 100);
        let (certificate_release, certificate_gate) = flume::bounded(1);
        let (token_release, token_gate) = flume::bounded(1);
        {
            let mut cloud = rig.cloud.lock().unwrap();
            cloud.cert_sign_csr = true;
            cloud.cert_gate = Some(certificate_gate);
            cloud.token_status = 503;
            if pending {
                cloud.token_gate = Some(token_gate);
            }
        }
        block_on(rig.run(|c| async move {
            until(|| {
                c.cloud.lock().unwrap().certs == 1
                    && c.cloud.lock().unwrap().tokens == 1
                    && if pending {
                        matches!(
                            c.context.diagnostics().auth.ha,
                            crate::xiaomi::auth::AuthenticationState::Refreshing
                        )
                    } else {
                        matches!(
                            c.context.diagnostics().auth.ha,
                            crate::xiaomi::auth::AuthenticationState::Unavailable(_)
                        )
                    }
            })
            .await;
            let status = c.context.diagnostics().auth.ha.clone();
            certificate_release.send(()).unwrap();
            until(|| {
                matches!(
                    c.context.diagnostics().auth.certificate_update,
                    crate::xiaomi::auth::CertificateUpdate::Updated
                )
            })
            .await;
            assert_eq!(c.context.diagnostics().auth.ha, status);
            assert!(c.context.token_usable());
            if pending {
                token_release.send(()).unwrap();
                until(|| {
                    matches!(
                        c.context.diagnostics().auth.ha,
                        crate::xiaomi::auth::AuthenticationState::Unavailable(_)
                    )
                })
                .await;
            }
        }));
    }
}

#[test]
fn new_ha_generation_does_not_wait_for_old_certificate_response() {
    let rig = Rig::new(true, &["one"]);
    rig.clock
        .automatic
        .store(false, std::sync::atomic::Ordering::Relaxed);
    let (release, gate) = flume::bounded(1);
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.cert_status = 503;
        cloud.cert_gate = Some(gate);
    }
    block_on(rig.run(|c| async move {
        until(|| c.cloud.lock().unwrap().certs == 1).await;
        let record = c.store.xiaomi().load().unwrap().unwrap();
        c.store
            .xiaomi()
            .fixture_update_tokens(&record.ha_authorization().unwrap().tokens)
            .unwrap();
        c.context.reload_credentials();
        {
            let mut cloud = c.cloud.lock().unwrap();
            cloud.cert_gate = None;
            cloud.cert_status = 200;
        }
        until(|| c.cloud.lock().unwrap().certs == 2).await;
        release.send(()).unwrap();
    }));
}
