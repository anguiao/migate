use super::*;

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
                    c.context.diagnostics().auth.mijia,
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
                c.context.diagnostics().auth.ha,
                crate::xiaomi::auth::AuthenticationState::SignInRequired(_)
            ));
        }));
    }
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
            function(&c.context, "one", "outlet:2")
                .execute(Command::On)
                .await
                .is_err()
        );
        assert!(c.context.credentials.borrow().http_paused());
        old_release.send(()).unwrap();
        async_io::Timer::after(Duration::from_millis(30)).await;
        assert!(c.context.credentials.borrow().http_paused());
        assert!(
            c.context
                .physical(&DeviceKey::new("xiaomi", "two"))
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
            function(&control.context, "one", "outlet:2")
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
            assert!(
                control
                    .context
                    .credentials
                    .borrow()
                    .mijia_rejected()
                    .is_none()
            );
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
                        control.context.diagnostics().auth.mijia,
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
                function(&control.context, "one", "outlet:2")
                    .execute(Command::On)
                    .await
                    .unwrap();
                for _ in 0..10 {
                    future::yield_now().await;
                }
                assert_eq!(control.cloud.lock().unwrap().restores, 1);
                assert!(!matches!(
                    control.context.diagnostics().auth.ha,
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
                        control.context.diagnostics().auth.mijia,
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
            function(&control.context, "one", "outlet:2")
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
