use super::support::{cloud_active, with_cloud_peer};
use super::*;

#[test]
fn checking_credential_snapshots_does_not_query_storage() {
    let rig = Rig::new(true, &["one"]);
    let path = rig.source.context.mijia_path().unwrap();
    let key = rig.source.context.ha_key().unwrap();
    // Make a fresh storage lookup fail after the adapter captured credentials.
    rusqlite::Connection::open(rig.store.path())
        .unwrap()
        .execute(
            "ALTER TABLE xiaomi_account RENAME TO unavailable_account",
            [],
        )
        .unwrap();
    assert!(rig.source.context.current_mijia(&path));
    assert!(rig.source.context.current_ha(&key));
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
                c.context.status.borrow().auth.ha,
                crate::xiaomi::auth::AuthenticationState::Authenticated
            ));
            assert!(c.context.cloud_candidate("one", true).is_err());
            assert_eq!(c.cloud.lock().unwrap().ha_homes, 0);
            c.clock.advance(Duration::from_secs(5));
            until(|| {
                c.cloud.lock().unwrap().tokens == 1
                    && matches!(
                        c.context.status.borrow().auth.ha,
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
                c.context.status.borrow().auth.ha,
                crate::xiaomi::auth::AuthenticationState::Authenticated
            )
        })
        .await;
        async_io::Timer::after(Duration::from_millis(20)).await;
        assert_eq!(c.cloud.lock().unwrap().homes, before);
    }));
}

fn token_deadlines(rig: &Rig, refresh: i64, expiry: i64) {
    let mut record = rig.store.xiaomi().load().unwrap().unwrap();
    record.ha.authorization.tokens.refresh_at = rig.clock.wall() + refresh;
    record.ha.authorization.tokens.expires_at = rig.clock.wall() + expiry;
    rig.store.xiaomi().fixture_replace(&record).unwrap();
    rig.source.context.adopt_credentials(record);
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
                function(&current.context.registry, "one", "outlet:2")
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
        until(|| c.cloud.lock().unwrap().homes == 1).await;
        c.clock.advance(Duration::from_secs(4));
        async_io::Timer::after(Duration::from_millis(20)).await;
        assert_eq!(c.cloud.lock().unwrap().tokens, 0);
        c.clock.advance(Duration::from_secs(1));
        until(|| {
            c.context
                .live
                .borrow()
                .credentials
                .as_ref()
                .unwrap()
                .ha
                .authorization
                .tokens
                .access_token
                == "fresh-access"
        })
        .await;
        assert_eq!(c.cloud.lock().unwrap().tokens, 1);
        assert_eq!(c.cloud.lock().unwrap().certs, 1);
        assert_eq!(
            c.context
                .live
                .borrow()
                .credentials
                .as_ref()
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
fn certificate_failure_does_not_delay_cloud_directory_or_control() {
    let rig = Rig::new(true, &["one"]);
    let (_release, certificate) = flume::bounded(1);
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.online.insert("one".into());
        cloud.cert_status = 503;
        cloud.cert_gate = Some(certificate);
    }
    block_on(rig.run(|c| async move {
        until(|| c.cloud.lock().unwrap().certs == 1).await;
        let one = function(&c.context.registry, "one", "outlet:2");
        until(|| one.snapshot().availability == Availability::Available).await;
        one.execute(Command::On).await.unwrap();
        assert_eq!(c.cloud.lock().unwrap().property_writes.len(), 1);
        assert_eq!(c.cloud.lock().unwrap().tokens, 0);
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
                    .live
                    .borrow()
                    .credentials
                    .as_ref()
                    .unwrap()
                    .ha
                    .authorization
                    .tokens
                    .access_token,
                "fresh-access"
            );
            function(&c.context.registry, "one", "outlet:2")
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
                    c.context.status.borrow().auth.certificate_update,
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
                    .live
                    .borrow()
                    .credentials
                    .as_ref()
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
            let one = function(&c.context.registry, "one", "outlet:2");
            c.clock.advance(Duration::from_secs(5));
            until(|| c.cloud.lock().unwrap().tokens == 1).await;
            assert_eq!(
                format!("{:?}", c.context.status.borrow().auth.ha),
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
                    control.context.status.borrow().auth.ha,
                    crate::xiaomi::auth::AuthenticationState::Unavailable(_)
                )
            })
            .await;
            assert!(control.context.cloud_candidate("one", true).is_ok());
            assert!(!control.context.http_paused.get());
            control.clock.advance(Duration::from_secs(5));
            until(|| !control.context.token_usable()).await;
            assert!(matches!(
                control.context.control_path("one"),
                Some(super::super::routing::ControlPath::Local(_))
            ));
            function(&control.context.registry, "one", "outlet:2")
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
fn expired_local_certificate_revokes_only_the_local_candidate() {
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
            let expiry = control
                .context
                .status
                .borrow()
                .auth
                .certificate
                .unwrap()
                .not_after;
            let by = Duration::from_secs((expiry - control.clock.wall()) as u64);
            control.cloud.lock().unwrap().cert_status = 503;
            control.clock.advance(by);
            until(|| control.context.local_candidate("one", false).is_err()).await;
            assert!(control.context.cloud_candidate("one", true).is_ok());
            function(&control.context.registry, "one", "outlet:2")
                .execute(Command::On)
                .await
                .unwrap();
            assert_eq!(peer.count("rpcReq", "one", 2), 0);
            assert_eq!(control.cloud.lock().unwrap().property_writes.len(), 1);
        })
        .await;
    }));
}

#[test]
fn home_and_info_rejection_stop_only_mijia_recovery_after_account_rejection() {
    for home_failure in [true, false] {
        let rig = Rig::new(true, &["one"]);
        rig.enable_recovery();
        rig.cloud.lock().unwrap().online.insert("one".into());
        block_on(rig.run(|c| async move {
            until(|| c.context.cloud_candidate("one", true).is_ok()).await;
            c.clock
                .automatic
                .store(false, std::sync::atomic::Ordering::Relaxed);
            {
                let mut state = c.cloud.lock().unwrap();
                if home_failure {
                    state.home_status = 401;
                } else {
                    state.info_status = 401;
                }
                state.restore_status = 401;
            }
            c.context.request_sync();
            until(|| {
                matches!(
                    c.context.status.borrow().auth.mijia,
                    crate::xiaomi::auth::AuthenticationState::SignInRequired(_)
                )
            })
            .await;
            assert_eq!(c.cloud.lock().unwrap().restores, 1);
            assert_eq!(c.cloud.lock().unwrap().tokens, 0);
            assert_eq!(
                c.store.xiaomi().load().unwrap().unwrap().recovery.stop,
                Some(crate::storage::RecoveryStop::Rejected)
            );
            c.context.request_sync();
            c.clock.advance(Duration::from_secs(30));
            async_io::Timer::after(Duration::from_millis(20)).await;
            assert_eq!(c.cloud.lock().unwrap().restores, 1);
            assert!(!matches!(
                c.context.status.borrow().auth.ha,
                crate::xiaomi::auth::AuthenticationState::SignInRequired(_)
            ));
        }));
    }
}

#[test]
fn late_old_certificate_401_keeps_fresh_auth_and_retries_with_current_credentials() {
    let rig = Rig::new(true, &["one"]);
    rig.cloud.lock().unwrap().online.insert("one".into());
    let (release, gate) = flume::bounded(1);
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.cert_status = 401;
        cloud.cert_gate = Some(gate);
    }
    block_on(rig.run(|c| async move {
        until(|| {
            c.cloud.lock().unwrap().certs == 1 && c.context.cloud_candidate("one", true).is_ok()
        })
        .await;
        c.clock
            .automatic
            .store(false, std::sync::atomic::Ordering::Relaxed);
        let key = c.context.ha_key().unwrap();
        c.context
            .token_rejected(
                &key,
                crate::xiaomi::auth::FailureReason::CloudNotification(
                    crate::xiaomi::cloud::NotificationError::Unauthorized,
                ),
            )
            .unwrap();
        until(|| {
            c.context
                .live
                .borrow()
                .credentials
                .as_ref()
                .unwrap()
                .ha
                .authorization
                .tokens
                .access_token
                == "fresh-access"
        })
        .await;
        {
            let mut cloud = c.cloud.lock().unwrap();
            cloud.cert_status = 200;
            cloud.cert_gate = None;
        }
        release.send(()).unwrap();
        until(|| c.cloud.lock().unwrap().certs == 2).await;
        until(|| {
            matches!(
                c.context.status.borrow().auth.certificate_update,
                crate::xiaomi::auth::CertificateUpdate::Updated
            )
        })
        .await;
        assert_eq!(c.cloud.lock().unwrap().tokens, 1);
        assert!(!matches!(
            *c.context.ha_sync.borrow(),
            super::super::credentials::HaSync::Paused
        ));
        assert!(!matches!(
            c.context.status.borrow().auth.ha,
            crate::xiaomi::auth::AuthenticationState::SignInRequired(_)
        ));
    }));
}

#[test]
fn an_old_directory_round_cannot_confirm_deletion_or_clear_new_http_pause() {
    let rig = Rig::new(true, &["one", "two"]);
    rig.enable_recovery();
    rig.cloud
        .lock()
        .unwrap()
        .online
        .extend(["one".into(), "two".into()]);
    let (old_release, old_gate) = flume::bounded(1);
    let (_new_release, new_gate) = flume::bounded(1);
    block_on(rig.run(|c| async move {
        until(|| {
            c.context.cloud_candidate("one", true).is_ok()
                && c.context.cloud_candidate("two", true).is_ok()
        })
        .await;
        c.clock
            .automatic
            .store(false, std::sync::atomic::Ordering::Relaxed);
        let before = c.cloud.lock().unwrap().infos;
        {
            let mut cloud = c.cloud.lock().unwrap();
            cloud.members = vec!["one".into()];
            cloud.info_gate = Some(old_gate);
        }
        c.context.request_sync();
        until(|| c.cloud.lock().unwrap().infos == before + 1).await;
        c.cloud.lock().unwrap().info_gate = Some(new_gate);
        let record = c.store.xiaomi().load().unwrap().unwrap();
        c.store
            .xiaomi()
            .update_recovery(&record.recovery.credentials, &record.mijia)
            .unwrap();
        c.context.adopt_credentials(record);
        until(|| c.cloud.lock().unwrap().infos == before + 2).await;
        c.cloud.lock().unwrap().property_status = 503;
        assert!(
            function(&c.context.registry, "one", "outlet:2")
                .execute(Command::On)
                .await
                .is_err()
        );
        assert!(c.context.http_paused.get());
        old_release.send(()).unwrap();
        async_io::Timer::after(Duration::from_millis(30)).await;
        assert!(c.context.http_paused.get());
        assert!(
            c.context
                .registry
                .get(&DeviceKey::new("xiaomi", "two"))
                .is_some()
        );
        assert!(
            c.store
                .devices()
                .load_configuration()
                .unwrap()
                .derive()
                .published
                .contains(&DeviceKey::new("xiaomi", "two"))
        );
        assert_eq!(c.cloud.lock().unwrap().property_writes.len(), 1);
        assert_eq!(c.cloud.lock().unwrap().tokens, 0);
    }));
}

#[test]
fn temporary_certificate_renewal_failures_retry_without_interrupting_local_control() {
    let rig = Rig::new(true, &["one"]);
    rig.cloud.lock().unwrap().cert_status = 503;
    block_on(rig.run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        let control = &c;
        c.peer(peer, |peer| async move {
            initialized(control, &peer, &["one"]).await;
            until(|| {
                matches!(
                    control.context.status.borrow().auth.certificate_update,
                    crate::xiaomi::auth::CertificateUpdate::Failed(_)
                )
            })
            .await;
            let before = control.cloud.lock().unwrap().certs;
            function(&control.context.registry, "one", "outlet:2")
                .execute(Command::On)
                .await
                .unwrap();
            control.cloud.lock().unwrap().cert_status = 200;
            until(|| {
                control.cloud.lock().unwrap().certs > before
                    && matches!(
                        control.context.status.borrow().auth.certificate_update,
                        crate::xiaomi::auth::CertificateUpdate::Updated
                    )
            })
            .await;
            assert_eq!(
                function(&control.context.registry, "one", "outlet:2")
                    .snapshot()
                    .availability,
                Availability::Available
            );
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
                    control.context.status.borrow().auth.ha,
                    crate::xiaomi::auth::AuthenticationState::SignInRequired(_)
                )
            })
            .await;
            let before = control.cloud.lock().unwrap().homes;
            control.cloud.lock().unwrap().status = 200;
            control.clock.advance(Duration::from_secs(600));
            peer.list_changed(&["one"]);
            function(&control.context.registry, "one", "outlet:2")
                .execute(Command::On)
                .await
                .unwrap();
            for _ in 0..10 {
                future::yield_now().await;
            }
            assert_eq!(control.cloud.lock().unwrap().homes, before);
            assert!(matches!(
                control.context.status.borrow().auth.ha,
                crate::xiaomi::auth::AuthenticationState::SignInRequired(_)
            ));
        })
        .await;
    }));
}

#[test]
fn candidate_unauthorized_requests_coalesce_into_one_mijia_recovery_and_resume_preparation() {
    let rig = Rig::new(true, &["one"]);
    rig.enable_recovery();
    let (release, gate) = flume::bounded(1);
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.members = vec!["one".into(), "multi".into(), "two".into()];
        cloud.detail_status = 401;
        cloud.restore_gate = Some(gate);
    }
    block_on(rig.run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| control.cloud.lock().unwrap().restores == 1).await;
            control.cloud.lock().unwrap().detail_status = 200;
            release.send(()).unwrap();
            initialized(control, &peer, &["one", "multi", "two"]).await;
            assert_eq!(control.cloud.lock().unwrap().restores, 1);
            assert_eq!(control.cloud.lock().unwrap().tokens, 0);
            assert_eq!(
                control
                    .store
                    .xiaomi()
                    .load()
                    .unwrap()
                    .unwrap()
                    .mijia
                    .service_token(),
                "restored-service"
            );
            function(&control.context.registry, "one", "outlet:2")
                .execute(Command::On)
                .await
                .unwrap();
        })
        .await;
    }));
}

#[test]
fn stale_candidate_401_cannot_pause_identical_replacement_session() {
    let rig = Rig::new(true, &["one"]);
    rig.enable_recovery();
    let (release, gate) = flume::bounded(1);
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.members = vec!["one".into(), "multi".into()];
        cloud.detail_status = 401;
        cloud.detail_gate = Some(gate);
    }
    block_on(rig.run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(Rc::new(Peer::default()), |peer| async move {
            initialized(control, &peer, &["one"]).await;
            until(|| control.cloud.lock().unwrap().details > 0).await;
            {
                let mut cloud = control.cloud.lock().unwrap();
                cloud.detail_status = 200;
                cloud.detail_gate = None;
            }
            let record = control.store.xiaomi().load().unwrap().unwrap();
            control
                .store
                .xiaomi()
                .update_recovery(&record.recovery.credentials, &record.mijia)
                .unwrap();
            control.context.adopt_credentials(record);
            release.send(()).unwrap();
            initialized(control, &peer, &["one", "multi"]).await;
            assert_eq!(control.cloud.lock().unwrap().restores, 0);
            assert_eq!(control.cloud.lock().unwrap().tokens, 0);
            assert!(control.context.mijia_rejected.borrow().is_none());
        })
        .await;
    }));
}

#[test]
fn rejected_or_verification_recovery_preserves_local_access_and_stops_only_mijia() {
    for (response, expected) in [
        (401, crate::storage::RecoveryStop::Rejected),
        (403, crate::storage::RecoveryStop::VerificationRequired),
    ] {
        let rig = Rig::new(true, &["one"]);
        rig.enable_recovery();
        {
            let mut cloud = rig.cloud.lock().unwrap();
            cloud.members = vec!["one".into(), "multi".into()];
            cloud.detail_status = 401;
            cloud.restore_status = response;
        }
        block_on(rig.run(|c| async move {
            c.announce();
            let control = &c;
            c.peer(Rc::new(Peer::default()), |peer| async move {
                initialized(control, &peer, &["one"]).await;
                until(|| {
                    matches!(
                        control.context.status.borrow().auth.mijia,
                        crate::xiaomi::auth::AuthenticationState::SignInRequired(_)
                    )
                })
                .await;
                assert_eq!(control.cloud.lock().unwrap().restores, 1);
                assert_eq!(control.cloud.lock().unwrap().tokens, 0);
                assert_eq!(
                    control
                        .store
                        .xiaomi()
                        .load()
                        .unwrap()
                        .unwrap()
                        .recovery
                        .stop,
                    Some(expected)
                );
                control.clock.advance(Duration::from_secs(600));
                control.context.request_sync();
                function(&control.context.registry, "one", "outlet:2")
                    .execute(Command::On)
                    .await
                    .unwrap();
                for _ in 0..10 {
                    future::yield_now().await;
                }
                assert_eq!(control.cloud.lock().unwrap().restores, 1);
                assert!(!matches!(
                    control.context.status.borrow().auth.ha,
                    crate::xiaomi::auth::AuthenticationState::SignInRequired(_)
                ));
            })
            .await;
        }));
    }
}

#[test]
fn coalesced_candidate_401s_preserve_the_temporary_recovery_backoff() {
    let rig = Rig::new(true, &["one"]);
    rig.enable_recovery();
    block_on(rig.run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(Rc::new(Peer::default()), |peer| async move {
            initialized(control, &peer, &["one"]).await;
            control
                .clock
                .automatic
                .store(false, std::sync::atomic::Ordering::Relaxed);
            let retry_deadline = control.clock.now() + Duration::from_secs(1);
            {
                let mut cloud = control.cloud.lock().unwrap();
                cloud.members = vec!["one".into(), "multi".into(), "two".into()];
                cloud.detail_status = 401;
                cloud.restore_status = 503;
            }
            control.context.request_sync();
            until(|| {
                control.cloud.lock().unwrap().restores == 1
                    && matches!(
                        control.context.status.borrow().auth.mijia,
                        crate::xiaomi::auth::AuthenticationState::Unavailable(_)
                    )
            })
            .await;
            {
                let mut cloud = control.cloud.lock().unwrap();
                cloud.restore_status = 200;
                cloud.detail_status = 200;
            }
            for _ in 0..10 {
                future::yield_now().await;
            }
            assert_eq!(control.cloud.lock().unwrap().restores, 1);
            function(&control.context.registry, "one", "outlet:2")
                .execute(Command::On)
                .await
                .unwrap();
            control.clock.advance(Duration::from_millis(999));
            for _ in 0..10 {
                future::yield_now().await;
            }
            assert_eq!(control.cloud.lock().unwrap().restores, 1);
            advance_to(&control.clock, retry_deadline).await;
            until(|| control.cloud.lock().unwrap().restores == 2).await;
            control
                .clock
                .automatic
                .store(true, std::sync::atomic::Ordering::Relaxed);
            initialized(control, &peer, &["one", "multi", "two"]).await;
            assert_eq!(control.cloud.lock().unwrap().restores, 2);
            assert_eq!(control.cloud.lock().unwrap().tokens, 0);
        })
        .await;
    }));
}

#[test]
fn background_prepares_missing_hub_identity_before_certificate_request() {
    let rig = Rig::new(true, &["one"]);
    let mut record = rig.store.xiaomi().load().unwrap().unwrap();
    record.hub = None;
    rig.store.xiaomi().fixture_replace(&record).unwrap();
    rig.source.context.adopt_credentials(record);
    rig.cloud.lock().unwrap().cert_status = 503;
    rig.clock
        .automatic
        .store(false, std::sync::atomic::Ordering::Relaxed);
    block_on(rig.run(|c| async move {
        async_io::Timer::after(Duration::from_millis(150)).await;
        assert_eq!(c.cloud.lock().unwrap().certs, 1);
        let hub = c.store.xiaomi().load().unwrap().unwrap().hub.unwrap();
        assert!(hub.certificate_pem.is_none());
        assert!(!hub.virtual_did.is_empty());
        assert!(!hub.private_key_pem.is_empty());
    }));
}

#[test]
fn initial_certificate_failure_reuses_the_saved_identity_on_retry() {
    let rig = Rig::new(true, &["one"]);
    let mut record = rig.store.xiaomi().load().unwrap().unwrap();
    record.hub = None;
    rig.store.xiaomi().fixture_replace(&record).unwrap();
    rig.source.context.adopt_credentials(record);
    rig.clock
        .automatic
        .store(false, std::sync::atomic::Ordering::Relaxed);
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.cert_status = 503;
        cloud.cert_sign_csr = true;
    }
    block_on(rig.run(|c| async move {
        until(|| {
            c.cloud.lock().unwrap().certs == 1
                && matches!(
                    c.context.status.borrow().auth.certificate_update,
                    crate::xiaomi::auth::CertificateUpdate::Failed(
                        crate::xiaomi::auth::FailureReason::Cloud(_)
                    )
                )
        })
        .await;
        let identity = c.store.xiaomi().load().unwrap().unwrap().hub.unwrap();
        assert!(identity.certificate_pem.is_none());
        c.cloud.lock().unwrap().cert_status = 200;
        c.clock.advance(Duration::from_secs(1));
        until(|| {
            c.store
                .xiaomi()
                .load()
                .unwrap()
                .unwrap()
                .hub
                .unwrap()
                .certificate_pem
                .is_some()
        })
        .await;
        let saved = c.store.xiaomi().load().unwrap().unwrap().hub.unwrap();
        assert_eq!(saved.virtual_did, identity.virtual_did);
        assert_eq!(saved.private_key_pem, identity.private_key_pem);
        assert_eq!(c.cloud.lock().unwrap().certs, 2);
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
                    c.context.status.borrow().auth.ha,
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
fn late_certificate_rejection_cannot_stop_identical_replacement_generation() {
    let rig = Rig::new(true, &["one"]);
    rig.clock
        .automatic
        .store(false, std::sync::atomic::Ordering::Relaxed);
    let (release, gate) = flume::bounded(1);
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.cert_status = 401;
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
            cloud.cert_status = 200;
            cloud.cert_gate = None;
        }
        release.send(()).unwrap();
        until(|| {
            c.cloud.lock().unwrap().certs == 2
                && matches!(
                    c.context.status.borrow().auth.certificate_update,
                    crate::xiaomi::auth::CertificateUpdate::Updated
                )
        })
        .await;
        assert_eq!(c.cloud.lock().unwrap().tokens, 0);
        assert!(c.context.token_usable());
        assert!(!c.store.xiaomi().load().unwrap().unwrap().ha.stopped);
    }));
}

#[test]
fn stopped_ha_stays_stopped_after_adapter_restart_while_mijia_and_hub_work() {
    let mut rig = Rig::new(true, &["one"]);
    rig.cloud.lock().unwrap().online.insert("one".into());
    rig.store.xiaomi().stop_ha_refresh().unwrap();
    let (mut restarted, ready) = XiaomiAdapter::build(
        rig.store.clone(),
        rig.source.cloud.clone(),
        rig.discovery.clone(),
        rig.clock.clone(),
    )
    .unwrap();
    restarted.test_ca = rig.source.test_ca.clone();
    restarted.request_timeout = rig.source.request_timeout;
    rig.source = restarted;
    rig.ready = ready;
    block_on(rig.run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(Rc::new(Peer::default()), |peer| async move {
            initialized(control, &peer, &["one"]).await;
            assert!(matches!(
                control.context.status.borrow().auth.ha,
                crate::xiaomi::auth::AuthenticationState::SignInRequired(_)
            ));
            assert!(!control.context.token_usable());
            assert!(control.context.cloud_candidate("one", true).is_ok());
            function(&control.context.registry, "one", "outlet:2")
                .execute(Command::On)
                .await
                .unwrap();
            assert_eq!(peer.count("rpcReq", "one", 2), 1);
            control.clock.advance(Duration::from_secs(600));
            async_io::Timer::after(Duration::from_millis(20)).await;
            assert_eq!(control.cloud.lock().unwrap().ha_homes, 0);
            assert_eq!(control.cloud.lock().unwrap().tokens, 0);
            assert_eq!(control.cloud.lock().unwrap().certs, 0);
        })
        .await;
    }));
}

#[test]
fn background_reports_and_repairs_a_damaged_certificate_without_replacing_identity() {
    let rig = Rig::new(true, &["one"]);
    rig.clock
        .automatic
        .store(false, std::sync::atomic::Ordering::Relaxed);
    let (release, gate) = flume::bounded(1);
    let mut record = rig.store.xiaomi().load().unwrap().unwrap();
    let identity = record.hub.as_ref().unwrap().clone();
    record.hub.as_mut().unwrap().certificate_pem = Some("damaged certificate".into());
    rig.store.xiaomi().fixture_replace(&record).unwrap();
    rig.source.context.adopt_credentials(record);
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.cert_sign_csr = true;
        cloud.cert_gate = Some(gate);
    }
    block_on(rig.run(|c| async move {
        until(|| c.cloud.lock().unwrap().certs == 1).await;
        assert_eq!(
            c.context.status.borrow().auth.certificate_update,
            crate::xiaomi::auth::CertificateUpdate::Failed(
                crate::xiaomi::auth::FailureReason::InvalidCertificate
            )
        );
        assert!(c.context.status.borrow().auth.certificate.is_none());
        release.send(()).unwrap();
        until(|| {
            matches!(
                c.context.status.borrow().auth.certificate_update,
                crate::xiaomi::auth::CertificateUpdate::Updated
            )
        })
        .await;
        let repaired = c.store.xiaomi().load().unwrap().unwrap().hub.unwrap();
        assert_eq!(repaired.virtual_did, identity.virtual_did);
        assert_eq!(repaired.private_key_pem, identity.private_key_pem);
        assert!(c.context.status.borrow().auth.certificate.is_some());
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
                            c.context.status.borrow().auth.ha,
                            crate::xiaomi::auth::AuthenticationState::Refreshing
                        )
                    } else {
                        matches!(
                            c.context.status.borrow().auth.ha,
                            crate::xiaomi::auth::AuthenticationState::Unavailable(_)
                        )
                    }
            })
            .await;
            let status = c.context.status.borrow().auth.ha.clone();
            certificate_release.send(()).unwrap();
            until(|| {
                matches!(
                    c.context.status.borrow().auth.certificate_update,
                    crate::xiaomi::auth::CertificateUpdate::Updated
                )
            })
            .await;
            assert_eq!(c.context.status.borrow().auth.ha, status);
            assert!(c.context.token_usable());
            if pending {
                token_release.send(()).unwrap();
                until(|| {
                    matches!(
                        c.context.status.borrow().auth.ha,
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
