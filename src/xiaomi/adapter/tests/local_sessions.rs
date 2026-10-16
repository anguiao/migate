use super::*;

#[test]
fn cached_local_control_survives_temporary_cloud_detail_failures() {
    block_on(Rig::new(true, &["one"]).run(|c| async move {
        c.cloud.lock().unwrap().detail_status = 503;
        c.announce();
        let peer = Rc::new(Peer::default());
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| {
                control
                    .context
                    .live
                    .borrow()
                    .devices
                    .values()
                    .all(|slot| slot.loading.is_none())
                    && matches!(
                        control
                            .context
                            .status
                            .borrow()
                            .authentication
                            .authentication,
                        crate::xiaomi::auth::AuthenticationState::Authenticated
                    )
                    && function(&control.context.registry, "one", "main")
                        .snapshot()
                        .power
                        != Power::Unknown
            })
            .await;
            assert_eq!(
                function(&control.context.registry, "one", "main")
                    .snapshot()
                    .availability,
                Availability::Available
            );
            function(&control.context.registry, "one", "main")
                .execute(Command::On)
                .await
                .unwrap();
            assert!(peer.count("rpcReq", "one", 2) > 0);
        })
        .await;
    }));
}

#[test]
fn cached_local_access_recovers_while_cloud_member_sync_is_stalled() {
    let rig = Rig::new(true, &["one"]);
    let (_release, gate) = flume::bounded(1);
    rig.cloud.lock().unwrap().home_gate = Some(gate);
    block_on(rig.run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        let control = &c;
        c.peer(peer, |peer| async move {
            initialized(control, &peer, &["one"]).await;
            until(|| control.cloud.lock().unwrap().homes == 1).await;
            assert_eq!(control.cloud.lock().unwrap().homes, 1);
            assert_eq!(
                function(&control.context.registry, "one", "main")
                    .snapshot()
                    .availability,
                Availability::Available
            );
            function(&control.context.registry, "one", "main")
                .execute(Command::On)
                .await
                .unwrap();
            assert_eq!(peer.count("rpcReq", "one", 2), 1);
        })
        .await;
    }));
}

#[test]
fn discovery_timeout_keeps_its_reason_and_diagnostic_wakes_do_not_skip_retry() {
    block_on(Rig::new(true,&[]).run(|c|async move {
    advance_to(&c.clock,Duration::from_secs(30)).await;until(||matches!(c.context.status.borrow().gateway,GatewayStatus::Unavailable(_))).await;
    c.context.set_catalog_problem(Some("cloud diagnostic".into()));c.context.changed.notify(usize::MAX);for _ in 0..10 {future::yield_now().await;}
    assert!(matches!(c.context.status.borrow().gateway,GatewayStatus::Unavailable(ref reason) if reason.contains("gateway")));
    let mut changes=c.discovery.subscribe();let mut retry=Box::pin(super::super::local::wait_retry(&c.context,&c.discovery,&mut changes,None,Duration::from_secs(5)));assert!(poll_once(retry.as_mut()).await.is_none());
    c.context.changed.notify(usize::MAX);assert!(poll_once(retry.as_mut()).await.is_none());c.clock.advance(Duration::from_secs(5));retry.await.unwrap();
}));
}

#[test]
fn whole_gateway_list_failures_preserve_confirmed_access_and_availability() {
    for timeout in [false, true] {
        block_on(Rig::new(true, &["one", "multi"]).run(|c| async move {
            c.announce();
            let peer = Rc::new(Peer::default());
            let control = &c;
            c.peer(peer, |peer| async move {
                initialized(control, &peer, &["one", "multi"]).await;
                if timeout {
                    peer.state.borrow_mut().list_timeout = true;
                } else {
                    peer.state.borrow_mut().list_error = true;
                }
                peer.list_changed(&["one"]);
                until(|| control.context.status.borrow().catalog_problem.is_some()).await;
                peer.notify("one", 2, true);
                until(|| {
                    function(&control.context.registry, "one", "main")
                        .snapshot()
                        .power
                        == Power::On
                })
                .await;
                for physical in control.context.registry.list() {
                    assert_eq!(physical.availability(), Availability::Available);
                }
                function(&control.context.registry, "multi", "usb")
                    .execute(Command::On)
                    .await
                    .unwrap();
            })
            .await;
        }));
    }
}

#[test]
fn gateway_list_retries_recover_without_events_and_preserve_cloud_diagnostics() {
    for timeout in [false, true] {
        let rig = Rig::new(true, &["one"]);
        rig.cloud.lock().unwrap().status = 503;
        block_on(rig.run(|c| async move {
            c.announce();
            let peer = Rc::new(Peer::default());
            peer.state.borrow_mut().list_error = !timeout;
            peer.state.borrow_mut().list_timeout = timeout;
            let control = &c;
            c.peer(peer, |peer| async move {
                until(|| {
                    control.context.access_problem.borrow().is_some()
                        && control.context.cloud_problem.borrow().is_some()
                })
                .await;
                assert_eq!(peer.lists(), 1);
                for _ in 0..10 {
                    super::super::local::refresh_access(&control.context);
                    future::yield_now().await;
                }
                assert_eq!(peer.lists(), 1);
                {
                    let mut state = peer.state.borrow_mut();
                    state.list_error = false;
                    state.list_timeout = false;
                }
                control
                    .clock
                    .advance(Duration::from_secs(if timeout { 60 } else { 3 }));
                initialized(control, &peer, &["one"]).await;
                assert_eq!(peer.lists(), 2);
                assert!(control.context.access_problem.borrow().is_none());
                assert!(control.context.cloud_problem.borrow().is_some());
                assert_eq!(
                    control.context.status.borrow().catalog_problem,
                    *control.context.cloud_problem.borrow()
                );
                function(&control.context.registry, "one", "main")
                    .execute(Command::On)
                    .await
                    .unwrap();
            })
            .await;
        }));
    }
}

#[test]
fn disconnect_cancels_gateway_list_backoff_before_a_new_session_starts() {
    block_on(Rig::new(true, &["one"]).run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        peer.state.borrow_mut().list_error = true;
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| control.context.access_problem.borrow().is_some()).await;
            assert_eq!(peer.lists(), 1);
        })
        .await;
        until(|| {
            matches!(
                c.context.status.borrow().gateway,
                GatewayStatus::Unavailable(_)
            )
        })
        .await;
        c.clock.advance(Duration::from_secs(6));
        let peer = Rc::new(Peer::default());
        c.peer(peer, |peer| async move {
            initialized(control, &peer, &["one"]).await;
            peer.state.borrow_mut().online.insert("one".into(), false);
            super::super::local::refresh_access(&control.context);
            until(|| {
                matches!(
                    function(&control.context.registry, "one", "main")
                        .snapshot()
                        .availability,
                    Availability::Unavailable(_)
                )
            })
            .await;
            let requests = peer.lists();
            control.clock.advance(Duration::from_secs(30));
            for _ in 0..10 {
                future::yield_now().await;
            }
            assert_eq!(peer.lists(), requests);
            assert!(control.context.access_problem.borrow().is_none());
        })
        .await;
    }));
}

#[test]
fn a_new_master_replaces_a_failed_cached_route_without_changing_devices() {
    block_on(Rig::new(true, &["one", "multi"]).run(|mut c| async move {
        c.announce();
        let next = Async::<TcpListener>::bind(([127, 0, 0, 1], 0)).unwrap();
        let next_address = next.get_ref().local_addr().unwrap();
        let foreign = Async::<TcpListener>::bind(([127, 0, 0, 1], 0)).unwrap();
        let foreign_address = foreign.get_ref().local_addr().unwrap();
        let before = c.store.devices().load_publication(None).unwrap().records;
        let control = &c;
        c.peer(Rc::new(Peer::default()), |peer| async move {
            initialized(control, &peer, &["one", "multi"]).await;
            control
                .events
                .send(announcement(
                    next_address,
                    &control.data[0].uid,
                    "next",
                    456,
                ))
                .unwrap();
            control
                .events
                .send(announcement(
                    foreign_address,
                    "another-account",
                    "foreign",
                    1,
                ))
                .unwrap();
            until(|| control.discovery.snapshot().gateways.len() == 3).await;
            function(&control.context.registry, "one", "main")
                .execute(Command::On)
                .await
                .unwrap();
            assert_eq!(peer.count("rpcReq", "one", 2), 1);
            assert_eq!(
                control.context.status.borrow().gateway,
                GatewayStatus::Connected("123".into())
            );
        })
        .await;
        drop(std::mem::replace(&mut c.listener, next));
        until(|| {
            matches!(
                c.context.status.borrow().gateway,
                GatewayStatus::Unavailable(_)
            )
        })
        .await;
        c.clock.advance(Duration::from_secs(6));
        let control = &c;
        c.peer(Rc::new(Peer::default()), |peer| async move {
            initialized(control, &peer, &["one", "multi"]).await;
            assert_eq!(
                control.context.status.borrow().gateway,
                GatewayStatus::Connected("456".into())
            );
            assert_eq!(control.discovery.snapshot().gateways.len(), 3);
            assert!(poll_once(foreign.accept()).await.is_none());
            assert_eq!(
                control
                    .store
                    .devices()
                    .load_publication(None)
                    .unwrap()
                    .records,
                before
            );
            for (did, id, siid) in [
                ("one", "main", 2),
                ("multi", "outlet_1", 3),
                ("multi", "outlet_2", 4),
                ("multi", "usb", 9),
            ] {
                let function = function(&control.context.registry, did, id);
                until(|| {
                    peer.count("get", did, siid) == 1 && function.snapshot().power == Power::Off
                })
                .await;
                function.execute(Command::On).await.unwrap();
                assert_eq!(peer.count("rpcReq", did, siid), 1);
                peer.notify(did, siid, false);
                until(|| function.snapshot().power == Power::Off).await;
            }
        })
        .await;
    }));
}

#[test]
fn masters_failing_after_connack_cannot_starve_a_later_healthy_master() {
    async fn failing_master(
        c: &Control,
        listener: &Async<TcpListener>,
        did: u64,
        reject_subscription: bool,
        attempts: &RefCell<Vec<u64>>,
    ) {
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            let mut broker = Broker::new(c.acceptor.accept(socket).await.unwrap());
            assert!(matches!(broker.next().await, Packet::Connect(_)));
            broker
                .send(Packet::ConnAck(ConnAck::new(
                    ConnectReturnCode::Success,
                    false,
                )))
                .await;
            let Packet::Subscribe(subscription) = broker.next().await else {
                panic!("baseline SUBSCRIBE expected")
            };
            {
                let mut attempts = attempts.borrow_mut();
                attempts.push(did);
                assert!(
                    attempts.len() <= 2,
                    "a failed master was retried before the third candidate"
                );
                assert_eq!(attempts.as_slice(), &[123, 456][..attempts.len()]);
            }
            if reject_subscription {
                broker
                    .send(Packet::SubAck(SubAck::new(
                        subscription.pkid,
                        vec![SubscribeReasonCode::Unspecified; 2],
                    )))
                    .await;
            }
            drop(broker);
            until(|| {
                matches!(
                    c.context.status.borrow().gateway,
                    GatewayStatus::Unavailable(_)
                )
            })
            .await;
            c.clock.advance(Duration::from_secs(30));
        }
    }

    block_on(Rig::new(true, &["one"]).run(|c| async move {
        let second = Async::<TcpListener>::bind(([127, 0, 0, 1], 0)).unwrap();
        let third = Async::<TcpListener>::bind(([127, 0, 0, 1], 0)).unwrap();
        for (listener, name, did) in [
            (&c.listener, "first", 123),
            (&second, "second", 456),
            (&third, "third", 789),
        ] {
            c.events
                .send(announcement(
                    listener.get_ref().local_addr().unwrap(),
                    &c.data[0].uid,
                    name,
                    did,
                ))
                .unwrap();
        }
        let attempts = RefCell::new(Vec::new());
        let healthy = async {
            let (socket, _) = third.accept().await.unwrap();
            let mut broker = Broker::new(c.acceptor.accept(socket).await.unwrap());
            broker.setup().await;
            let peer = Rc::new(Peer::default());
            peer.state.borrow_mut().client_id = broker.client_id.clone();
            future::or(peer_run(broker, peer.clone(), c.data.clone()), async {
                initialized(&c, &peer, &["one"]).await;
                assert_eq!(attempts.borrow().as_slice(), &[123, 456]);
                assert_eq!(
                    c.context.status.borrow().gateway,
                    GatewayStatus::Connected("789".into())
                );
                let function = function(&c.context.registry, "one", "main");
                function.execute(Command::On).await.unwrap();
                peer.notify("one", 2, false);
                until(|| function.snapshot().power == Power::Off).await;
            })
            .await;
        };
        future::or(
            failing_master(&c, &c.listener, 123, true, &attempts),
            future::or(failing_master(&c, &second, 456, false, &attempts), healthy),
        )
        .await;
    }));
}

#[test]
fn a_new_master_in_the_current_group_wakes_connection_retry() {
    block_on(Rig::new(true, &[]).run(|c| async move {
        c.announce();
        until(|| c.discovery.snapshot().gateways.len() == 1).await;
        let previous = c
            .discovery
            .snapshot()
            .route(&c.data[0].group_id, None)
            .unwrap();
        let mut changes = c.discovery.subscribe();
        let mut retry = Box::pin(super::super::local::wait_retry(
            &c.context,
            &c.discovery,
            &mut changes,
            Some(&previous),
            Duration::from_secs(30),
        ));
        assert!(poll_once(retry.as_mut()).await.is_none());
        let address = c.listener.get_ref().local_addr().unwrap();
        c.events
            .send(announcement(address, "another-account", "foreign", 100))
            .unwrap();
        until(|| c.discovery.snapshot().gateways.len() == 2).await;
        assert!(poll_once(retry.as_mut()).await.is_none());
        c.events
            .send(announcement(address, &c.data[0].uid, "next", 456))
            .unwrap();
        until(|| c.discovery.snapshot().gateways.len() == 3).await;
        assert!(poll_once(retry.as_mut()).await.is_some());
    }));
}

#[test]
fn reconnect_keeps_command_consumers_cancels_old_writes_and_rebuilds_only_registered_devices() {
    block_on(Rig::new(true, &["one", "multi"]).run(|c| async move {
        c.announce();
        let one = function(&c.context.registry, "one", "main");
        let mut command = Box::pin(one.execute(Command::On));
        let peer = Rc::new(Peer::default());
        let control = &c;
        let pending = &mut command;
        c.peer(peer, |peer| async move {
            initialized(control, &peer, &["one", "multi"]).await;
            peer.state.borrow_mut().hold_writes = true;
            assert!(poll_once(pending.as_mut()).await.is_none());
            until(|| {
                peer.state.borrow().held.iter().any(|(publish, _)| {
                    decode_mips(&publish.payload).unwrap().payload["rpc"].is_object()
                })
            })
            .await;
        })
        .await;
        assert!(command.await.is_err());
        until(|| {
            matches!(
                c.context.status.borrow().gateway,
                GatewayStatus::Unavailable(_)
            )
        })
        .await;
        c.cloud.lock().unwrap().members = vec!["multi".into()];
        c.context.request_sync();
        until(|| {
            c.context
                .registry
                .get(&DeviceKey::new("xiaomi", "one"))
                .is_none()
        })
        .await;
        assert_eq!(
            one.execute(Command::Off).await,
            Err(crate::device::CommandError::Cancelled)
        );
        c.clock.advance(Duration::from_secs(6));
        let next = Rc::new(Peer::default());
        c.peer(next, |peer| async move {
            initialized(control, &peer, &["multi"]).await;
            assert_eq!(peer.count("subscribe", "one", 2), 0);
            assert_eq!(peer.count("rpcReq", "one", 2), 0);
            let usb = function(&control.context.registry, "multi", "usb");
            usb.execute(Command::On).await.unwrap();
            usb.execute(Command::Toggle).await.unwrap();
            assert_eq!(usb.snapshot().power, Power::Off);
            assert_eq!(peer.count("rpcReq", "multi", 9), 2);
        })
        .await;
    }));
}

#[test]
fn new_physical_group_discovers_own_local_access_after_member_session() {
    let mut rig = Rig::new(true, &["one"]);
    let uid = rig.data[0].uid.clone();
    let instance: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/group3-instance.json"
    ))
    .unwrap();
    let urn = instance["type"].as_str().unwrap().to_string();
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.online.insert("one".into());
        cloud.catalog_groups = vec![json!({
            "did": "group-record",
            "name": "Native group",
            "model": "mijia.light.group3",
            "spec_type": urn,
            "isOnline": false,
            "extra": {
                "member_list": {
                    "group_did": "group-1",
                    "status": "1",
                    "display_member_switch": 0,
                    "member_ship": {"one": "1"},
                    "is_physical": true
                }
            }
        })];
        cloud
            .catalog_instances
            .insert(urn.clone(), instance.clone());
        for (siid, value) in [(1, json!(false)), (2, json!(50)), (3, json!(2700))] {
            cloud.raw_values.insert(("group-1".into(), 2, siid), value);
        }
    }
    let mut record = cached(&uid, "group-1", false);
    record.model = "mijia.light.group3".into();
    record.instance_type = urn.clone();
    record.device = crate::xiaomi::capability::parse_device_for_model(
        DeviceKey::new("xiaomi", "group-1"),
        "mijia.light.group3",
        &instance,
    )
    .unwrap();
    rig.data.push(record);
    let group = rig.cloud.lock().unwrap().catalog_groups.pop().unwrap();
    block_on(rig.run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        let control = &c;
        c.peer(peer, |peer| async move {
            initialized(control, &peer, &["one"]).await;
            assert!(
                !control
                    .context
                    .live
                    .borrow()
                    .devices
                    .contains_key("group-1")
            );
            let lists = peer.lists();
            control.cloud.lock().unwrap().catalog_groups = vec![group];
            control.context.request_sync();
            until(|| {
                control
                    .context
                    .live
                    .borrow()
                    .devices
                    .get("group-1")
                    .is_some_and(|slot| slot.admitted.is_some())
            })
            .await;
            until(|| {
                matches!(
                    control.context.control_path("group-1"),
                    Some(super::super::routing::ControlPath::Local(_))
                )
            })
            .await;
            assert!(peer.lists() > lists);
            let id = {
                let live = control.context.live.borrow();
                let record = live.devices["group-1"].admitted.as_ref().unwrap();
                let cached = record.cached.borrow();
                cached
                    .device
                    .functions
                    .iter()
                    .find(|function| function.lighting.is_some())
                    .unwrap()
                    .description
                    .id
                    .clone()
            };
            until(|| {
                function(&control.context.registry, "group-1", &id)
                    .snapshot()
                    .availability
                    == Availability::Available
            })
            .await;
            function(&control.context.registry, "group-1", &id)
                .execute(Command::On)
                .await
                .unwrap();
            assert!(peer.count("rpcReq", "group-1", 2) > 0);
        })
        .await;
    }));
}

#[test]
fn native_groups_use_own_routes_and_respect_virtual_type() {
    let mut rig = Rig::light("xiaomi.light.ceil04");
    let instance: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/group3-instance.json"
    ))
    .unwrap();
    let urn = instance["type"].as_str().unwrap().to_owned();
    for (did, physical) in [("group-p", true), ("group-v", false)] {
        let mut record = cached(&rig.data[0].uid, did, false);
        record.model = "mijia.light.group3".into();
        record.instance_type = urn.clone();
        record.device = crate::xiaomi::capability::parse_group(
            DeviceKey::new("xiaomi", did),
            &record.model,
            &instance,
        )
        .unwrap();
        rig.data.push(record);
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.catalog_groups.push(json!({
            "did": format!("record-{did}"), "name": did,
            "model": "mijia.light.group3", "spec_type": urn, "isOnline": false,
            "extra": {"member_list": {
                "group_did": did, "status": "1", "member_ship": {"one": "1"},
                "is_physical": physical
            }}
        }));
        for (piid, value) in [(1, json!(false)), (2, json!(50)), (3, json!(3000))] {
            cloud.raw_values.insert((did.into(), 2, piid), value);
        }
    }
    rig.cloud
        .lock()
        .unwrap()
        .catalog_instances
        .insert(urn, instance);
    block_on(rig.run(|c| async move {
        let peer = Rc::new(Peer::default());
        peer.state.borrow_mut().omit.insert("group-p".into());
        for did in ["one", "group-p", "group-v"] {
            for (piid, value) in [(1, json!(false)), (2, json!(50)), (3, json!(3000))] {
                peer.value(
                    did,
                    crate::xiaomi::capability::PropertyBinding { siid: 2, piid },
                    value,
                );
            }
        }
        c.announce();
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| {
                ["group-p", "group-v"].iter().all(|did| {
                    control
                        .context
                        .live
                        .borrow()
                        .devices
                        .get(*did)
                        .is_some_and(|slot| slot.admitted.is_some())
                })
            })
            .await;
            until(|| {
                control.context.active_notification("one")
                    == Some(super::super::model::PathKind::Local)
            })
            .await;
            let member = function(&control.context.registry, "one", "main");
            until(|| member.snapshot().power == Power::Off).await;
            let physical = function(&control.context.registry, "group-p", "main");
            let virtual_group = function(&control.context.registry, "group-v", "main");
            assert_eq!(member.snapshot().availability, Availability::Available);
            assert!(control.context.control_path("group-p").is_none());
            assert!(matches!(
                physical.snapshot().availability,
                Availability::Unavailable(_)
            ));
            // Obtain the virtual group's own advertised local row as well.
            peer.list_changed(&["group-v"]);
            until(|| {
                control.context.live.borrow().devices["group-v"]
                    .access
                    .is_some()
            })
            .await;
            assert!(control.context.control_path("group-v").is_none());
            assert!(matches!(
                virtual_group.snapshot().availability,
                Availability::Unavailable(_)
            ));
            assert_eq!(peer.count("subscribe", "group-v", 2), 0);
            peer.state.borrow_mut().omit.remove("group-p");
            peer.list_changed(&["group-p"]);
            until(|| {
                control.context.active_notification("group-p")
                    == Some(super::super::model::PathKind::Local)
            })
            .await;
            assert!(matches!(
                control.context.control_path("group-p"),
                Some(super::super::routing::ControlPath::Local(_))
            ));
            assert!(peer.count("subscribe", "group-p", 2) > 0);
            peer.notify_value(
                "group-p",
                crate::xiaomi::capability::PropertyBinding { siid: 2, piid: 1 },
                json!(true),
                false,
            );
            until(|| physical.snapshot().power == Power::On).await;
            assert_eq!(member.snapshot().power, Power::Off);
            control.cloud.lock().unwrap().catalog_groups[1]["isOnline"] = json!(true);
            control.context.request_sync();
            until(|| {
                matches!(
                    control.context.control_path("group-v"),
                    Some(super::super::routing::ControlPath::Cloud(_))
                )
            })
            .await;
            assert_eq!(peer.count("subscribe", "group-v", 2), 0);
        })
        .await;
    }));
}
