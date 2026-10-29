use super::*;

#[test]
fn same_value_notification_invalidates_an_inflight_read() {
    block_on(Rig::new(true, &["one", "two"]).run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        let control = &c;
        c.peer(peer, |peer| async move {
            initialized(control, &peer, &["one", "two"]).await;
            let one = function(&control.context.registry, "one", "outlet:2");
            let two = function(&control.context.registry, "two", "outlet:2");
            peer.state.borrow_mut().hold_reads = true;
            let reads = peer.count("get", "one", 2);
            control.context.live.borrow().devices["one"]
                .admitted
                .as_ref()
                .unwrap()
                .refresh();
            until(|| peer.count("get", "one", 2) == reads + 1).await;
            let before = one.snapshot();
            peer.notify("one", 2, false);
            peer.notify("two", 2, true);
            until(|| two.snapshot().power() == Some(Power::On)).await;
            assert_eq!(one.snapshot(), before);
            peer.release("one", 2, json!({"value":true})).await;
            async_io::Timer::after(Duration::from_millis(20)).await;
            assert_eq!(one.snapshot().power(), Some(Power::Off));
        })
        .await;
    }));
}

#[test]
fn notification_after_write_preserves_state_and_advances_the_channel_fifo() {
    block_on(Rig::new(true, &["one", "two"]).run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        let control = &c;
        c.peer(peer, |peer| async move {
            initialized(control, &peer, &["one", "two"]).await;
            let one = function(&control.context.registry, "one", "outlet:2");
            let two = function(&control.context.registry, "two", "outlet:2");
            peer.state.borrow_mut().hold_writes = true;
            let mut first = Box::pin(one.execute(Command::On));
            let mut second = Box::pin(one.execute(Command::Toggle));
            assert!(poll_once(first.as_mut()).await.is_none());
            assert!(poll_once(second.as_mut()).await.is_none());
            until(|| peer.count("rpcReq", "one", 2) == 1).await;
            peer.notify("one", 2, false);
            peer.notify("two", 2, true);
            until(|| two.snapshot().power() == Some(Power::On)).await;
            peer.release("one", 2, accepted("one", 2, 0)).await;
            assert_eq!(first.await.unwrap().power(), Some(Power::Off));
            until(|| peer.count("rpcReq", "one", 2) == 2 && !peer.state.borrow().held.is_empty())
                .await;
            assert_eq!(
                peer.state
                    .borrow()
                    .seen
                    .iter()
                    .rev()
                    .find(|(operation, _)| operation == "rpcReq")
                    .unwrap()
                    .1["rpc"]["params"][0]["value"],
                true
            );
            peer.release("one", 2, accepted("one", 2, 0)).await;
            assert_eq!(second.await.unwrap().power(), Some(Power::On));
        })
        .await;
    }));
}

#[test]
fn virtual_groups_follow_typed_members_and_new_relations_without_group_reads() {
    let rig = Rig::lights(&[
        ("one", "xiaomi.light.ceil04"),
        ("two", "xiaomi.light.ceil04"),
    ]);
    {
        let mut cloud = rig.cloud.lock().unwrap();
        let instance = super::super::support::group_instance();
        cloud
            .catalog_instances
            .insert(instance["type"].as_str().unwrap().into(), instance);
        cloud.catalog_groups = vec![
            super::super::support::group_record("record-1", "group-1", &["one"], false),
            super::super::support::group_record("record-2", "group-2", &["two"], false),
        ];
        cloud.online.extend(["one", "two"].map(str::to_owned));
        for did in ["one", "two"] {
            for (piid, value) in [(1, json!(false)), (2, json!(50)), (3, json!(2700))] {
                cloud.raw_values.insert((did.into(), 2, piid), value);
            }
        }
        // Deliberately stale group values must never overwrite member feedback.
        cloud
            .raw_values
            .insert(("group-1".into(), 2, 1), json!(true));
        cloud
            .raw_values
            .insert(("group-2".into(), 2, 1), json!(true));
    }
    let listener = Async::<TcpListener>::bind(([127, 0, 0, 1], 0)).unwrap();
    rig.source.context.notification_connector.set(
        super::super::super::notifications::NotificationConnector::Plaintext(
            listener.get_ref().local_addr().unwrap(),
        ),
    );
    let peer = Rc::new(CloudPeer::default());
    block_on(rig.run(|c| async move {
        let peer_run = peer.clone();
        future::or(peer_run.run(&listener), async {
            c.announce();
            until(|| {
                ["group-1", "group-2"].iter().all(|did| {
                    c.context
                        .registry
                        .get(&DeviceKey::new("xiaomi", *did))
                        .is_some()
                })
            })
            .await;
            until(|| cloud_active(&c, "one") && cloud_active(&c, "two")).await;
            let first = function(&c.context.registry, "group-1", "light:2");
            let second = function(&c.context.registry, "group-2", "light:2");
            until(|| {
                first.snapshot().power() == Some(Power::Off)
                    && second.snapshot().power() == Some(Power::Off)
            })
            .await;

            c.cloud
                .lock()
                .unwrap()
                .raw_values
                .insert(("one".into(), 2, 1), json!(true));
            peer.property("one", 2, true);
            until(|| first.snapshot().power() == Some(Power::On)).await;
            assert_eq!(second.snapshot().power(), Some(Power::Off));
            for (piid, value) in [(2, json!(70)), (3, json!(4000))] {
                c.cloud
                    .lock()
                    .unwrap()
                    .raw_values
                    .insert(("one".into(), 2, piid), value.clone());
                peer.send(Packet::Publish(Publish::new(
                    format!("device/one/up/properties_changed/2/{piid}"),
                    QoS::AtMostOnce,
                    json!({"params":{"did":"one","siid":2,"piid":piid,"value":value}}).to_string(),
                )));
            }
            until(|| {
                first.snapshot().lighting().is_some_and(|light| {
                    light.brightness_percent == Some(70.0) && light.temperature_kelvin == Some(4000)
                })
            })
            .await;

            // Adding an already observed member applies its current state;
            // no new event or duplicate subscription is needed.
            let subscriptions = peer.count("device/one/up/properties_changed/2/1");
            c.cloud.lock().unwrap().catalog_groups[1] =
                super::super::support::group_record("record-2", "group-2", &["two", "one"], false);
            c.context.request_sync();
            until(|| second.snapshot().power() == Some(Power::On)).await;
            assert_eq!(
                peer.count("device/one/up/properties_changed/2/1"),
                subscriptions
            );
            c.cloud
                .lock()
                .unwrap()
                .raw_values
                .insert(("one".into(), 2, 1), json!(false));
            peer.property("one", 2, false);
            until(|| {
                first.snapshot().power() == Some(Power::Off)
                    && second.snapshot().power() == Some(Power::Off)
            })
            .await;

            // Losing a member's actual source invalidates both groups instead
            // of treating the unavailable member as off.
            peer.online("one", false);
            until(|| {
                first.snapshot().power() == Some(Power::Unknown)
                    && second.snapshot().power() == Some(Power::Unknown)
            })
            .await;
            let cleared = first.snapshot().lighting().unwrap();
            assert_eq!(cleared.brightness_percent, None);
            assert_eq!(cleared.temperature_kelvin, None);
            assert_eq!(cleared.color, None);
            assert_eq!(cleared.mode, None);
            c.cloud
                .lock()
                .unwrap()
                .raw_values
                .insert(("one".into(), 2, 1), json!(true));
            peer.online("one", true);
            until(|| {
                first.snapshot().power() == Some(Power::On)
                    && second.snapshot().power() == Some(Power::On)
            })
            .await;

            assert!(
                c.cloud
                    .lock()
                    .unwrap()
                    .property_reads
                    .iter()
                    .all(|(did, _, _)| !did.starts_with("group-"))
            );
            assert!(!peer.state.borrow().seen.iter().any(|packet| matches!(packet,
                Packet::Subscribe(subscribe) if subscribe.filters.iter().any(|filter|
                    (filter.path.contains("group-") && filter.path.contains("properties_changed"))
                        || filter.path.ends_with("properties_changed/#")
                )
            )));
        })
        .await;
    }));
}

#[test]
fn virtual_group_reconnect_reads_members_even_when_property_subscriptions_fail() {
    let rig = super::super::support::repair_rig(0, false);
    let listener = Async::<TcpListener>::bind(([127, 0, 0, 1], 0)).unwrap();
    rig.source.context.notification_connector.set(
        super::super::super::notifications::NotificationConnector::Plaintext(
            listener.get_ref().local_addr().unwrap(),
        ),
    );
    let peer = Rc::new(CloudPeer::default());
    let property = "device/member-a/up/properties_changed/2/1";
    peer.state
        .borrow_mut()
        .reject
        .insert(property.into(), SubscribeReasonCode::QuotaExceeded);
    block_on(rig.run(|c| async move {
        future::or(peer.run(&listener), async {
            super::super::support::repair_ready(&c).await;
            let member = c.context.live.borrow().devices["member-a"]
                .admitted
                .clone()
                .unwrap();
            until(|| member.test_cloud_exhausted()).await;
            c.clock
                .automatic
                .store(false, std::sync::atomic::Ordering::Relaxed);
            assert!(member.observer().is_none());
            let group = function(&c.context.registry, "group-1", "light:2");
            assert_eq!(group.snapshot().power(), Some(Power::Off));
            let key = ("member-a".to_string(), 2, 1);
            let reads = || {
                c.cloud
                    .lock()
                    .unwrap()
                    .property_reads
                    .iter()
                    .filter(|read| **read == key)
                    .count()
            };
            let before = reads();
            let subscriptions = peer.count(property);
            c.cloud
                .lock()
                .unwrap()
                .raw_values
                .insert(key.clone(), json!(true));
            peer.send(Packet::Disconnect(Disconnect::new()));
            until(|| c.context.notifications.borrow().is_none()).await;
            let due = c.clock.now() + Duration::from_secs(10);
            until(|| c.clock.deadlines.lock().unwrap().contains(&due)).await;
            c.clock.advance(Duration::from_secs(10));
            until(|| peer.state.borrow().connects.len() == 2).await;
            c.clock
                .automatic
                .store(true, std::sync::atomic::Ordering::Relaxed);
            until(|| peer.count(property) > subscriptions && member.test_cloud_exhausted()).await;
            until(|| reads() > before && group.snapshot().power() == Some(Power::On)).await;
            assert!(member.observer().is_none());
            assert_eq!(group.snapshot().availability, Availability::Available);
            assert!(
                c.context
                    .diagnostics()
                    .devices
                    .iter()
                    .any(|device| device.did == "group-1" && device.sync_limited_reason.is_some())
            );
            assert!(
                c.cloud
                    .lock()
                    .unwrap()
                    .property_reads
                    .iter()
                    .all(|(did, _, _)| did != "group-1")
            );
        })
        .await;
    }));
}
