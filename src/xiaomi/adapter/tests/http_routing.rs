use super::*;

async fn cloud_ready(c: &Control, dids: &[&str]) {
    until(|| {
        dids.iter().all(|did| {
            c.context
                .registry
                .get(&DeviceKey::new("xiaomi", *did))
                .unwrap()
                .functions()[0]
                .snapshot()
                .availability
                == Availability::Available
        })
    })
    .await;
}

#[test]
fn successful_write_from_replaced_identical_session_does_not_change_new_generation_state() {
    let rig = Rig::new(true, &["one"]);
    rig.cloud.lock().unwrap().online.insert("one".into());
    let (release, gate) = flume::bounded(1);
    block_on(rig.run(|c| async move {
        cloud_ready(&c, &["one"]).await;
        let one = function(&c.context.registry, "one", "outlet:2");
        until(|| one.snapshot().power() == Some(Power::Off)).await;
        c.clock
            .automatic
            .store(false, std::sync::atomic::Ordering::Relaxed);
        c.cloud.lock().unwrap().property_gate = Some(gate);
        let mut command = Box::pin(one.execute(Command::On));
        assert!(poll_once(command.as_mut()).await.is_none());
        until(|| c.cloud.lock().unwrap().property_writes.len() == 1).await;
        let record = c.store.xiaomi().load().unwrap().unwrap();
        c.store.xiaomi().replace(&record).unwrap();
        c.context.adopt_credentials(record).unwrap();
        release.send(()).unwrap();
        command.await.unwrap();
        assert_eq!(one.snapshot().power(), Some(Power::Off));
        assert_eq!(c.cloud.lock().unwrap().property_writes.len(), 1);
    }));
}

#[test]
fn concurrent_unauthorized_writes_share_one_mijia_recovery_without_replay() {
    let rig = Rig::new(true, &["one", "two"]);
    rig.enable_recovery();
    rig.cloud
        .lock()
        .unwrap()
        .online
        .extend(["one".into(), "two".into()]);
    let (release_writes, writes_gate) = flume::bounded(2);
    let (release_restore, restore_gate) = flume::bounded(1);
    block_on(rig.run(|c| async move {
        cloud_ready(&c, &["one", "two"]).await;
        let old_generation = c.store.xiaomi().mijia_generation(&c.uid).unwrap().unwrap();
        let before_tokens = c.cloud.lock().unwrap().tokens;
        {
            let mut cloud = c.cloud.lock().unwrap();
            cloud.property_status = 401;
            cloud.property_gate = Some(writes_gate);
            cloud.restore_gate = Some(restore_gate);
        }
        let one = function(&c.context.registry, "one", "outlet:2");
        let two = function(&c.context.registry, "two", "outlet:2");
        let writes = async {
            let (first, second) =
                future::zip(one.execute(Command::On), two.execute(Command::On)).await;
            assert!(first.is_err());
            assert!(second.is_err());
        };
        let restore = async {
            until(|| c.cloud.lock().unwrap().property_writes.len() == 2).await;
            release_writes.send(()).unwrap();
            release_writes.send(()).unwrap();
            until(|| c.cloud.lock().unwrap().restores == 1).await;
            assert_eq!(c.cloud.lock().unwrap().tokens, before_tokens);
            c.cloud.lock().unwrap().property_status = 200;
            release_restore.send(()).unwrap();
            until(|| {
                c.store
                    .xiaomi()
                    .mijia_generation(&c.uid)
                    .unwrap()
                    .as_deref()
                    != Some(&old_generation)
            })
            .await;
            until(|| c.context.cloud_candidate("one", true).is_ok()).await;
            let cloud = c.cloud.lock().unwrap();
            assert_eq!(cloud.restores, 1);
            assert_eq!(cloud.sts_requests, 1);
            assert_eq!(cloud.property_writes.len(), 2);
        };
        future::zip(writes, restore).await;
    }));
}

#[test]
fn device_http_remains_usable_without_ha_authorization() {
    let rig = Rig::new(true, &["one"]);
    rig.cloud.lock().unwrap().online.insert("one".into());
    let mut record = rig.store.xiaomi().load().unwrap().unwrap();
    record.ha = None;
    rig.store.xiaomi().replace(&record).unwrap();
    rig.source.context.adopt_credentials(record).unwrap();
    block_on(rig.run(|c| async move {
        cloud_ready(&c, &["one"]).await;
        function(&c.context.registry, "one", "outlet:2")
            .execute(Command::On)
            .await
            .unwrap();
        assert_eq!(c.cloud.lock().unwrap().property_writes.len(), 1);
        assert_eq!(c.cloud.lock().unwrap().tokens, 0);
    }));
}

async fn background_property_error(status: u16, code: i64) -> crate::xiaomi::cloud::CloudError {
    let body = json!({"code":0,"result":[{"did":"one","siid":2,"piid":1,"code":code}]}).to_string();
    let (base, _) = dynamic_mock_server(1, move |_| MockResponse::json(status, &body));
    crate::xiaomi::mijia::MijiaClient::for_test(&base, Duration::from_secs(1))
        .read_property_observation(
            &crate::xiaomi::mijia::MijiaSession::new(
                "account".into(),
                "token".into(),
                "c2VjdXJpdHk=".into(),
                "agent".into(),
            )
            .unwrap(),
            "one",
            crate::xiaomi::capability::PropertyBinding { siid: 2, piid: 1 },
        )
        .await
        .unwrap_err()
}

async fn background_transport_error(timeout: bool) -> crate::xiaomi::cloud::CloudError {
    let (base, _) = dynamic_mock_server(1, move |_| {
        if timeout {
            MockResponse::json(200, "{}").delayed(Duration::from_millis(150))
        } else {
            MockResponse::raw("")
        }
    });
    crate::xiaomi::mijia::MijiaClient::for_test(&base, Duration::from_millis(50))
        .read_property_observation(
            &crate::xiaomi::mijia::MijiaSession::new(
                "account".into(),
                "token".into(),
                "c2VjdXJpdHk=".into(),
                "agent".into(),
            )
            .unwrap(),
            "one",
            crate::xiaomi::capability::PropertyBinding { siid: 2, piid: 1 },
        )
        .await
        .unwrap_err()
}

#[test]
fn background_property_http_failure_keeps_both_cloud_controls_available() {
    let rig = Rig::new(true, &["one", "two"]);
    rig.cloud
        .lock()
        .unwrap()
        .online
        .extend(["one".into(), "two".into()]);
    block_on(rig.run(|c| async move {
        cloud_ready(&c, &["one", "two"]).await;
        let token = c.context.mijia_path().unwrap();
        let work = c.context.live.borrow().devices["one"]
            .admitted
            .clone()
            .unwrap();
        let mut errors = Vec::new();
        for status in [408, 429, 500, 503] {
            errors.push(background_property_error(status, 0).await);
        }
        let network = background_transport_error(false).await;
        assert_eq!(
            network.kind(),
            &crate::xiaomi::cloud::CloudErrorKind::Network
        );
        errors.push(network);
        let timeout = background_transport_error(true).await;
        assert_eq!(
            timeout.kind(),
            &crate::xiaomi::cloud::CloudErrorKind::Timeout
        );
        errors.push(timeout);
        for error in errors {
            c.context
                .cloud_property_read_error(&token, &work, &error)
                .unwrap();
            assert!(!c.context.http_paused.get());
            assert!(c.context.cloud_candidate("one", true).is_ok());
            assert!(c.context.cloud_candidate("two", true).is_ok());
        }
        function(&c.context.registry, "one", "outlet:2")
            .execute(Command::On)
            .await
            .unwrap();
        function(&c.context.registry, "two", "outlet:2")
            .execute(Command::On)
            .await
            .unwrap();
        assert_eq!(c.cloud.lock().unwrap().property_writes.len(), 2);
    }));
}

#[test]
fn background_property_device_errors_revoke_only_current_device_access() {
    for code in [-704_042_011, -704_010_000, -704_012_904, -704_012_905] {
        let rig = Rig::new(true, &["one", "two"]);
        rig.cloud
            .lock()
            .unwrap()
            .online
            .extend(["one".into(), "two".into()]);
        block_on(rig.run(|c| async move {
            cloud_ready(&c, &["one", "two"]).await;
            let token = c.context.mijia_path().unwrap();
            let mut stale = token.clone();
            stale.generation = "stale".into();
            let work = c.context.live.borrow().devices["one"]
                .admitted
                .clone()
                .unwrap();
            let error = background_property_error(200, code).await;

            c.context
                .cloud_property_read_error(&stale, &work, &error)
                .unwrap();
            assert!(c.context.cloud_candidate("one", true).is_ok());
            c.context
                .cloud_property_read_error(&token, &work, &error)
                .unwrap();
            assert!(c.context.cloud_candidate("one", true).is_err());
            assert!(c.context.cloud_candidate("two", true).is_ok());
            assert!(!c.context.http_paused.get());
        }));
    }
}

#[test]
fn retired_background_read_keeps_device_access_but_current_token_rejection_is_processed() {
    let rig = Rig::new(true, &["one"]);
    rig.cloud.lock().unwrap().online.insert("one".into());
    block_on(rig.run(|c| async move {
        cloud_ready(&c, &["one"]).await;
        let token = c.context.mijia_path().unwrap();
        let mut stale = token.clone();
        stale.generation = "stale".into();
        let work = c.context.live.borrow().devices["one"]
            .admitted
            .clone()
            .unwrap();
        let offline = background_property_error(200, -704_042_011).await;
        let unauthorized = background_property_error(401, 0).await;
        work.life.close();
        c.context
            .cloud_property_read_error(&token, &work, &offline)
            .unwrap();
        assert!(
            c.context.live.borrow().devices["one"]
                .cloud_access
                .as_ref()
                .unwrap()
                .online
        );
        c.context
            .cloud_property_read_error(&stale, &work, &unauthorized)
            .unwrap();
        assert!(c.context.mijia_rejected.borrow().is_none());
        c.context
            .cloud_property_read_error(&token, &work, &unauthorized)
            .unwrap();
        assert_eq!(
            c.context.mijia_rejected.borrow().as_deref(),
            Some(token.generation.as_str())
        );
    }));
}

#[test]
fn a_closed_outlet_does_not_observe_a_late_cloud_write_receipt() {
    let rig = Rig::new(true, &["one"]);
    rig.cloud.lock().unwrap().online.insert("one".into());
    let (release, gate) = flume::bounded(1);
    block_on(rig.run(|c| async move {
        cloud_ready(&c, &["one"]).await;
        let one = function(&c.context.registry, "one", "outlet:2");
        until(|| one.snapshot().power() == Some(Power::Off)).await;
        c.clock
            .automatic
            .store(false, std::sync::atomic::Ordering::Relaxed);
        c.cloud.lock().unwrap().property_gate = Some(gate);
        let mut command = Box::pin(one.execute(Command::On));
        assert!(poll_once(command.as_mut()).await.is_none());
        until(|| c.cloud.lock().unwrap().property_writes.len() == 1).await;
        let record = c.context.live.borrow().devices["one"]
            .admitted
            .clone()
            .unwrap();
        record.physical().close();
        release.send(()).unwrap();
        assert_eq!(command.await, Err(crate::device::CommandError::Cancelled));
        // The closed future settles first; wait for the late HTTP receipt to
        // pass through Observations::accept_write before checking the state.
        until(|| {
            let status = c
                .context
                .commands
                .snapshot("one", ["outlet:2".to_owned()].into_iter());
            status[0].inflight.is_none()
                && status[0].last.as_ref().is_some_and(|last| {
                    last.result == Some(Err(crate::device::CommandError::Cancelled))
                })
        })
        .await;
        assert_eq!(one.snapshot().power(), Some(Power::Off));
    }));
}

#[test]
fn http_temporary_failure_pauses_the_account_until_a_complete_scoped_sync() {
    let rig = Rig::new(true, &["one", "two"]);
    rig.cloud
        .lock()
        .unwrap()
        .online
        .extend(["one".into(), "two".into()]);
    block_on(rig.run(|c| async move {
        cloud_ready(&c, &["one", "two"]).await;
        c.clock
            .automatic
            .store(false, std::sync::atomic::Ordering::Relaxed);
        let one = function(&c.context.registry, "one", "outlet:2");
        let two = function(&c.context.registry, "two", "outlet:2");
        {
            let mut cloud = c.cloud.lock().unwrap();
            cloud.property_status = 503;
            cloud.info_partial = true;
            cloud.members = vec!["one".into()];
        }
        assert!(one.execute(Command::On).await.is_err());
        assert!(matches!(
            one.snapshot().availability,
            Availability::Unavailable(_)
        ));
        assert!(matches!(
            two.snapshot().availability,
            Availability::Unavailable(_)
        ));
        assert_eq!(c.cloud.lock().unwrap().property_writes.len(), 1);
        let infos = c.cloud.lock().unwrap().infos;
        c.clock.advance(Duration::from_secs(1));
        until(|| c.cloud.lock().unwrap().infos > infos).await;
        until(|| c.context.status.borrow().catalog_problem.is_some()).await;
        assert!(
            c.context
                .registry
                .get(&DeviceKey::new("xiaomi", "two"))
                .is_some()
        );
        assert!(matches!(
            one.snapshot().availability,
            Availability::Unavailable(_)
        ));
        assert_eq!(c.cloud.lock().unwrap().property_writes.len(), 1);
        {
            let mut cloud = c.cloud.lock().unwrap();
            cloud.info_partial = false;
            cloud.property_status = 200;
        }
        c.clock.advance(Duration::from_secs(2));
        c.context.request_sync();
        cloud_ready(&c, &["one"]).await;
        assert!(
            c.context
                .registry
                .get(&DeviceKey::new("xiaomi", "two"))
                .is_none()
        );
        assert_eq!(c.cloud.lock().unwrap().property_writes.len(), 1);
        one.execute(Command::On).await.unwrap();
        assert_eq!(c.cloud.lock().unwrap().property_writes.len(), 2);
    }));
}

#[test]
fn http_device_errors_only_suspend_the_target_and_other_business_errors_keep_it_available() {
    for code in [-704_042_011, -704_010_000, -704_012_904, -9] {
        let rig = Rig::new(true, &["one", "two"]);
        rig.cloud
            .lock()
            .unwrap()
            .online
            .extend(["one".into(), "two".into()]);
        block_on(rig.run(|c| async move {
            cloud_ready(&c, &["one", "two"]).await;
            c.clock
                .automatic
                .store(false, std::sync::atomic::Ordering::Relaxed);
            let one = function(&c.context.registry, "one", "outlet:2");
            let two = function(&c.context.registry, "two", "outlet:2");
            until(|| one.snapshot().power() == Some(Power::Off)).await;
            let token_refreshes = c.cloud.lock().unwrap().tokens;
            c.cloud.lock().unwrap().property_code = code;
            assert!(matches!(
                one.execute(Command::On).await,
                Err(crate::device::CommandError::Rejected(_))
            ));
            assert_eq!(two.snapshot().availability, Availability::Available);
            if code == -9 {
                assert_eq!(one.snapshot().availability, Availability::Available);
            } else {
                assert!(matches!(
                    one.snapshot().availability,
                    Availability::Unavailable(_)
                ));
                assert!(c.context.cloud_candidate("one", true).is_err());
                assert!(matches!(
                    one.execute(Command::Off).await,
                    Err(crate::device::CommandError::Unavailable(_))
                ));
                assert_eq!(c.cloud.lock().unwrap().property_writes.len(), 1);
            }
            assert_eq!(c.cloud.lock().unwrap().tokens, token_refreshes);
            c.cloud.lock().unwrap().property_code = 0;
            two.execute(Command::Off).await.unwrap();
            assert_eq!(c.cloud.lock().unwrap().property_writes.len(), 2);
            assert_eq!(one.snapshot().power(), Some(Power::Off));
        }));
    }
}

#[test]
fn concurrent_http_token_rejections_merge_refresh_and_never_replay_failed_writes() {
    let rig = Rig::new(true, &["one", "two"]);
    rig.enable_recovery();
    rig.cloud
        .lock()
        .unwrap()
        .online
        .extend(["one".into(), "two".into()]);
    let (properties, property_gate) = flume::bounded(2);
    let (token, token_gate) = flume::bounded(1);
    block_on(rig.run(|c| async move {
        cloud_ready(&c, &["one", "two"]).await;
        {
            let mut cloud = c.cloud.lock().unwrap();
            cloud.property_status = 401;
            cloud.property_gate = Some(property_gate);
            cloud.restore_gate = Some(token_gate);
        }
        let one = function(&c.context.registry, "one", "outlet:2");
        let two = function(&c.context.registry, "two", "outlet:2");
        let mut first = Box::pin(one.execute(Command::On));
        let mut second = Box::pin(two.execute(Command::On));
        assert!(poll_once(first.as_mut()).await.is_none());
        assert!(poll_once(second.as_mut()).await.is_none());
        until(|| c.cloud.lock().unwrap().property_writes.len() == 2).await;
        properties.send(()).unwrap();
        properties.send(()).unwrap();
        assert!(first.await.is_err());
        assert!(second.await.is_err());
        until(|| c.cloud.lock().unwrap().restores == 1).await;
        {
            let mut cloud = c.cloud.lock().unwrap();
            cloud.property_status = 200;
            cloud.property_gate = None;
        }
        token.send(()).unwrap();
        cloud_ready(&c, &["one", "two"]).await;
        assert_eq!(c.cloud.lock().unwrap().restores, 1);
        assert_eq!(c.cloud.lock().unwrap().property_writes.len(), 2);
        one.execute(Command::On).await.unwrap();
        assert_eq!(c.cloud.lock().unwrap().property_writes.len(), 3);
        {
            let mut cloud = c.cloud.lock().unwrap();
            cloud.property_status = 401;
            cloud.restore_status = 401;
            cloud.restore_gate = None;
        }
        assert!(two.execute(Command::Off).await.is_err());
        until(|| {
            matches!(
                c.context.status.borrow().auth.mijia,
                crate::xiaomi::auth::AuthenticationState::SignInRequired(_)
            )
        })
        .await;
        assert_eq!(c.cloud.lock().unwrap().restores, 2);
        assert_eq!(c.cloud.lock().unwrap().tokens, 0);
        assert!(
            c.store
                .xiaomi()
                .load()
                .unwrap()
                .unwrap()
                .recovery
                .unwrap()
                .stop
                .is_some()
        );
        assert_eq!(c.cloud.lock().unwrap().property_writes.len(), 4);
    }));
}

#[test]
fn cloud_only_members_publish_offline_and_readd_with_their_saved_identity_and_name() {
    let rig = Rig::new(true, &["one"]);
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.members.push("multi".into());
        cloud.names.insert("multi".into(), "Cloud strip".into());
    }
    block_on(rig.run(|c| async move {
        let key = DeviceKey::new("xiaomi", "multi");
        until(|| c.context.registry.get(&key).is_some()).await;
        assert!(c.context.live.borrow().session.is_none());
        let physical = c.context.registry.get(&key).unwrap();
        assert_eq!(physical.description().label(), "Cloud strip");
        assert_eq!(physical.functions().len(), 3);
        for function in physical.functions() {
            assert_eq!(function.snapshot().power(), Some(Power::Unknown));
            assert!(matches!(
                function.snapshot().availability,
                Availability::Unavailable(_)
            ));
        }
        let usb = function(&c.context.registry, "multi", "outlet:9");
        assert!(matches!(
            usb.execute(Command::On).await,
            Err(crate::device::CommandError::Unavailable(_))
        ));
        assert!(c.cloud.lock().unwrap().property_writes.is_empty());
        assert!(c.cloud.lock().unwrap().property_reads.is_empty());
        c.cloud.lock().unwrap().online.insert("multi".into());
        c.context.request_sync();
        until(|| usb.snapshot().availability == Availability::Available).await;
        usb.execute(Command::On).await.unwrap();
        assert_eq!(c.cloud.lock().unwrap().property_writes.len(), 1);
        c.publication
            .rename(&key, "Controller saved strip")
            .unwrap();
        let before = c
            .store
            .devices()
            .load_configuration()
            .unwrap()
            .device(&key)
            .unwrap()
            .clone();
        c.cloud.lock().unwrap().members = vec!["one".into()];
        c.context.request_sync();
        until(|| c.context.registry.get(&key).is_none()).await;
        assert!(
            !c.store
                .devices()
                .load_configuration()
                .unwrap()
                .derive()
                .published
                .contains(&key)
        );
        assert!(
            c.store
                .devices()
                .load_configuration()
                .unwrap()
                .device(&key)
                .unwrap()
                .native
                .is_none()
        );
        assert_eq!(
            usb.execute(Command::On).await,
            Err(crate::device::CommandError::Cancelled)
        );
        {
            let mut cloud = c.cloud.lock().unwrap();
            cloud.members.push("multi".into());
            cloud.names.insert("multi".into(), "New cloud name".into());
        }
        c.context.request_sync();
        until(|| c.context.registry.get(&key).is_some()).await;
        let after = c
            .store
            .devices()
            .load_configuration()
            .unwrap()
            .device(&key)
            .unwrap()
            .clone();
        assert_eq!(after.unique_id, before.unique_id);
        assert_eq!(after.endpoint, before.endpoint);
        assert_eq!(after.functions, before.functions);
        assert_eq!(after.label, "Controller saved strip");
        assert_eq!(
            c.context.registry.get(&key).unwrap().description().label(),
            "Controller saved strip"
        );
        assert!(c.context.live.borrow().session.is_none());
        assert_eq!(c.cloud.lock().unwrap().property_writes.len(), 1);
    }));
}

#[test]
fn runtime_identity_and_home_scope_failures_retire_cloud_eligibility_and_preserve_local() {
    for mode in 0..3 {
        let rig = Rig::new(true, &["one"]);
        rig.cloud.lock().unwrap().online.insert("one".into());
        block_on(rig.run(|c| async move {
            cloud_ready(&c, &["one"]).await;
            c.announce();
            let control = &c;
            c.peer(Rc::new(Peer::default()), |peer| async move {
                initialized(control, &peer, &["one"]).await;
                {
                    let mut cloud = control.cloud.lock().unwrap();
                    cloud.foreign = mode == 0;
                    cloud.missing_home = mode == 1;
                    cloud.other_home = mode == 2;
                }
                control.context.request_sync();
                until(|| {
                    if mode == 2 {
                        control.context.status.borrow().catalog_problem.is_some()
                    } else {
                        matches!(
                            control.context.status.borrow().auth.mijia,
                            crate::xiaomi::auth::AuthenticationState::Unavailable(_)
                        )
                    }
                })
                .await;
                assert!(matches!(
                    control.context.control_path("one"),
                    Some(super::super::routing::ControlPath::Local(_))
                ));
                let one = function(&control.context.registry, "one", "outlet:2");
                one.execute(Command::On).await.unwrap();
                assert_eq!(peer.count("rpcReq", "one", 2), 1);
                peer.state.borrow_mut().online.insert("one".into(), false);
                peer.list_changed(&["one"]);
                until(|| {
                    control.context.live.borrow().devices["one"]
                        .access
                        .as_ref()
                        .is_some_and(|access| access.as_ref().is_ok_and(|access| !access.online))
                })
                .await;
                assert!(matches!(
                    one.snapshot().availability,
                    Availability::Unavailable(_)
                ));
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
                        .load_configuration()
                        .unwrap()
                        .derive()
                        .published
                        .contains(&DeviceKey::new("xiaomi", "one"))
                );
                assert!(control.cloud.lock().unwrap().property_writes.is_empty());
            })
            .await;
        }));
    }
}

#[test]
fn temporary_directory_failures_pause_existing_http_candidates_until_sync_recovers() {
    let rig = Rig::new(true, &["one"]);
    rig.cloud.lock().unwrap().online.insert("one".into());
    block_on(rig.run(|c| async move {
        cloud_ready(&c, &["one"]).await;
        // Settle the ordinary startup read before freezing the clock, so the
        // directory failure below is the only HTTP pause under test.
        until(|| {
            function(&c.context.registry, "one", "outlet:2")
                .snapshot()
                .power()
                == Some(Power::Off)
        })
        .await;
        c.clock
            .automatic
            .store(false, std::sync::atomic::Ordering::Relaxed);
        c.cloud.lock().unwrap().status = 503;
        c.context.request_sync();
        until(|| {
            matches!(
                c.context.status.borrow().auth.mijia,
                crate::xiaomi::auth::AuthenticationState::Unavailable(_)
            )
        })
        .await;
        let one = function(&c.context.registry, "one", "outlet:2");
        assert!(matches!(
            one.snapshot().availability,
            Availability::Unavailable(_)
        ));
        assert!(
            c.context
                .registry
                .get(&DeviceKey::new("xiaomi", "one"))
                .is_some()
        );
        c.cloud.lock().unwrap().status = 200;
        c.clock.advance(Duration::from_secs(1));
        cloud_ready(&c, &["one"]).await;
        assert!(c.cloud.lock().unwrap().property_writes.is_empty());
        one.execute(Command::On).await.unwrap();
        assert_eq!(c.cloud.lock().unwrap().property_writes.len(), 1);
    }));
}

#[test]
fn published_devices_restore_cloud_control_without_a_local_gateway() {
    let rig = Rig::new(true, &["one", "multi"]);
    rig.cloud
        .lock()
        .unwrap()
        .online
        .extend(["one".into(), "multi".into()]);
    block_on(rig.run(|c| async move {
        let before = c
            .store
            .devices()
            .load_configuration()
            .unwrap()
            .device(&DeviceKey::new("xiaomi", "one"))
            .unwrap()
            .clone();
        until(|| c.cloud.lock().unwrap().homes > 0).await;
        async_io::Timer::after(Duration::from_millis(100)).await;
        let one = function(&c.context.registry, "one", "outlet:2");
        assert_eq!(one.snapshot().availability, Availability::Available);
        assert_eq!(one.snapshot().power(), Some(Power::Off));
        let (_hold_reads, read_gate) = flume::bounded(2);
        {
            let mut cloud = c.cloud.lock().unwrap();
            cloud
                .read_gates
                .insert(("one".into(), 2, 1), read_gate.clone());
            cloud.read_gates.insert(("multi".into(), 9, 1), read_gate);
        }
        one.execute(Command::On).await.unwrap();
        let usb = function(&c.context.registry, "multi", "outlet:9");
        usb.execute(Command::Off).await.unwrap();
        assert_eq!(one.snapshot().power(), Some(Power::On));
        assert_eq!(usb.snapshot().power(), Some(Power::Off));
        assert_eq!(c.cloud.lock().unwrap().property_writes.len(), 2);
        assert_eq!(
            c.store
                .devices()
                .load_configuration()
                .unwrap()
                .device(&DeviceKey::new("xiaomi", "one"))
                .unwrap(),
            &before
        );
    }));
}

#[test]
fn old_cloud_write_rejections_do_not_revoke_a_replacement_native_instance() {
    for code in [-704_042_011, -704_010_000, -704_012_904] {
        let rig = Rig::light("xiaomi.light.ceil04");
        rig.cloud.lock().unwrap().online.insert("one".into());
        let (release, gate) = flume::bounded(1);
        block_on(rig.run(|c| async move {
            cloud_ready(&c, &["one"]).await;
            let one = function(&c.context.registry, "one", "light:2");
            until(|| one.snapshot().power() == Some(Power::Off)).await;
            c.clock
                .automatic
                .store(false, std::sync::atomic::Ordering::Relaxed);
            {
                let mut cloud = c.cloud.lock().unwrap();
                cloud.property_code = code;
                cloud.property_gate = Some(gate);
            }
            let mut old = Box::pin(one.execute(Command::On));
            assert!(poll_once(old.as_mut()).await.is_none());
            until(|| c.cloud.lock().unwrap().numeric_writes.len() == 1).await;
            let old_physical = c
                .context
                .registry
                .get(&DeviceKey::new("xiaomi", "one"))
                .unwrap();
            let mut changed = c.context.live.borrow().devices["one"]
                .admitted
                .as_ref()
                .unwrap()
                .native
                .clone();
            changed = crate::xiaomi::capability::NativeDescription::new(
                changed.model().to_owned(),
                changed.instance_type().to_owned(),
                super::support::device_with_temperature_grid(
                    changed.device(),
                    crate::device::NumericRange::new(2700, 6500, 1).unwrap(),
                ),
            )
            .unwrap();
            {
                let mut live = c.context.live.borrow_mut();
                let slot = live.devices.get_mut("one").unwrap();
                slot.candidate.as_mut().unwrap().native = Some(changed);
            }
            {
                let mut cloud = c.cloud.lock().unwrap();
                cloud.property_code = 0;
                cloud.property_gate = None;
                cloud.read_values.insert(("one".into(), 2, 1), true);
                cloud.raw_values.insert(("one".into(), 2, 3), json!(6000));
            }
            c.context.reconcile("one").unwrap();
            let current_physical = loop {
                let current = c
                    .context
                    .registry
                    .get(&DeviceKey::new("xiaomi", "one"))
                    .unwrap();
                if !old_physical.same_instance(&current) {
                    break current;
                }
                future::yield_now().await;
            };
            let current = current_physical.function("light:2").unwrap();
            // The replacement's cloud read waits for the device's next round.
            c.clock.advance(Duration::from_millis(201));
            until(|| current.snapshot().power() == Some(Power::On)).await;
            assert!(c.context.cloud_candidate("one", true).is_ok());
            // The sent light receipt is still owned by the original request;
            // release it only after the replacement instance is serving.
            release.send(()).unwrap();
            assert_eq!(
                old.await,
                Err(crate::device::CommandError::Rejected(format!(
                    "cloud code {code}"
                )))
            );
            until(|| {
                let status = c
                    .context
                    .commands
                    .snapshot("one", ["light:2".to_owned()].into_iter());
                status[0].inflight.is_none()
                    && status[0]
                        .last
                        .as_ref()
                        .is_some_and(|last| last.result.is_some())
            })
            .await;
            assert!(c.context.cloud_candidate("one", true).is_ok());
            assert_eq!(current.snapshot().availability, Availability::Available);
            assert_eq!(current.snapshot().power(), Some(Power::On));
            assert_eq!(c.cloud.lock().unwrap().numeric_writes.len(), 1);
            current.execute(Command::Off).await.unwrap();
            let writes = &c.cloud.lock().unwrap().numeric_writes;
            assert_eq!(writes.len(), 2);
            assert_eq!(writes[1].0[0]["siid"], 2);
            assert_eq!(writes[1].0[0]["piid"], 1);
            assert_eq!(writes[1].0[0]["value"], json!(false));
        }));
    }
}
