use super::*;
use crate::xiaomi::capability::PropertyBinding;

const POWER: PropertyBinding = PropertyBinding { siid: 2, piid: 1 };

fn observation_rig(members: &[&str]) -> Rig {
    let mut rig = Rig::new(true, members);
    rig.source.request_timeout = Duration::from_secs(2);
    rig
}

fn device_work(c: &Control, did: &str) -> Rc<super::super::work::DeviceWork> {
    c.context.live.borrow().devices[did]
        .admitted
        .as_ref()
        .unwrap()
        .clone()
}

async fn freeze_initialized(c: &Control, peer: &Peer, dids: &[&str]) {
    initialized(c, peer, dids).await;
    c.clock
        .automatic
        .store(false, std::sync::atomic::Ordering::Relaxed);
    async_io::Timer::after(Duration::from_millis(20)).await;
}

async fn refresh_read(c: &Control, peer: &Peer, did: &str, siid: u32) {
    let reads = peer.count("get", did, siid);
    let deadline = c.clock.now() + Duration::from_millis(200);
    device_work(c, did).refresh();
    until(|| c.clock.deadlines.lock().unwrap().contains(&deadline)).await;
    c.clock.advance(Duration::from_millis(200));
    until(|| peer.count("get", did, siid) == reads + 1).await;
}

async fn release_operation(peer: &Peer, operation: &str, did: &str, siid: u32, value: Value) {
    let matches = |publish: &Publish| {
        let payload = decode_mips(&publish.payload).unwrap().payload;
        publish.topic.rsplit('/').next() == Some(operation)
            && payload["did"] == did
            && (payload["siid"] == siid || payload["rpc"]["params"][0]["siid"] == siid)
    };
    until(|| {
        peer.state
            .borrow()
            .held
            .iter()
            .any(|(publish, _)| matches(publish))
    })
    .await;
    let index = peer
        .state
        .borrow()
        .held
        .iter()
        .position(|(publish, _)| matches(publish))
        .unwrap();
    let (publish, _) = peer.state.borrow_mut().held.remove(index);
    let client = peer.state.borrow().client_id.clone();
    peer.send(response(&client, &publish, value));
}

#[test]
fn an_accepted_write_invalidates_an_older_held_local_read() {
    block_on(observation_rig(&["one", "two"]).run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(Rc::new(Peer::default()), |peer| async move {
            freeze_initialized(control, &peer, &["one", "two"]).await;
            let one = function(&control.context.registry, "one", "main");
            let two = function(&control.context.registry, "two", "main");
            peer.state.borrow_mut().hold_reads = true;
            refresh_read(control, &peer, "one", 2).await;

            assert_eq!(one.execute(Command::On).await.unwrap().power, Power::On);
            let accepted = one.snapshot();
            release_operation(&peer, "get", "one", 2, json!({"value":false})).await;
            peer.notify("two", 2, true);
            until(|| two.snapshot().power == Power::On).await;

            assert_eq!(one.snapshot(), accepted);
            assert_eq!(peer.count("rpcReq", "one", 2), 1);
        })
        .await;
    }));
}

#[test]
fn a_source_change_and_new_read_do_not_invalidate_a_held_write_without_notifications() {
    block_on(observation_rig(&["one", "two"]).run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(Rc::new(Peer::default()), |peer| async move {
            freeze_initialized(control, &peer, &["one", "two"]).await;
            let one = function(&control.context.registry, "one", "main");
            let two = function(&control.context.registry, "two", "main");
            peer.state.borrow_mut().hold_writes = true;
            let mut command = Box::pin(one.execute(Command::On));
            assert!(poll_once(command.as_mut()).await.is_none());
            until(|| peer.count("rpcReq", "one", 2) == 1).await;

            device_work(control, "one").observations.source_changed();
            peer.state.borrow_mut().hold_reads = true;
            refresh_read(control, &peer, "one", 2).await;
            release_operation(&peer, "get", "one", 2, json!({"value":false})).await;
            peer.notify("two", 2, true);
            until(|| two.snapshot().power == Power::On).await;
            assert_eq!(one.snapshot().power, Power::Off);
            assert!(poll_once(command.as_mut()).await.is_none());

            release_operation(&peer, "rpcReq", "one", 2, accepted("one", 2, 0)).await;
            assert_eq!(command.await.unwrap().power, Power::On);
            assert_eq!(one.snapshot().power, Power::On);
            assert_eq!(peer.count("rpcReq", "one", 2), 1);
        })
        .await;
    }));
}

#[test]
fn a_native_replacement_keeps_a_late_write_receipt_out_of_the_new_sink() {
    block_on(Rig::light("xiaomi.light.ceil04").run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        peer.value("one", POWER, json!(false));
        let control = &c;
        c.peer(peer, |peer| async move {
            let one = function(&control.context.registry, "one", "main");
            initialized(control, &peer, &["one"]).await;
            let before = control
                .context
                .registry
                .get(&DeviceKey::new("xiaomi", "one"))
                .unwrap();
            peer.state.borrow_mut().hold_writes = true;
            let mut command = Box::pin(one.execute(Command::On));
            assert!(poll_once(command.as_mut()).await.is_none());
            until(|| peer.count("rpcReq", "one", 2) == 1).await;
            let old_work = device_work(control, "one");

            let mut changed = old_work.native.clone();
            changed = crate::xiaomi::capability::NativeDescription::new(
                changed.model().to_owned(),
                changed.instance_type().to_owned(),
                super::support::device_with_temperature_grid(
                    changed.device(),
                    crate::device::NumericRange::new(3000, 5500, 1).unwrap(),
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
                .latest = Some(changed);
            control.context.reconcile("one").unwrap();
            until(|| {
                control
                    .context
                    .registry
                    .get(&DeviceKey::new("xiaomi", "one"))
                    .is_some_and(|now| !before.same_instance(&now))
            })
            .await;

            assert!(old_work.life.is_closed());
            assert!(matches!(
                one.snapshot().availability,
                Availability::Unavailable(_)
            ));
            release_operation(&peer, "rpcReq", "one", 2, accepted("one", 2, 0)).await;
            // The sent light receipt keeps its ownership and settles the
            // original request; it does not project into the new sink.
            assert!(command.await.is_ok());
            assert_ne!(one.snapshot().power, Power::On);
        })
        .await;
    }));
}

#[test]
fn a_duplicate_description_preserves_a_held_write_and_its_consumer() {
    block_on(observation_rig(&["multi"]).run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(Rc::new(Peer::default()), |peer| async move {
            freeze_initialized(control, &peer, &["multi"]).await;
            let outlet = function(&control.context.registry, "multi", "outlet_1");
            let work = device_work(control, "multi");
            peer.state.borrow_mut().hold_writes = true;
            let mut command = Box::pin(outlet.execute(Command::On));
            assert!(poll_once(command.as_mut()).await.is_none());
            until(|| peer.count("rpcReq", "multi", 3) == 1).await;

            let duplicate = work.native.clone();
            control
                .context
                .live
                .borrow_mut()
                .devices
                .get_mut("multi")
                .unwrap()
                .latest = Some(duplicate);
            control.context.reconcile("multi").unwrap();
            assert!(Rc::ptr_eq(&work, &device_work(control, "multi")));
            release_operation(&peer, "rpcReq", "multi", 3, accepted("multi", 3, 0)).await;
            assert_eq!(command.await.unwrap().power, Power::On);

            let mut following = Box::pin(outlet.execute(Command::Toggle));
            assert!(poll_once(following.as_mut()).await.is_none());
            until(|| peer.count("rpcReq", "multi", 3) == 2).await;
            release_operation(&peer, "rpcReq", "multi", 3, accepted("multi", 3, 0)).await;
            assert_eq!(following.await.unwrap().power, Power::Off);
            for id in ["outlet_2", "usb"] {
                assert_eq!(
                    function(&control.context.registry, "multi", id)
                        .snapshot()
                        .power,
                    Power::Off
                );
            }
        })
        .await;
    }));
}

#[test]
fn local_disconnect_preserves_the_work_instance_and_cloud_command_consumer() {
    let rig = observation_rig(&["one"]);
    rig.cloud.lock().unwrap().online.insert("one".into());
    block_on(rig.run(|c| async move {
        c.announce();
        let retained = Rc::new(RefCell::new(None));
        let saved = retained.clone();
        let control = &c;
        c.peer(Rc::new(Peer::default()), |peer| async move {
            freeze_initialized(control, &peer, &["one"]).await;
            until(|| {
                control.context.live.borrow().devices["one"]
                    .cloud_access
                    .as_ref()
                    .is_some_and(|access| access.online)
            })
            .await;
            let work = device_work(control, "one");
            assert!(!work.test_local_subscribed_empty());
            function(&control.context.registry, "one", "main")
                .execute(Command::On)
                .await
                .unwrap();
            assert_eq!(peer.count("rpcReq", "one", 2), 1);
            *saved.borrow_mut() = Some(work);
        })
        .await;

        until(|| c.context.live.borrow().session.is_none()).await;
        let before = retained.borrow().as_ref().unwrap().clone();
        let after = device_work(&c, "one");
        assert!(Rc::ptr_eq(&before, &after));
        assert!(!after.life.is_closed());
        assert!(after.test_local_subscribed_empty());
        assert!(c.context.live.borrow().devices["one"].access.is_none());
        let one = function(&c.context.registry, "one", "main");
        assert_eq!(one.snapshot().availability, Availability::Available);
        assert_eq!(one.execute(Command::Off).await.unwrap().power, Power::Off);
        assert_eq!(c.cloud.lock().unwrap().property_writes.len(), 1);
    }));
}

#[test]
fn a_suback_from_a_previous_native_instance_cannot_restore_subscription_readiness() {
    block_on(
        Rig::lights(&[
            ("one", "xiaomi.light.ceil04"),
            ("two", "xiaomi.light.ceil04"),
        ])
        .run(|c| async move {
            c.announce();
            let peer = Rc::new(Peer::default());
            for did in ["one", "two"] {
                for (siid, piid, value) in
                    [(2, 1, json!(false)), (2, 2, json!(50)), (2, 3, json!(2700))]
                {
                    peer.value(did, PropertyBinding { siid, piid }, value);
                }
            }
            let control = &c;
            c.peer(peer, |peer| async move {
                initialized(control, &peer, &["one", "two"]).await;
                peer.state
                    .borrow_mut()
                    .sub_timeout
                    .insert(("one".into(), 2, 3));
                let first = device_work(control, "one");
                let original = first.native.clone();
                let mut narrowed = original.clone();
                narrowed = crate::xiaomi::capability::NativeDescription::new(
                    narrowed.model().to_owned(),
                    narrowed.instance_type().to_owned(),
                    super::support::device_with_temperature_grid(
                        narrowed.device(),
                        crate::device::NumericRange::new(3000, 5500, 1).unwrap(),
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
                    .latest = Some(narrowed);
                control.context.reconcile("one").unwrap();
                until(|| {
                    let current = device_work(control, "one");
                    !Rc::ptr_eq(&first, &current)
                })
                .await;
                until(|| !peer.state.borrow().held_subs.is_empty()).await;
                let old = peer.state.borrow_mut().held_subs.remove(0);

                let second = device_work(control, "one");
                control
                    .context
                    .live
                    .borrow_mut()
                    .devices
                    .get_mut("one")
                    .unwrap()
                    .latest = Some(original);
                control.context.reconcile("one").unwrap();
                until(|| {
                    let current = device_work(control, "one");
                    !Rc::ptr_eq(&second, &current)
                })
                .await;
                assert!(second.life.is_closed());

                // The stale SUBACK belongs to the retired instance. Use the
                // already prepared second light as the shared-wire barrier; the
                // not-yet-ready current instance cannot deliver its own notify.
                peer.send(Packet::SubAck(SubAck::new(
                    old.pkid,
                    vec![SubscribeReasonCode::QoS2],
                )));
                peer.notify_value("two", POWER, json!(true), false);
                until(|| {
                    function(&control.context.registry, "two", "main")
                        .snapshot()
                        .power
                        == Power::On
                })
                .await;
                let current = device_work(control, "one");
                assert!(!current.test_local_subscribed_pair(2, 3));

                until(|| {
                    peer.state
                        .borrow()
                        .held_subs
                        .iter()
                        .any(|sub| sub.pkid != old.pkid && topic_key(&sub.filters[0].path).1 == 2)
                })
                .await;
                let held = std::mem::take(&mut peer.state.borrow_mut().held_subs);
                for sub in held {
                    peer.send(Packet::SubAck(SubAck::new(
                        sub.pkid,
                        vec![SubscribeReasonCode::QoS2],
                    )));
                }
                until(|| current.test_local_subscribed_len() == current.bindings().len()).await;
            })
            .await;
        }),
    );
}
