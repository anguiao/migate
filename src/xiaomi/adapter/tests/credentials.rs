use super::support::{cloud_active, with_cloud_peer};
use super::*;

#[test]
fn ha_identity_is_checked_while_mijia_directory_is_unavailable() {
    let rig = Rig::new(true, &["one"]);
    rig.cloud.lock().unwrap().home_status = 503;
    block_on(rig.run(|c| async move {
        until(|| {
            matches!(
                c.context.status.borrow().auth.ha,
                crate::xiaomi::auth::AuthenticationState::Authenticated
            )
        })
        .await;
        assert!(c.cloud.lock().unwrap().homes > 0);
        assert!(c.context.cloud_candidate("one", true).is_err());
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
        let token = c
            .context
            .live
            .borrow()
            .credentials
            .as_ref()
            .unwrap()
            .ha_authorization()
            .unwrap()
            .tokens
            .access_token
            .clone();
        c.context
            .token_rejected(
                &token,
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
    record
        .ha
        .as_mut()
        .unwrap()
        .authorization
        .as_mut()
        .unwrap()
        .tokens
        .refresh_at = rig.clock.wall() + refresh;
    record
        .ha
        .as_mut()
        .unwrap()
        .authorization
        .as_mut()
        .unwrap()
        .tokens
        .expires_at = rig.clock.wall() + expiry;
    rig.store.xiaomi().replace(&record).unwrap();
    rig.source.context.live.borrow_mut().credentials = Some(record);
}

#[test]
fn a_missing_replacement_certificate_clears_previous_readiness() {
    let rig = Rig::new(true, &["one"]);
    assert!(
        rig.source
            .context
            .status
            .borrow()
            .auth
            .certificate
            .is_some()
    );
    let mut record = rig.store.xiaomi().load().unwrap().unwrap();
    let certificate = record.hub.as_ref().unwrap().certificate_pem.clone();
    record.ha.as_mut().unwrap().authorization = None;
    record.hub.as_mut().unwrap().certificate_pem = None;
    rig.store.xiaomi().replace(&record).unwrap();
    block_on(rig.run(|control| async move {
        until(|| control.context.status.borrow().auth.certificate.is_none()).await;
        assert!(matches!(
            control.context.status.borrow().auth.certificate_update,
            crate::xiaomi::auth::CertificateUpdate::Failed(
                crate::xiaomi::auth::FailureReason::MissingCertificate
            )
        ));
        record.hub.as_mut().unwrap().certificate_pem = certificate;
        control.store.xiaomi().replace(&record).unwrap();
        control.context.changed.notify(usize::MAX);
        until(|| control.context.status.borrow().auth.certificate.is_some()).await;
    }));
}

#[test]
fn background_distinguishes_missing_invalid_and_repaired_certificates() {
    let rig = Rig::new(true, &["one"]);
    let mut record = rig.store.xiaomi().load().unwrap().unwrap();
    let original = record.hub.as_ref().unwrap().certificate_pem.clone();
    record.ha.as_mut().unwrap().authorization = None;
    record.hub.as_mut().unwrap().certificate_pem = None;
    rig.store.xiaomi().replace(&record).unwrap();
    rig.source
        .context
        .adopt_credentials(record.clone())
        .unwrap();
    block_on(rig.run(|control| async move {
        until(|| control.context.status.borrow().auth.certificate.is_none()).await;
        assert_eq!(
            control.context.status.borrow().auth.certificate_update,
            crate::xiaomi::auth::CertificateUpdate::Failed(
                crate::xiaomi::auth::FailureReason::MissingCertificate
            )
        );

        record.hub.as_mut().unwrap().certificate_pem = Some("malformed certificate".into());
        control.store.xiaomi().replace(&record).unwrap();
        control.context.changed.notify(usize::MAX);
        until(|| {
            control
                .context
                .live
                .borrow()
                .credentials
                .as_ref()
                .unwrap()
                .hub
                .as_ref()
                .unwrap()
                .certificate_pem
                .as_deref()
                == Some("malformed certificate")
        })
        .await;
        assert_eq!(
            control.context.status.borrow().auth.certificate_update,
            crate::xiaomi::auth::CertificateUpdate::Failed(
                crate::xiaomi::auth::FailureReason::InvalidCertificate
            )
        );

        record.hub.as_mut().unwrap().certificate_pem = original;
        control.store.xiaomi().replace(&record).unwrap();
        control.context.changed.notify(usize::MAX);
        until(|| control.context.status.borrow().auth.certificate.is_some()).await;
        assert_eq!(
            control.context.status.borrow().auth.certificate_update,
            crate::xiaomi::auth::CertificateUpdate::NotNeeded
        );
        assert!(control.cloud.lock().unwrap().homes > 0);
        assert_eq!(control.cloud.lock().unwrap().certs, 0);
    }));
}

#[test]
fn saved_hub_control_works_without_ha_authorization() {
    let rig = Rig::new(true, &["one"]);
    let mut record = rig.store.xiaomi().load().unwrap().unwrap();
    record.ha.as_mut().unwrap().authorization = None;
    rig.store.xiaomi().replace(&record).unwrap();
    rig.source.context.adopt_credentials(record).unwrap();
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
fn token_deadline_is_polled_while_initial_home_identity_is_pending() {
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
                .as_ref()
                .unwrap()
                .authorization
                .as_ref()
                .unwrap()
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
                .as_ref()
                .unwrap()
                .authorization
                .as_ref()
                .unwrap()
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
    rig.cloud.lock().unwrap().cert_gate = Some(certificate);
    let (_home_release, home) = flume::bounded(1);
    block_on(rig.run(|c| async move {
        future::or(peer.run(&listener), async {
            until(|| cloud_active(&c, "one") && c.cloud.lock().unwrap().certs == 1).await;
            c.cloud.lock().unwrap().home_gate = Some(home);
            c.context.request_sync();
            until(|| c.cloud.lock().unwrap().homes >= 2).await;
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
                    .as_ref()
                    .unwrap()
                    .authorization
                    .as_ref()
                    .unwrap()
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
                    .as_ref()
                    .unwrap()
                    .authorization
                    .as_ref()
                    .unwrap()
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
                    .as_ref()
                    .unwrap()
                    .authorization
                    .as_ref()
                    .unwrap()
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
            record
                .ha
                .as_mut()
                .unwrap()
                .authorization
                .as_mut()
                .unwrap()
                .tokens
                .refresh_at = control.clock.wall() + 5;
            record
                .ha
                .as_mut()
                .unwrap()
                .authorization
                .as_mut()
                .unwrap()
                .tokens
                .expires_at = control.clock.wall() + 10;
            control.store.xiaomi().replace(&record).unwrap();
            control.context.adopt_credentials(record).unwrap();
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
                c.store
                    .xiaomi()
                    .load()
                    .unwrap()
                    .unwrap()
                    .recovery
                    .unwrap()
                    .stop,
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
        let token = c
            .store
            .xiaomi()
            .load()
            .unwrap()
            .unwrap()
            .ha
            .as_ref()
            .unwrap()
            .authorization
            .as_ref()
            .unwrap()
            .tokens
            .access_token
            .clone();
        c.context
            .token_rejected(
                &token,
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
                .as_ref()
                .unwrap()
                .authorization
                .as_ref()
                .unwrap()
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
        c.store.xiaomi().replace(&record).unwrap();
        c.context.adopt_credentials(record).unwrap();
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
                    .unwrap()
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
            control.store.xiaomi().replace(&record).unwrap();
            control.context.adopt_credentials(record).unwrap();
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
                        .unwrap()
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
