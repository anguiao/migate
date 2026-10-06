use super::*;

async fn cloud_ready(c: &Control, dids: &[&str]) {
    until(|| {
        dids.iter().all(|did| {
            function(&c.context.registry, did, "main")
                .snapshot()
                .availability
                == Availability::Available
        })
    })
    .await;
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
        let one = function(&c.context.registry, "one", "main");
        let two = function(&c.context.registry, "two", "main");
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
    for code in [-704_042_011, -704_012_904, -9] {
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
            let one = function(&c.context.registry, "one", "main");
            let two = function(&c.context.registry, "two", "main");
            until(|| one.snapshot().power == Power::Off).await;
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
            }
            c.cloud.lock().unwrap().property_code = 0;
            two.execute(Command::Off).await.unwrap();
            assert_eq!(c.cloud.lock().unwrap().property_writes.len(), 2);
            assert_eq!(one.snapshot().power, Power::Off);
        }));
    }
}

#[test]
fn concurrent_http_token_rejections_merge_refresh_and_never_replay_failed_writes() {
    let rig = Rig::new(true, &["one", "two"]);
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
            cloud.token_gate = Some(token_gate);
        }
        let one = function(&c.context.registry, "one", "main");
        let two = function(&c.context.registry, "two", "main");
        let mut first = Box::pin(one.execute(Command::On));
        let mut second = Box::pin(two.execute(Command::On));
        assert!(poll_once(first.as_mut()).await.is_none());
        assert!(poll_once(second.as_mut()).await.is_none());
        until(|| c.cloud.lock().unwrap().property_writes.len() == 2).await;
        properties.send(()).unwrap();
        properties.send(()).unwrap();
        assert!(first.await.is_err());
        assert!(second.await.is_err());
        until(|| c.cloud.lock().unwrap().tokens == 1).await;
        {
            let mut cloud = c.cloud.lock().unwrap();
            cloud.property_status = 200;
            cloud.property_gate = None;
        }
        token.send(()).unwrap();
        cloud_ready(&c, &["one", "two"]).await;
        assert_eq!(c.cloud.lock().unwrap().tokens, 1);
        assert_eq!(c.cloud.lock().unwrap().property_writes.len(), 2);
        one.execute(Command::On).await.unwrap();
        assert_eq!(c.cloud.lock().unwrap().property_writes.len(), 3);
        c.cloud.lock().unwrap().property_status = 401;
        assert!(two.execute(Command::Off).await.is_err());
        until(|| {
            matches!(
                c.context.status.borrow().authentication.authentication,
                crate::xiaomi::auth::AuthenticationState::SignInRequired(_)
            )
        })
        .await;
        assert_eq!(c.cloud.lock().unwrap().tokens, 1);
        assert_eq!(c.cloud.lock().unwrap().property_writes.len(), 4);
    }));
}

#[test]
fn cloud_only_members_cannot_publish_new_or_deleted_devices() {
    let rig = Rig::new(true, &["one"]);
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.members = vec!["one".into(), "two".into()];
        cloud.online.extend(["one".into(), "two".into()]);
    }
    block_on(rig.run(|c| async move {
        cloud_ready(&c, &["one"]).await;
        until(|| {
            c.context
                .live
                .borrow()
                .devices
                .get("two")
                .is_some_and(|slot| slot.candidate.is_some())
        })
        .await;
        assert!(
            c.context
                .registry
                .get(&DeviceKey::new("xiaomi", "two"))
                .is_none()
        );
        assert!(
            c.store
                .devices()
                .load_published(&DeviceKey::new("xiaomi", "two"))
                .unwrap()
                .is_none()
        );
        c.context.remove("one").unwrap();
        c.context.request_sync();
        until(|| {
            c.context
                .live
                .borrow()
                .devices
                .get("one")
                .is_some_and(|slot| slot.candidate.is_some())
        })
        .await;
        assert!(
            c.context
                .registry
                .get(&DeviceKey::new("xiaomi", "one"))
                .is_none()
        );
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
                            control
                                .context
                                .status
                                .borrow()
                                .authentication
                                .authentication,
                            crate::xiaomi::auth::AuthenticationState::Unavailable(_)
                        )
                    }
                })
                .await;
                assert!(matches!(
                    control.context.control_path("one"),
                    Some(super::super::routing::ControlPath::Local(_))
                ));
                let one = function(&control.context.registry, "one", "main");
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
                        .load_published(&DeviceKey::new("xiaomi", "one"))
                        .unwrap()
                        .is_some()
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
        c.clock
            .automatic
            .store(false, std::sync::atomic::Ordering::Relaxed);
        c.cloud.lock().unwrap().status = 503;
        c.context.request_sync();
        until(|| {
            matches!(
                c.context.status.borrow().authentication.authentication,
                crate::xiaomi::auth::AuthenticationState::Unavailable(_)
            )
        })
        .await;
        let one = function(&c.context.registry, "one", "main");
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
