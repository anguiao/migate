use super::*;

#[test]
fn replacing_a_binding_clears_its_previous_subscription_failure() {
    block_on(Rig::new(true, &["one"]).run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        peer.state
            .borrow_mut()
            .sub_reject
            .insert(("one".into(), 2, 1));
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| {
                control
                    .context
                    .status
                    .borrow()
                    .pending
                    .iter()
                    .any(|pending| pending.did == "one" && pending.reason.contains("subscribe 2.1"))
            })
            .await;
            let mut replacement = control.context.live.borrow().devices["one"]
                .admitted
                .as_ref()
                .unwrap()
                .cached
                .borrow()
                .clone();
            replacement.device = {
                let function = replacement.device.functions()[0].clone();
                let power = function.power();
                super::support::device_with_functions(
                    &replacement.device,
                    vec![super::support::function_with_power(
                        &function,
                        crate::xiaomi::capability::PropertyBinding {
                            siid: 7,
                            piid: power.piid,
                        },
                    )],
                )
            };
            control
                .context
                .live
                .borrow_mut()
                .devices
                .get_mut("one")
                .unwrap()
                .candidate = Some((replacement, None));
            control.context.reconcile("one").unwrap();
            until(|| {
                control.context.live.borrow().devices["one"]
                    .candidate
                    .is_none()
            })
            .await;
            assert!(
                control
                    .context
                    .status
                    .borrow()
                    .pending
                    .iter()
                    .all(|pending| {
                        pending.did != "one" || !pending.reason.contains("subscribe 2.1")
                    })
            );
            until(|| peer.count("subscribe", "one", 7) == 1).await;
            initialized(control, &peer, &["one"]).await;
        })
        .await;
    }));
}

#[test]
fn compatible_binding_updates_keep_identity_ignore_old_reads_and_use_the_new_command_binding() {
    let rig = Rig::new(true, &["one"]);
    block_on(rig.run(|c| async move {
        c.publication
            .rename(&DeviceKey::new("xiaomi", "one"), "Controller saved name")
            .unwrap();
        c.announce();
        let peer = Rc::new(Peer::default());
        let miot_name = "米家后来修改的厨房插座名称超过三十二字节";
        assert!(miot_name.len() > 32);
        peer.state
            .borrow_mut()
            .names
            .insert("one".into(), miot_name.into());
        peer.state
            .borrow_mut()
            .read_timeout
            .insert(("one".into(), 2, 1));
        peer.state
            .borrow_mut()
            .sub_timeout
            .insert(("one".into(), 2, 1));
        peer.state
            .borrow_mut()
            .values
            .insert(("one".into(), 7, 1), true);
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| peer.count("get", "one", 2) == 1 && peer.count("subscribe", "one", 2) == 1)
                .await;
            let key = DeviceKey::new("xiaomi", "one");
            let before = control
                .store
                .devices()
                .load_configuration()
                .unwrap()
                .device(&key)
                .cloned()
                .unwrap();
            assert_eq!(before.label, "Controller saved name");
            let mut changed = control.context.live.borrow().devices["one"]
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
            control
                .context
                .live
                .borrow_mut()
                .devices
                .get_mut("one")
                .unwrap()
                .candidate = Some((changed, None));
            control.context.reconcile("one").unwrap();
            until(|| {
                peer.count("subscribe", "one", 7) == 1
                    && function(&control.context.registry, "one", "main")
                        .snapshot()
                        .power
                        == Power::On
            })
            .await;
            let old = std::mem::take(&mut peer.state.borrow_mut().held_subs);
            for sub in old {
                peer.send(Packet::SubAck(SubAck::new(
                    sub.pkid,
                    vec![SubscribeReasonCode::QoS2],
                )));
            }
            peer.release("one", 2, json!({"value":false})).await;
            function(&control.context.registry, "one", "main")
                .execute(Command::On)
                .await
                .unwrap();
            assert_eq!(peer.count("rpcReq", "one", 7), 1);
            assert_eq!(
                function(&control.context.registry, "one", "main")
                    .snapshot()
                    .power,
                Power::On
            );
            let after = control
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
            assert_eq!(after.label, before.label);
            assert_eq!(after.configuration_version, before.configuration_version);
            assert_eq!(
                control
                    .store
                    .devices()
                    .load_configuration()
                    .unwrap()
                    .device(&key)
                    .unwrap()
                    .native
                    .as_ref()
                    .unwrap()
                    .device
                    .functions()[0]
                    .power()
                    .siid,
                7
            );
        })
        .await;
    }));
}

#[test]
fn endpoint_exhaustion_is_pending_and_does_not_stop_cached_devices() {
    let rig = Rig::new(true, &["one"]);
    rig.cloud.lock().unwrap().members = vec!["one".into(), "multi".into()];
    let mut configuration = rig.store.devices().load_configuration().unwrap();
    let historical = super::support::device_with_key(&rig.data[0].device, "history");
    configuration
        .devices
        .push(crate::storage::tests::fixtures::identity(
            &historical.physical_description("History"),
            65533,
        ));
    rig.store
        .devices()
        .commit_configuration(&configuration, &[])
        .unwrap();
    block_on(rig.run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        let control = &c;
        c.peer(peer, |peer| async move {
            initialized(control, &peer, &["one"]).await;
            until(|| {
                control
                    .context
                    .status
                    .borrow()
                    .pending
                    .iter()
                    .any(|pending| pending.did == "multi" && pending.reason.contains("exhausted"))
            })
            .await;
            assert_eq!(control.context.registry.list().len(), 1);
            assert!(
                control
                    .store
                    .devices()
                    .load_configuration()
                    .unwrap()
                    .device(&DeviceKey::new("xiaomi", "multi"))
                    .cloned()
                    .is_none()
            );
            function(&control.context.registry, "one", "main")
                .execute(Command::On)
                .await
                .unwrap();
        })
        .await;
    }));
}

#[test]
fn publication_database_failure_keeps_the_complete_cached_configuration() {
    let rig = Rig::new(true, &["one"]);
    let store = rig.store.clone();
    rig.cloud.lock().unwrap().members = vec!["one".into(), "multi".into()];
    rusqlite::Connection::open(store.path()).unwrap().execute_batch("CREATE TRIGGER fail_multi BEFORE UPDATE ON bridge_configuration BEGIN SELECT RAISE(ABORT,'publication failed'); END;").unwrap();
    block_on(rig.run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(Rc::new(Peer::default()), |peer| async move {
            initialized(control, &peer, &["one"]).await;
            until(|| control.publication.failure().is_some()).await;
            assert_eq!(control.context.registry.list().len(), 1);
            assert!(
                control
                    .store
                    .devices()
                    .load_configuration()
                    .unwrap()
                    .device(&DeviceKey::new("xiaomi", "multi"))
                    .cloned()
                    .is_none()
            );
            assert!(
                control
                    .store
                    .devices()
                    .load_configuration()
                    .unwrap()
                    .device(&DeviceKey::new("xiaomi", "multi"))
                    .and_then(|device| device.native.as_ref())
                    .is_none()
            );
            function(&control.context.registry, "one", "main")
                .execute(Command::On)
                .await
                .unwrap();
        })
        .await;
    }));
}
