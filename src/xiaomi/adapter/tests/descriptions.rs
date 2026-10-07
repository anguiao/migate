use super::*;

#[test]
fn compatible_binding_updates_keep_identity_ignore_old_reads_and_use_the_new_command_binding() {
    let rig = Rig::new(true, &["one"]);
    rig.store
        .devices()
        .set_device_label(&DeviceKey::new("xiaomi", "one"), "Controller saved name")
        .unwrap();
    block_on(rig.run(|c| async move {
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
                .load_published(&key)
                .unwrap()
                .unwrap();
            assert_eq!(before.label, "Controller saved name");
            let mut changed = control.context.live.borrow().devices["one"]
                .admitted
                .as_ref()
                .unwrap()
                .cached
                .borrow()
                .clone();
            changed.device.functions[0].power.siid = 7;
            control
                .context
                .live
                .borrow_mut()
                .devices
                .get_mut("one")
                .unwrap()
                .candidate = Some(changed);
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
            assert_eq!(
                control
                    .store
                    .devices()
                    .load_published(&key)
                    .unwrap()
                    .unwrap(),
                before
            );
            assert_eq!(
                control
                    .store
                    .devices()
                    .load_parsed(&control.data[0].uid, "local", &key)
                    .unwrap()
                    .unwrap()
                    .device
                    .functions[0]
                    .power
                    .siid,
                7
            );
        })
        .await;
    }));
}

#[test]
fn incompatible_topology_stays_published_and_unavailable_until_a_compatible_description_returns() {
    block_on(Rig::new(true, &["one", "multi"]).run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        let control = &c;
        c.peer(peer, |peer| async move {
            initialized(control, &peer, &["one", "multi"]).await;
            let original = control.context.live.borrow().devices["multi"]
                .admitted
                .as_ref()
                .unwrap()
                .cached
                .borrow()
                .clone();
            let mut incompatible = original.clone();
            incompatible.device.functions.pop();
            control
                .context
                .live
                .borrow_mut()
                .devices
                .get_mut("multi")
                .unwrap()
                .candidate = Some(incompatible);
            control.context.reconcile("multi").unwrap();
            let physical = control
                .context
                .registry
                .get(&DeviceKey::new("xiaomi", "multi"))
                .unwrap();
            assert_eq!(physical.functions().len(), 3);
            assert!(matches!(
                physical.availability(),
                Availability::Unavailable(_)
            ));
            assert!(
                control
                    .context
                    .status
                    .borrow()
                    .pending
                    .iter()
                    .any(
                        |pending| pending.did == "multi" && pending.reason.contains("incompatible")
                    )
            );
            assert_eq!(
                control
                    .store
                    .devices()
                    .load_parsed(&original.uid, "local", physical.key())
                    .unwrap()
                    .unwrap(),
                original
            );
            function(&control.context.registry, "one", "main")
                .execute(Command::On)
                .await
                .unwrap();
            control
                .context
                .live
                .borrow_mut()
                .devices
                .get_mut("multi")
                .unwrap()
                .blocked = None;
            control
                .context
                .live
                .borrow_mut()
                .devices
                .get_mut("multi")
                .unwrap()
                .candidate = Some(original);
            control.context.reconcile("multi").unwrap();
            initialized(control, &peer, &["one", "multi"]).await;
        })
        .await;
    }));
}

#[test]
fn endpoint_exhaustion_is_pending_and_does_not_stop_cached_devices() {
    let rig = Rig::new(true, &["one"]);
    rig.cloud.lock().unwrap().members = vec!["one".into(), "multi".into()];
    rusqlite::Connection::open(rig.store.path())
        .unwrap()
        .execute(
            "INSERT INTO endpoint_allocation VALUES (65533,'test','history','main')",
            [],
        )
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
                    .load_published(&DeviceKey::new("xiaomi", "multi"))
                    .unwrap()
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
fn publication_database_failures_are_fatal_and_do_not_publish_partial_topology() {
    let rig = Rig::new(true, &["one"]);
    let store = rig.store.clone();
    rig.cloud.lock().unwrap().members = vec!["one".into(), "multi".into()];
    rusqlite::Connection::open(store.path()).unwrap().execute_batch("CREATE TRIGGER fail_multi BEFORE INSERT ON device_function WHEN NEW.device_id='multi' BEGIN SELECT RAISE(ABORT,'publication failed'); END;").unwrap();
    let error = block_on(rig.run_result(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        c.peer(peer, |_| future::pending()).await;
    }))
    .unwrap_err();
    assert!(matches!(error, AdapterError::Storage(_)));
    assert!(
        store
            .devices()
            .load_published(&DeviceKey::new("xiaomi", "multi"))
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .devices()
            .load_parsed(
                &store.xiaomi().load().unwrap().unwrap().uid,
                "local",
                &DeviceKey::new("xiaomi", "multi")
            )
            .unwrap()
            .is_none()
    );
}
