use super::notifications::{cloud_active, with_cloud_peer};
use super::*;

fn token_deadlines(rig: &Rig, refresh: i64, expiry: i64) {
    let mut record = rig.store.xiaomi().load().unwrap().unwrap();
    record.tokens.refresh_at = rig.clock.wall() + refresh;
    record.tokens.expires_at = rig.clock.wall() + expiry;
    rig.store.xiaomi().replace(&record).unwrap();
    rig.source.context.live.borrow_mut().credentials = Some(record);
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
                .tokens
                .access_token
                == "fresh-access"
        })
        .await;
        assert_eq!(c.cloud.lock().unwrap().tokens, 1);
        assert_eq!(c.cloud.lock().unwrap().certs, 0);
        assert_eq!(
            c.context
                .live
                .borrow()
                .credentials
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
        let one = function(&c.context.registry, "one", "main");
        assert_eq!(one.snapshot().availability, Availability::Available);
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
                    .tokens
                    .access_token,
                "fresh-access"
            );
            function(&c.context.registry, "one", "main")
                .execute(Command::On)
                .await
                .unwrap();
            assert!(
                c.cloud.lock().unwrap().property_writes[0]
                    .4
                    .contains("fresh-access")
            );
            assert_eq!(c.cloud.lock().unwrap().tokens, 1);
            c.cloud.lock().unwrap().cert_gate = None;
            cert_release.send(()).unwrap();
            until(|| {
                matches!(
                    c.context.status.borrow().authentication.certificate_update,
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
            let one = function(&c.context.registry, "one", "main");
            c.clock.advance(Duration::from_secs(5));
            until(|| c.cloud.lock().unwrap().tokens == 1).await;
            assert_eq!(one.snapshot().availability, Availability::Available);
            one.execute(Command::On).await.unwrap();
            c.clock.advance(Duration::from_secs(5));
            until(|| c.context.notifications.borrow().is_none()).await;
            assert!(matches!(
                one.snapshot().availability,
                Availability::Unavailable(_)
            ));
            assert!(!c.context.token_usable());
            assert!(one.execute(Command::Off).await.is_err());
            assert_eq!(c.cloud.lock().unwrap().property_writes.len(), 1);
            assert_eq!(peer.state.borrow().connects.len(), 1);
            c.cloud.lock().unwrap().token_gate = None;
            release.send(()).unwrap();
            until(|| cloud_active(&c, "one")).await;
            assert_eq!(peer.state.borrow().connects[1], "fresh-access");
            assert_eq!(one.snapshot().availability, Availability::Available);
            assert_eq!(c.cloud.lock().unwrap().property_writes.len(), 1);
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
            record.tokens.refresh_at = control.clock.wall() + 5;
            record.tokens.expires_at = control.clock.wall() + 10;
            control.store.xiaomi().replace(&record).unwrap();
            control.context.adopt_credentials(record);
            control.cloud.lock().unwrap().token_status = 503;
            control.clock.advance(Duration::from_secs(5));
            until(|| control.cloud.lock().unwrap().tokens == 1).await;
            until(|| {
                matches!(
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
            assert!(control.context.cloud_candidate("one", true));
            assert!(!control.context.http_paused.get());
            control.clock.advance(Duration::from_secs(5));
            until(|| !control.context.token_usable()).await;
            assert!(matches!(
                control.context.control_path("one"),
                Some(super::super::routing::ControlPath::Local(_))
            ));
            function(&control.context.registry, "one", "main")
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
                .authentication
                .certificate
                .unwrap()
                .not_after;
            let by = Duration::from_secs((expiry - control.clock.wall()) as u64);
            control.cloud.lock().unwrap().cert_status = 503;
            control.clock.advance(by);
            until(|| control.context.local_candidate("one", false).is_none()).await;
            assert!(control.context.cloud_candidate("one", true));
            function(&control.context.registry, "one", "main")
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
fn home_and_info_rejection_of_the_fresh_token_keep_the_login_prompt() {
    for home_failure in [true, false] {
        let rig = Rig::new(true, &["one"]);
        rig.cloud.lock().unwrap().online.insert("one".into());
        block_on(rig.run(|c| async move {
            until(|| c.context.cloud_candidate("one", true)).await;
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
            }
            c.context.request_sync();
            until(|| c.cloud.lock().unwrap().tokens == 1).await;
            c.clock.advance(Duration::from_secs(1));
            until(|| {
                matches!(
                    *c.context.cloud_sync.borrow(),
                    super::super::cloud::CloudSync::Paused
                )
            })
            .await;
            assert!(matches!(
                c.context.status.borrow().authentication.authentication,
                crate::xiaomi::auth::AuthenticationState::SignInRequired(_)
            ));
            c.context.request_sync();
            c.clock.advance(Duration::from_secs(30));
            async_io::Timer::after(Duration::from_millis(20)).await;
            assert_eq!(c.cloud.lock().unwrap().tokens, 1);
            assert!(matches!(
                c.context.status.borrow().authentication.authentication,
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
        until(|| c.cloud.lock().unwrap().certs == 1 && c.context.cloud_candidate("one", true))
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
            .tokens
            .access_token;
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
                c.context.status.borrow().authentication.certificate_update,
                crate::xiaomi::auth::CertificateUpdate::Updated
            )
        })
        .await;
        assert_eq!(c.cloud.lock().unwrap().tokens, 1);
        assert!(!matches!(
            *c.context.cloud_sync.borrow(),
            super::super::cloud::CloudSync::Paused
        ));
        assert!(!matches!(
            c.context.status.borrow().authentication.authentication,
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
        until(|| c.context.cloud_candidate("one", true) && c.context.cloud_candidate("two", true))
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
        let token = c
            .context
            .live
            .borrow()
            .credentials
            .as_ref()
            .unwrap()
            .tokens
            .access_token
            .clone();
        c.cloud.lock().unwrap().info_gate = Some(new_gate);
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
                .tokens
                .access_token
                == "fresh-access"
        })
        .await;
        until(|| c.cloud.lock().unwrap().infos == before + 2).await;
        c.cloud.lock().unwrap().property_status = 503;
        assert!(
            function(&c.context.registry, "one", "main")
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
                .load_published(&DeviceKey::new("xiaomi", "two"))
                .unwrap()
                .is_some()
        );
        assert_eq!(c.cloud.lock().unwrap().property_writes.len(), 1);
        assert_eq!(c.cloud.lock().unwrap().tokens, 1);
    }));
}
