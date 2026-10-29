use super::*;

#[test]
fn commands_are_concurrent_across_channels_and_fifo_with_execution_time_toggle() {
    block_on(Rig::new(true, &["one", "multi"]).run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        let control = &c;
        c.peer(peer, |peer| async move {
            initialized(control, &peer, &["one", "multi"]).await;
            peer.state.borrow_mut().hold_writes = true;
            let a = function(&control.context.registry, "one", "outlet:2");
            let b = function(&control.context.registry, "multi", "outlet:3");
            let d = function(&control.context.registry, "multi", "outlet:4");
            let mut first = Box::pin(a.execute(Command::On));
            let mut second = Box::pin(a.execute(Command::Toggle));
            let mut third = Box::pin(a.execute(Command::Off));
            let mut other = Box::pin(b.execute(Command::On));
            let mut channel = Box::pin(d.execute(Command::On));
            for command in [
                &mut first,
                &mut second,
                &mut third,
                &mut other,
                &mut channel,
            ] {
                assert!(poll_once(command.as_mut()).await.is_none());
            }
            until(|| {
                peer.count("rpcReq", "one", 2) == 1
                    && peer.count("rpcReq", "multi", 3) == 1
                    && peer.count("rpcReq", "multi", 4) == 1
            })
            .await;
            control.clock.advance(Duration::from_secs(6));
            assert!(poll_once(second.as_mut()).await.is_none());
            peer.release("multi", 4, accepted("multi", 4, 1)).await;
            assert_eq!(channel.await.unwrap().power(), Some(Power::On));
            peer.release("multi", 3, accepted("multi", 3, 0)).await;
            other.await.unwrap();
            peer.release("one", 2, accepted("one", 2, 0)).await;
            first.await.unwrap();
            until(|| peer.count("rpcReq", "one", 2) == 2).await;
            assert_eq!(
                peer.state
                    .borrow()
                    .seen
                    .iter()
                    .filter(|(op, value)| op == "rpcReq" && value["did"] == "one")
                    .nth(1)
                    .unwrap()
                    .1["rpc"]["params"][0]["value"],
                false
            );
            peer.release("one", 2, accepted("one", 2, 1)).await;
            second.await.unwrap();
            until(|| peer.count("rpcReq", "one", 2) == 3).await;
            peer.release("one", 2, accepted("one", 2, 0)).await;
            third.await.unwrap();
            assert_eq!(a.snapshot().power(), Some(Power::Off));
        })
        .await;
    }));
}

#[test]
fn an_offline_event_before_an_active_success_reply_preserves_acceptance() {
    block_on(Rig::new(true, &["one", "two"]).run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        let control = &c;
        c.peer(peer, |peer| async move {
            initialized(control, &peer, &["one", "two"]).await;
            peer.state.borrow_mut().hold_writes = true;
            let one = function(&control.context.registry, "one", "outlet:2");
            let mut command = Box::pin(one.execute(Command::On));
            assert!(poll_once(command.as_mut()).await.is_none());
            until(|| peer.count("rpcReq", "one", 2) == 1).await;
            peer.state.borrow_mut().online.insert("one".into(), false);
            peer.list_changed(&["one"]);
            until(|| matches!(one.snapshot().availability, Availability::Unavailable(_))).await;
            assert!(poll_once(command.as_mut()).await.is_none());
            peer.release("one", 2, accepted("one", 2, 0)).await;
            let accepted = command.await.unwrap();
            assert_eq!(accepted.power(), Some(Power::On));
            assert!(matches!(
                accepted.availability,
                Availability::Unavailable(_)
            ));
            assert_eq!(
                function(&control.context.registry, "two", "outlet:2")
                    .snapshot()
                    .availability,
                Availability::Available
            );
        })
        .await;
    }));
}

#[test]
fn a_late_success_for_an_obsolete_native_instance_does_not_override_the_current_value() {
    block_on(Rig::light("xiaomi.light.ceil04").run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        peer.value(
            "one",
            crate::xiaomi::capability::PropertyBinding { siid: 2, piid: 1 },
            json!(false),
        );
        peer.value(
            "one",
            crate::xiaomi::capability::PropertyBinding { siid: 2, piid: 2 },
            json!(50),
        );
        peer.value(
            "one",
            crate::xiaomi::capability::PropertyBinding { siid: 2, piid: 3 },
            json!(2700),
        );
        let control = &c;
        c.peer(peer, |peer| async move {
            initialized(control, &peer, &["one"]).await;
            peer.state.borrow_mut().hold_writes = true;
            let one = function(&control.context.registry, "one", "light:2");
            let mut command = Box::pin(one.execute(Command::On));
            assert!(poll_once(command.as_mut()).await.is_none());
            until(|| peer.count("rpcReq", "one", 2) == 1).await;
            let before_physical = control
                .context
                .registry
                .get(&DeviceKey::new("xiaomi", "one"))
                .unwrap();
            let mut changed = control.context.live.borrow().devices["one"]
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
            control
                .context
                .live
                .borrow_mut()
                .devices
                .get_mut("one")
                .unwrap()
                .candidate
                .as_mut()
                .unwrap()
                .native = Some(changed);
            control.context.reconcile("one").unwrap();
            let current_physical = loop {
                let current = control
                    .context
                    .registry
                    .get(&DeviceKey::new("xiaomi", "one"))
                    .unwrap();
                if !before_physical.same_instance(&current) {
                    break current;
                }
                future::yield_now().await;
            };
            let current = current_physical.function("light:2").unwrap();
            until(|| current.snapshot().power() == Some(Power::Off)).await;
            peer.release("one", 2, accepted("one", 2, 0)).await;
            // A sent light receipt keeps its ownership and settles the
            // original request; it must not project into the new instance.
            assert!(
                command.await.is_ok(),
                "the original light receipt settles after the sink is retired"
            );
            assert!(matches!(
                one.snapshot().availability,
                Availability::Unavailable(_)
            ));
            assert_ne!(one.snapshot().power(), Some(Power::On));
            until(|| current.snapshot().availability == Availability::Available).await;
            assert_eq!(current.snapshot().power(), Some(Power::Off));
            peer.state.borrow_mut().hold_writes = false;
            current.execute(Command::On).await.unwrap();
            assert_eq!(peer.count("rpcReq", "one", 2), 2);
            assert!(
                peer.state.borrow().seen.iter().any(|(operation, value)| {
                    operation == "rpcReq"
                        && value["did"] == "one"
                        && value["rpc"]["params"][0]["siid"] == 2
                        && value["rpc"]["params"][0]["piid"] == 1
                        && value["rpc"]["params"][0]["value"] == true
                }),
                "the replacement instance sends its own absolute power target"
            );
            assert_eq!(
                current.snapshot().power(),
                Some(Power::Off),
                "light acceptance does not infer actual power before feedback"
            );
        })
        .await;
    }));
}

#[test]
fn restored_and_duplicate_description_sync_keeps_channel_commands() {
    let mut rig = Rig::new(true, &["multi"]);
    let stable = rig.data[1].clone();
    let (mut source, ready) = XiaomiAdapter::build(
        rig.store.clone(),
        rig.source.cloud.clone(),
        rig.discovery.clone(),
        rig.clock.clone(),
    )
    .unwrap();
    source.test_ca = rig.source.test_ca.clone();
    source.request_timeout = rig.source.request_timeout;
    rig.source = source;
    rig.ready = ready;
    block_on(rig.run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        let control = &c;
        c.peer(peer, |peer| async move {
            initialized(control, &peer, &["multi"]).await;
            let key = DeviceKey::new("xiaomi", "multi");
            let physical = control.context.registry.get(&key).unwrap();
            let runtime_order = physical
                .functions()
                .iter()
                .map(|function| function.function_key().id.clone())
                .collect::<Vec<_>>();
            let cached_order = stable
                .device()
                .functions()
                .iter()
                .map(|function| function.id().to_owned())
                .collect::<Vec<_>>();
            assert_eq!(runtime_order, cached_order);
            for (id, siid) in [("outlet:3", 3), ("outlet:4", 4), ("outlet:9", 9)] {
                physical
                    .function(id)
                    .unwrap()
                    .execute(Command::On)
                    .await
                    .unwrap();
                assert_eq!(peer.count("rpcReq", "multi", siid), 1);
            }
            control
                .context
                .live
                .borrow_mut()
                .devices
                .get_mut("multi")
                .unwrap()
                .candidate
                .as_mut()
                .unwrap()
                .native = Some(stable.clone());
            control.context.reconcile("multi").unwrap();
            let current = control.context.registry.get(&key).unwrap();
            assert!(
                physical.same_instance(&current),
                "an identical duplicate description keeps the serving instance"
            );
            for (id, siid) in [("outlet:3", 3), ("outlet:4", 4), ("outlet:9", 9)] {
                physical
                    .function(id)
                    .unwrap()
                    .execute(Command::Off)
                    .await
                    .unwrap();
                assert_eq!(peer.count("rpcReq", "multi", siid), 2);
            }
        })
        .await;
    }));
}
