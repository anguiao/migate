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
fn a_closed_outlet_does_not_observe_a_late_cloud_write_receipt() {
    let rig = Rig::new(true, &["one"]);
    rig.cloud.lock().unwrap().online.insert("one".into());
    let (release, gate) = flume::bounded(1);
    block_on(rig.run(|c| async move {
        cloud_ready(&c, &["one"]).await;
        let one = function(&c.context.registry, "one", "main");
        until(|| one.snapshot().power == Power::Off).await;
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
        record.physical.close();
        release.send(()).unwrap();
        assert_eq!(command.await, Err(crate::device::CommandError::Cancelled));
        // The closed future settles first; wait for the late HTTP receipt to
        // pass through Observations::accept_write before checking the state.
        until(|| {
            let status = c
                .context
                .commands
                .snapshot("one", ["main".to_owned()].into_iter());
            status[0].inflight.is_none()
                && status[0].last.as_ref().is_some_and(|last| {
                    last.result == Some(Err(crate::device::CommandError::Cancelled))
                })
        })
        .await;
        assert_eq!(one.snapshot().power, Power::Off);
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
            let one = function(&c.context.registry, "one", "main");
            let two = function(&c.context.registry, "two", "main");
            until(|| one.snapshot().power == Power::Off).await;
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
            assert_eq!(function.snapshot().power, Power::Unknown);
            assert!(matches!(
                function.snapshot().availability,
                Availability::Unavailable(_)
            ));
        }
        let usb = function(&c.context.registry, "multi", "usb");
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
            function(&c.context.registry, "one", "main")
                .snapshot()
                .power
                == Power::Off
        })
        .await;
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
        let one = function(&c.context.registry, "one", "main");
        assert_eq!(one.snapshot().availability, Availability::Available);
        assert_eq!(one.snapshot().power, Power::Off);
        one.execute(Command::On).await.unwrap();
        let usb = function(&c.context.registry, "multi", "usb");
        usb.execute(Command::Off).await.unwrap();
        assert_eq!(one.snapshot().power, Power::On);
        assert_eq!(usb.snapshot().power, Power::Off);
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
fn old_cloud_write_rejections_do_not_revoke_a_replacement_binding() {
    for code in [-704_042_011, -704_010_000, -704_012_904] {
        let rig = Rig::new(true, &["one"]);
        rig.cloud.lock().unwrap().online.insert("one".into());
        let (release, gate) = flume::bounded(1);
        block_on(rig.run(|c| async move {
            cloud_ready(&c, &["one"]).await;
            let one = function(&c.context.registry, "one", "main");
            until(|| one.snapshot().power == Power::Off).await;
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
            until(|| c.cloud.lock().unwrap().property_writes.len() == 1).await;
            let old_physical = c
                .context
                .registry
                .get(&DeviceKey::new("xiaomi", "one"))
                .unwrap();
            let mut changed = c.context.live.borrow().devices["one"]
                .admitted
                .as_ref()
                .unwrap()
                .cached
                .borrow()
                .clone();
            changed.device = {
                let function = changed.device.functions()[0].clone();
                let power = function.power();
                super::support::device_with_functions(
                    &changed.device,
                    vec![super::support::function_with_power(
                        &function,
                        crate::xiaomi::capability::PropertyBinding {
                            siid: 7,
                            piid: power.piid,
                        },
                    )],
                )
            };
            changed.instance_type = "urn:miot-spec-v2:device:outlet:0000A002:cuco-v3:2".into();
            {
                let mut live = c.context.live.borrow_mut();
                let slot = live.devices.get_mut("one").unwrap();
                slot.cloud_access.as_mut().unwrap().instance_type = changed.instance_type.clone();
                slot.candidate = Some((changed, None));
            }
            {
                let mut cloud = c.cloud.lock().unwrap();
                cloud.property_code = 0;
                cloud.property_gate = None;
                cloud.read_values.insert(("one".into(), 7, 1), true);
            }
            c.context.reconcile("one").unwrap();
            until(|| c.context.live.borrow().devices["one"].candidate.is_none()).await;
            let current_physical = c
                .context
                .registry
                .get(&DeviceKey::new("xiaomi", "one"))
                .unwrap();
            assert!(!old_physical.same_instance(&current_physical));
            assert!(matches!(
                one.snapshot().availability,
                Availability::Unavailable(_)
            ));
            let current = current_physical.function("main").unwrap();
            until(|| {
                c.cloud
                    .lock()
                    .unwrap()
                    .property_reads
                    .iter()
                    .any(|(_, siid, _)| *siid == 7)
            })
            .await;
            until(|| current.snapshot().power == Power::On).await;
            assert!(c.context.cloud_candidate("one", true).is_ok());
            release.send(()).unwrap();
            assert_eq!(old.await, Err(crate::device::CommandError::Cancelled));
            until(|| {
                let status = c
                    .context
                    .commands
                    .snapshot("one", ["main".to_owned()].into_iter());
                status[0].inflight.is_none()
                    && status[0]
                        .last
                        .as_ref()
                        .is_some_and(|last| last.result.is_some())
            })
            .await;
            assert!(c.context.cloud_candidate("one", true).is_ok());
            assert_eq!(current.snapshot().availability, Availability::Available);
            assert_eq!(current.snapshot().power, Power::On);
            assert_eq!(c.cloud.lock().unwrap().property_writes.len(), 1);
            current.execute(Command::Off).await.unwrap();
            let writes = &c.cloud.lock().unwrap().property_writes;
            assert_eq!(writes.len(), 2);
            assert_eq!(writes[1].1, 7);
        }));
    }
}
