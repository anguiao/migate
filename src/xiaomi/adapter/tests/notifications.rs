use super::*;
use futures_lite::io::{AsyncReadExt, AsyncWriteExt};

struct CloudBroker {
    stream: Async<std::net::TcpStream>,
    frames: crate::xiaomi::mqtt::Frames,
}
impl CloudBroker {
    async fn accept(listener: &Async<TcpListener>) -> Self {
        let (stream, _) = listener.accept().await.unwrap();
        let mut broker = Self {
            stream,
            frames: Default::default(),
        };
        assert!(matches!(broker.next().await, Packet::Connect(_)));
        broker
            .send(Packet::ConnAck(ConnAck::new(
                ConnectReturnCode::Success,
                false,
            )))
            .await;
        broker
    }
    async fn next(&mut self) -> Packet {
        loop {
            if let Some(packet) = self.frames.next_packet().unwrap() {
                return packet;
            }
            let mut bytes = [0; 4096];
            let len = self.stream.read(&mut bytes).await.unwrap();
            assert!(len > 0);
            self.frames.push(&bytes[..len]).unwrap();
        }
    }
    async fn send(&mut self, packet: Packet) {
        self.stream
            .write_all(&crate::xiaomi::mqtt::encode_packet(&packet).unwrap())
            .await
            .unwrap();
        self.stream.flush().await.unwrap();
    }
    async fn ack(&mut self, subscribe: Subscribe) {
        self.send(Packet::SubAck(SubAck::new(
            subscribe.pkid,
            vec![SubscribeReasonCode::QoS2],
        )))
        .await;
    }
    async fn property(&mut self, did: &str, siid: u32, value: bool) {
        self.send(Packet::Publish(Publish::new(
            format!("device/{did}/up/properties_changed/{siid}/1"),
            QoS::AtMostOnce,
            json!({"params":{"did":did,"siid":siid,"piid":1,"value":value}}).to_string(),
        )))
        .await;
    }
}
#[test]
fn cloud_notifications_activate_after_subscription_and_update_cloud_only_device() {
    let rig = Rig::new(true, &["one"]);
    rig.cloud.lock().unwrap().online.insert("one".into());
    let listener = Async::<TcpListener>::bind(([127, 0, 0, 1], 0)).unwrap();
    rig.source.context.notification_connector.set(
        super::super::notifications::NotificationConnector::Plaintext(
            listener.get_ref().local_addr().unwrap(),
        ),
    );
    block_on(rig.run(|c| async move {
        let mut broker = CloudBroker::accept(&listener).await;
        let mut property = false;
        let mut online = false;
        while !property || !online {
            if let Packet::Subscribe(subscribe) = broker.next().await {
                property |= subscribe.filters[0].path.contains("properties_changed");
                online |= subscribe.filters[0].path.ends_with("state/#");
                broker.ack(subscribe).await;
            }
        }
        until(|| {
            function(&c.context.registry, "one", "main")
                .snapshot()
                .power
                == Power::Off
        })
        .await;
        broker.property("one", 2, true).await;
        until(|| {
            function(&c.context.registry, "one", "main")
                .snapshot()
                .power
                == Power::On
        })
        .await;
        assert_eq!(
            function(&c.context.registry, "one", "main")
                .snapshot()
                .availability,
            Availability::Available
        );
    }));
}

#[derive(Default)]
struct CloudPeerState {
    connects: Vec<String>,
    connacks: std::collections::VecDeque<ConnectReturnCode>,
    seen: Vec<Packet>,
    held: Vec<Subscribe>,
    hold: BTreeSet<String>,
    reject: BTreeMap<String, SubscribeReasonCode>,
    send: Vec<Packet>,
}
#[derive(Default)]
struct CloudPeer {
    state: RefCell<CloudPeerState>,
    changed: Event,
}
impl CloudPeer {
    fn send(&self, packet: Packet) {
        self.state.borrow_mut().send.push(packet);
        self.changed.notify(usize::MAX);
    }
    fn property(&self, did: &str, siid: u32, value: bool) {
        self.send(Packet::Publish(Publish::new(
            format!("device/{did}/up/properties_changed/{siid}/1"),
            QoS::AtMostOnce,
            json!({"params":{"did":did,"siid":siid,"piid":1,"value":value}}).to_string(),
        )));
    }
    fn qos2(&self, did: &str, siid: u32, value: bool, pkid: u16) {
        let mut publish = Publish::new(
            format!("device/{did}/up/properties_changed/{siid}/1"),
            QoS::ExactlyOnce,
            json!({"params":{"did":did,"siid":siid,"piid":1,"value":value}}).to_string(),
        );
        publish.pkid = pkid;
        self.send(Packet::Publish(publish));
    }
    fn online(&self, did: &str, online: bool) {
        self.send(Packet::Publish(Publish::new(
            format!(
                "device/{did}/state/{}",
                if online { "online" } else { "offline" }
            ),
            QoS::AtMostOnce,
            json!({"device_id":did,"event":if online {"online"} else {"offline"}}).to_string(),
        )));
    }
    fn release(&self, path: &str) {
        let index = self
            .state
            .borrow()
            .held
            .iter()
            .position(|subscribe| subscribe.filters[0].path == path)
            .unwrap();
        let subscribe = self.state.borrow_mut().held.remove(index);
        self.send(Packet::SubAck(SubAck::new(
            subscribe.pkid,
            vec![SubscribeReasonCode::QoS2],
        )));
    }
    fn count(&self, path: &str) -> usize {
        self.state.borrow().seen.iter().filter(|packet|matches!(packet,Packet::Subscribe(subscribe) if subscribe.filters[0].path==path)).count()
    }
    fn received(&self, pkid: u16, complete: bool) -> bool {
        self.state.borrow().seen.iter().any(|packet| {
            if complete {
                matches!(packet,Packet::PubComp(ack) if ack.pkid==pkid)
            } else {
                matches!(packet,Packet::PubRec(ack) if ack.pkid==pkid)
            }
        })
    }
    async fn run(&self, listener: &Async<TcpListener>) {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let mut broker = CloudBroker {
                stream,
                frames: Default::default(),
            };
            let Packet::Connect(connect) = broker.next().await else {
                panic!("expected CONNECT")
            };
            self.state
                .borrow_mut()
                .connects
                .push(connect.login.unwrap().password);
            let code = self
                .state
                .borrow_mut()
                .connacks
                .pop_front()
                .unwrap_or(ConnectReturnCode::Success);
            broker
                .send(Packet::ConnAck(ConnAck::new(code, false)))
                .await;
            if code != ConnectReturnCode::Success {
                continue;
            }
            loop {
                let listener = self.changed.listen();
                let packets = std::mem::take(&mut self.state.borrow_mut().send);
                let mut disconnect = false;
                for packet in packets {
                    disconnect |= matches!(packet, Packet::Disconnect(_));
                    broker.send(packet).await;
                }
                if disconnect {
                    break;
                }
                let packet = future::or(async { Some(broker.next_optional().await) }, async {
                    listener.await;
                    None
                })
                .await;
                let Some(packet) = packet else {
                    continue;
                };
                let Some(packet) = packet else {
                    break;
                };
                self.state.borrow_mut().seen.push(packet.clone());
                match packet {
                    Packet::Subscribe(subscribe) => {
                        let path = subscribe.filters[0].path.clone();
                        if self.state.borrow().hold.contains(&path) {
                            self.state.borrow_mut().held.push(subscribe);
                        } else {
                            let reason = self
                                .state
                                .borrow()
                                .reject
                                .get(&path)
                                .copied()
                                .unwrap_or(SubscribeReasonCode::QoS2);
                            broker
                                .send(Packet::SubAck(SubAck::new(subscribe.pkid, vec![reason])))
                                .await;
                        }
                    }
                    Packet::Unsubscribe(unsubscribe) => {
                        let mut ack = UnsubAck::new(unsubscribe.pkid);
                        ack.reasons = vec![UnsubAckReason::Success; unsubscribe.filters.len()];
                        broker.send(Packet::UnsubAck(ack)).await;
                    }
                    Packet::PingReq => broker.send(Packet::PingResp).await,
                    _ => {}
                }
            }
        }
    }
}
impl CloudBroker {
    async fn next_optional(&mut self) -> Option<Packet> {
        loop {
            if let Some(packet) = self.frames.next_packet().unwrap() {
                return Some(packet);
            }
            let mut bytes = [0; 4096];
            let len = self.stream.read(&mut bytes).await.ok()?;
            if len == 0 {
                return None;
            }
            self.frames.push(&bytes[..len]).unwrap();
        }
    }
}
fn with_cloud_peer(members: &[&str]) -> (Rig, Async<TcpListener>, Rc<CloudPeer>) {
    let rig = Rig::new(true, members);
    rig.cloud
        .lock()
        .unwrap()
        .online
        .extend(members.iter().map(|did| (*did).to_owned()));
    let listener = Async::<TcpListener>::bind(([127, 0, 0, 1], 0)).unwrap();
    rig.source.context.notification_connector.set(
        super::super::notifications::NotificationConnector::Plaintext(
            listener.get_ref().local_addr().unwrap(),
        ),
    );
    (rig, listener, Rc::new(CloudPeer::default()))
}
fn cloud_active(c: &Control, did: &str) -> bool {
    c.context.live.borrow().devices[did]
        .work
        .as_ref()
        .is_some_and(|work| {
            matches!(
                *work.active.borrow(),
                Some(super::super::notifications::NotificationSource::Cloud(_))
            )
        })
}
fn local_active(c: &Control, did: &str) -> bool {
    c.context.live.borrow().devices[did]
        .work
        .as_ref()
        .is_some_and(|work| {
            matches!(
                *work.active.borrow(),
                Some(super::super::notifications::NotificationSource::Local(_))
            )
        })
}

#[test]
fn http_pause_keeps_cloud_notifications_and_online_reports_cannot_resume_http() {
    let (rig, listener, peer) = with_cloud_peer(&["one"]);
    block_on(rig.run(|c| async move {
        future::or(peer.run(&listener), async {
            until(|| cloud_active(&c, "one")).await;
            c.clock
                .automatic
                .store(false, std::sync::atomic::Ordering::Relaxed);
            let one = function(&c.context.registry, "one", "main");
            c.cloud.lock().unwrap().property_status = 503;
            assert!(one.execute(Command::On).await.is_err());
            assert!(c.context.http_paused.get());
            peer.property("one", 2, true);
            until(|| one.snapshot().power == Power::On).await;
            peer.online("one", false);
            until(|| {
                !c.context.live.borrow().devices["one"]
                    .cloud_access
                    .as_ref()
                    .unwrap()
                    .online
            })
            .await;
            assert!(c.context.http_paused.get());
            peer.online("one", true);
            until(|| {
                c.context.live.borrow().devices["one"]
                    .cloud_access
                    .as_ref()
                    .unwrap()
                    .online
                    && cloud_active(&c, "one")
            })
            .await;
            peer.property("one", 2, false);
            until(|| one.snapshot().power == Power::Off).await;
            assert!(c.context.http_paused.get());
            assert!(matches!(
                one.snapshot().availability,
                Availability::Unavailable(_)
            ));
            assert_eq!(c.cloud.lock().unwrap().property_writes.len(), 1);
        })
        .await;
    }));
}

#[test]
fn cloud_forbidden_and_scope_failure_revoke_properties_without_synthesizing_device_info() {
    for scope_failure in [false, true] {
        let (rig, listener, peer) = with_cloud_peer(&["one"]);
        block_on(rig.run(|c| async move {
            future::or(peer.run(&listener), async {
                until(|| cloud_active(&c, "one")).await;
                let one = function(&c.context.registry, "one", "main");
                c.clock
                    .automatic
                    .store(false, std::sync::atomic::Ordering::Relaxed);
                if scope_failure {
                    c.context.cloud_scope_valid.set(false);
                    c.context.changed.notify(usize::MAX);
                } else {
                    c.cloud.lock().unwrap().property_code = -704_012_904;
                    assert!(one.execute(Command::On).await.is_err());
                }
                until(|| !cloud_active(&c, "one")).await;
                let snapshot = one.snapshot();
                peer.property("one", 2, true);
                peer.online("one", true);
                async_io::Timer::after(Duration::from_millis(20)).await;
                assert_eq!(one.snapshot(), snapshot);
                if !scope_failure {
                    assert!(c.context.live.borrow().devices["one"].cloud_forbidden);
                    c.context
                        .live
                        .borrow_mut()
                        .devices
                        .get_mut("one")
                        .unwrap()
                        .cloud_access = None;
                    peer.online("one", true);
                    async_io::Timer::after(Duration::from_millis(20)).await;
                    assert!(
                        c.context.live.borrow().devices["one"]
                            .cloud_access
                            .is_none()
                    );
                }
            })
            .await;
        }));
    }
}

#[test]
fn partial_cloud_preparation_keeps_local_and_preparing_qos2_cannot_cross_activation() {
    let (rig, listener, cloud) = with_cloud_peer(&["multi"]);
    let held = "device/multi/up/properties_changed/9/1";
    cloud.state.borrow_mut().hold.insert(held.into());
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            c.announce();
            let control = &c;
            let cloud = &cloud;
            c.peer(Rc::new(Peer::default()), |local| async move {
                initialized(control, &local, &["multi"]).await;
                until(|| local_active(control, "multi")).await;
                control
                    .clock
                    .automatic
                    .store(false, std::sync::atomic::Ordering::Relaxed);
                async_io::Timer::after(Duration::from_millis(20)).await;
                let paths = [
                    "device/multi/up/properties_changed/2/1",
                    "device/multi/up/properties_changed/3/1",
                    "device/multi/up/properties_changed/4/1",
                    held,
                ];
                let counts = paths.map(|path| cloud.count(path));
                let handle = control
                    .context
                    .live
                    .borrow()
                    .session
                    .as_ref()
                    .unwrap()
                    .handle
                    .clone();
                control.context.local_outcome(
                    &handle,
                    &crate::xiaomi::central::RequestOutcome::<()> {
                        result: Err(crate::xiaomi::central::CentralError::Timeout),
                        stage: crate::xiaomi::central::RequestStage::MayHaveBeenSent,
                        timeout: Some(crate::xiaomi::central::RequestTimeout::Response),
                    },
                );
                until(|| {
                    cloud
                        .state
                        .borrow()
                        .held
                        .iter()
                        .any(|subscribe| subscribe.filters[0].path == held)
                })
                .await;
                assert!(local_active(control, "multi"));
                let outlet = function(&control.context.registry, "multi", "outlet_1");
                cloud.qos2("multi", 3, true, 41);
                until(|| cloud.received(41, false)).await;
                local.notify("multi", 3, true);
                until(|| outlet.snapshot().power == Power::On).await;
                cloud.state.borrow_mut().hold.remove(held);
                cloud.release(held);
                until(|| cloud_active(control, "multi")).await;
                until(|| outlet.snapshot().power == Power::Off).await;
                cloud.send(Packet::PubRel(PubRel::new(41)));
                until(|| cloud.received(41, true)).await;
                assert_eq!(outlet.snapshot().power, Power::Off);
                cloud.property("multi", 3, true);
                until(|| outlet.snapshot().power == Power::On).await;
                for (path, count) in paths.into_iter().zip(counts) {
                    assert_eq!(cloud.count(path), count + 1);
                }
            })
            .await;
        })
        .await;
    }));
}

#[test]
fn local_cloud_local_delivery_epochs_do_not_reactivate_an_old_pubrel() {
    let (rig, listener, cloud) = with_cloud_peer(&["one"]);
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            c.announce();
            let control = &c;
            let cloud = &cloud;
            c.peer(Rc::new(Peer::default()), |local| async move {
                initialized(control, &local, &["one"]).await;
                until(|| local_active(control, "one")).await;
                control
                    .context
                    .priority
                    .set(super::super::routing::PathPriority::CloudFirst);
                control.context.changed.notify(usize::MAX);
                until(|| cloud_active(control, "one")).await;
                cloud.qos2("one", 2, true, 45);
                until(|| cloud.received(45, false)).await;
                control
                    .context
                    .priority
                    .set(super::super::routing::PathPriority::LocalFirst);
                control.context.changed.notify(usize::MAX);
                until(|| local_active(control, "one")).await;
                local.notify("one", 2, false);
                until(|| {
                    function(&control.context.registry, "one", "main")
                        .snapshot()
                        .power
                        == Power::Off
                })
                .await;
                control
                    .context
                    .priority
                    .set(super::super::routing::PathPriority::CloudFirst);
                control.context.changed.notify(usize::MAX);
                until(|| cloud_active(control, "one")).await;
                cloud.send(Packet::PubRel(PubRel::new(45)));
                until(|| cloud.received(45, true)).await;
                assert_eq!(
                    function(&control.context.registry, "one", "main")
                        .snapshot()
                        .power,
                    Power::Off
                );
                cloud.property("one", 2, true);
                until(|| {
                    function(&control.context.registry, "one", "main")
                        .snapshot()
                        .power
                        == Power::On
                })
                .await;
            })
            .await;
        })
        .await;
    }));
}

#[test]
fn exhausted_local_preparation_uses_cloud_and_diagnostic_wakes_do_not_restart_it() {
    let (rig, listener, cloud) = with_cloud_peer(&["multi"]);
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            c.announce();
            let control = &c;
            let cloud = &cloud;
            let local = Rc::new(Peer::default());
            local
                .state
                .borrow_mut()
                .sub_reject
                .insert(("multi".into(), 9, 1));
            c.peer(local, |local| async move {
                until(|| {
                    local.count("subscribe", "multi", 9) == 4 && cloud_active(control, "multi")
                })
                .await;
                control
                    .clock
                    .automatic
                    .store(false, std::sync::atomic::Ordering::Relaxed);
                let count = local.count("subscribe", "multi", 9);
                for _ in 0..20 {
                    control.context.changed.notify(usize::MAX);
                    future::yield_now().await;
                }
                assert_eq!(local.count("subscribe", "multi", 9), count);
                cloud.property("multi", 9, true);
                until(|| {
                    function(&control.context.registry, "multi", "usb")
                        .snapshot()
                        .power
                        == Power::On
                })
                .await;
                assert!(
                    control.context.live.borrow().devices["multi"]
                        .work
                        .as_ref()
                        .unwrap()
                        .local_exhausted
                        .get()
                );
                assert_eq!(local.count("subscribe", "multi", 3), 1);
                // A real information update opens one new bounded preparation window.
                local.state.borrow_mut().sub_reject.clear();
                local.list_changed(&["multi"]);
                let due = control.clock.now() + Duration::from_millis(10);
                until(|| control.clock.deadlines.lock().unwrap().contains(&due)).await;
                control.clock.advance(Duration::from_millis(10));
                until(|| local_active(control, "multi")).await;
                assert_eq!(local.count("subscribe", "multi", 9), count + 1);
            })
            .await;
        })
        .await;
    }));
}

#[test]
fn pushless_published_devices_still_read_and_control_while_new_devices_wait_for_push() {
    for restored in [true, false] {
        block_on(Rig::new(restored, &["one"]).run(|c| async move {
            c.announce();
            let local = Rc::new(Peer::default());
            local.state.borrow_mut().pushless.insert("one".into());
            let control = &c;
            c.peer(local, |local| async move {
                if restored {
                    until(|| {
                        function(&control.context.registry, "one", "main")
                            .snapshot()
                            .power
                            == Power::Off
                    })
                    .await;
                    assert_eq!(local.count("get", "one", 2), 1);
                    assert_eq!(local.count("subscribe", "one", 2), 0);
                    function(&control.context.registry, "one", "main")
                        .execute(Command::On)
                        .await
                        .unwrap();
                    assert_eq!(local.count("rpcReq", "one", 2), 1);
                    assert!(
                        !control
                            .context
                            .live
                            .borrow()
                            .session
                            .as_ref()
                            .unwrap()
                            .devices
                            .contains_key("one")
                    );
                } else {
                    until(|| {
                        control
                            .context
                            .status
                            .borrow()
                            .pending
                            .iter()
                            .any(|pending| pending.did == "one" && pending.reason.contains("push"))
                    })
                    .await;
                    assert!(control.context.registry.list().is_empty());
                    assert_eq!(local.count("get", "one", 2), 0);
                }
            })
            .await;
        }));
    }
}

#[test]
fn mqtt_network_retry_caps_at_six_hundred_seconds_and_new_token_resets_it_immediately() {
    let (rig, listener, peer) = with_cloud_peer(&["one"]);
    rig.clock
        .automatic
        .store(false, std::sync::atomic::Ordering::Relaxed);
    peer.state
        .borrow_mut()
        .connacks
        .extend(std::iter::repeat_n(ConnectReturnCode::ServerUnavailable, 9));
    block_on(rig.run(|c| async move {
        future::or(peer.run(&listener), async {
            until(|| peer.state.borrow().connects.len() == 1).await;
            let mut cursor = 0;
            for (index, seconds) in [10, 20, 40, 80, 160, 320, 600].into_iter().enumerate() {
                let due = c.clock.now() + Duration::from_secs(seconds);
                until(|| c.clock.deadlines.lock().unwrap()[cursor..].contains(&due)).await;
                cursor = c.clock.deadlines.lock().unwrap().len();
                c.clock.advance(Duration::from_secs(seconds));
                until(|| peer.state.borrow().connects.len() == index + 2).await;
                assert!(!c.context.http_paused.get());
            }
            let capped = c.clock.now() + Duration::from_secs(600);
            until(|| c.clock.deadlines.lock().unwrap()[cursor..].contains(&capped)).await;
            let mut record = c.store.xiaomi().load().unwrap().unwrap();
            record.tokens.access_token = "replacement-token".into();
            c.store.xiaomi().replace(&record).unwrap();
            c.context.live.borrow_mut().credentials = Some(record);
            cursor = c.clock.deadlines.lock().unwrap().len();
            c.context.credentials_changed.notify(usize::MAX);
            until(|| peer.state.borrow().connects.len() == 9).await;
            assert_eq!(peer.state.borrow().connects[8], "replacement-token");
            let due = c.clock.now() + Duration::from_secs(10);
            until(|| c.clock.deadlines.lock().unwrap()[cursor..].contains(&due)).await;
            c.clock.advance(Duration::from_secs(10));
            until(|| cloud_active(&c, "one")).await;
            let old = c
                .context
                .notifications
                .borrow()
                .as_ref()
                .unwrap()
                .handle
                .clone();
            cursor = c.clock.deadlines.lock().unwrap().len();
            peer.send(Packet::Disconnect(Disconnect::new()));
            until(|| c.context.notifications.borrow().is_none()).await;
            let due = c.clock.now() + Duration::from_secs(10);
            until(|| c.clock.deadlines.lock().unwrap()[cursor..].contains(&due)).await;
            c.clock.advance(Duration::from_secs(10));
            until(|| cloud_active(&c, "one")).await;
            let one = function(&c.context.registry, "one", "main");
            until(|| one.snapshot().power == Power::Off).await;
            c.context.property_event(
                &super::super::notifications::NotificationSource::Cloud(old),
                "one",
                crate::xiaomi::capability::PropertyBinding { siid: 2, piid: 1 },
                true,
            );
            assert_eq!(one.snapshot().power, Power::Off);
            assert_eq!(c.cloud.lock().unwrap().tokens, 0);
        })
        .await;
    }));
}

#[test]
fn connect_and_suback_authorization_share_refresh_and_preserve_local_control() {
    for rejection in 0..3 {
        let connect_rejection = rejection == 0;
        let (rig, listener, cloud) = with_cloud_peer(&["one"]);
        let (release, gate) = flume::bounded(1);
        rig.cloud.lock().unwrap().home_gate = Some(gate);
        if connect_rejection {
            cloud.state.borrow_mut().connacks.extend([
                ConnectReturnCode::NotAuthorized,
                ConnectReturnCode::NotAuthorized,
            ]);
        } else {
            cloud.state.borrow_mut().reject.insert(
                if rejection == 1 {
                    "device/one/up/properties_changed/2/1"
                } else {
                    "device/one/state/#"
                }
                .into(),
                SubscribeReasonCode::NotAuthorized,
            );
        }
        block_on(rig.run(|c| async move {
            future::or(cloud.run(&listener), async {
                c.announce();
                let control = &c;
                let cloud = &cloud;
                c.peer(Rc::new(Peer::default()), |local| async move {
                    initialized(control, &local, &["one"]).await;
                    control.cloud.lock().unwrap().home_gate = None;
                    release.send(()).unwrap();
                    if rejection == 1 {
                        let handle = control
                            .context
                            .live
                            .borrow()
                            .session
                            .as_ref()
                            .unwrap()
                            .handle
                            .clone();
                        control.context.local_outcome(
                            &handle,
                            &crate::xiaomi::central::RequestOutcome::<()> {
                                result: Err(crate::xiaomi::central::CentralError::Timeout),
                                stage: crate::xiaomi::central::RequestStage::MayHaveBeenSent,
                                timeout: Some(crate::xiaomi::central::RequestTimeout::Response),
                            },
                        );
                    }
                    until(|| {
                        matches!(
                            *control.context.cloud_sync.borrow(),
                            super::super::cloud::CloudSync::Paused
                        )
                    })
                    .await;
                    assert_eq!(control.cloud.lock().unwrap().tokens, 1);
                    until(|| control.context.notifications.borrow().is_none()).await;
                    assert!(matches!(
                        control
                            .context
                            .status
                            .borrow()
                            .authentication
                            .authentication,
                        crate::xiaomi::auth::AuthenticationState::SignInRequired(_)
                    ));
                    function(&control.context.registry, "one", "main")
                        .execute(Command::On)
                        .await
                        .unwrap();
                    assert_eq!(local.count("rpcReq", "one", 2), 1);
                    let old = cloud.state.borrow().connects[0].clone();
                    control
                        .context
                        .token_rejected(
                            &old,
                            crate::xiaomi::auth::FailureReason::CloudNotification(
                                crate::xiaomi::cloud::NotificationError::Unauthorized,
                            ),
                        )
                        .unwrap();
                    assert_eq!(control.cloud.lock().unwrap().tokens, 1);
                    assert!(control.cloud.lock().unwrap().property_writes.is_empty());
                })
                .await;
            })
            .await;
        }));
    }
}

#[test]
fn driver_authorization_rejection_rebuilds_with_fresh_token_then_pauses_on_second_rejection() {
    let (rig, listener, peer) = with_cloud_peer(&["one"]);
    block_on(rig.run(|c| async move {
        future::or(peer.run(&listener), async {
            until(|| cloud_active(&c, "one")).await;
            let mut disconnect = Disconnect::new();
            disconnect.reason_code = DisconnectReasonCode::NotAuthorized;
            peer.send(Packet::Disconnect(disconnect.clone()));
            until(|| peer.state.borrow().connects.len() == 2 && cloud_active(&c, "one")).await;
            assert_eq!(peer.state.borrow().connects[1], "fresh-access");
            assert_eq!(c.cloud.lock().unwrap().tokens, 1);
            peer.send(Packet::Disconnect(disconnect));
            until(|| {
                matches!(
                    *c.context.cloud_sync.borrow(),
                    super::super::cloud::CloudSync::Paused
                ) && c.context.notifications.borrow().is_none()
            })
            .await;
            assert_eq!(c.cloud.lock().unwrap().tokens, 1);
            assert!(c.cloud.lock().unwrap().property_writes.is_empty());
        })
        .await;
    }));
}

#[test]
fn real_source_handoff_and_cloud_read_preserve_inflight_write_rules_and_fifo() {
    for notify_after_send in [false, true] {
        let (mut rig, listener, cloud) = with_cloud_peer(&["one"]);
        rig.source.request_timeout = Duration::from_secs(2);
        block_on(rig.run(|c| async move {
            future::or(cloud.run(&listener), async {
                c.announce();
                let control = &c;
                let cloud = &cloud;
                c.peer(Rc::new(Peer::default()), |local| async move {
                    initialized(control, &local, &["one"]).await;
                    until(|| local_active(control, "one")).await;
                    control
                        .clock
                        .automatic
                        .store(false, std::sync::atomic::Ordering::Relaxed);
                    let one = function(&control.context.registry, "one", "main");
                    local.state.borrow_mut().hold_writes = true;
                    let mut first = Box::pin(one.execute(Command::On));
                    assert!(poll_once(first.as_mut()).await.is_none());
                    until(|| local.count("rpcReq", "one", 2) == 1).await;
                    let mut second = Box::pin(one.execute(Command::Toggle));
                    assert!(poll_once(second.as_mut()).await.is_none());
                    let reads = control.cloud.lock().unwrap().property_reads.len();
                    let handle = control
                        .context
                        .live
                        .borrow()
                        .session
                        .as_ref()
                        .unwrap()
                        .handle
                        .clone();
                    control.context.local_outcome(
                        &handle,
                        &crate::xiaomi::central::RequestOutcome::<()> {
                            result: Err(crate::xiaomi::central::CentralError::Timeout),
                            stage: crate::xiaomi::central::RequestStage::MayHaveBeenSent,
                            timeout: Some(crate::xiaomi::central::RequestTimeout::Response),
                        },
                    );
                    until(|| {
                        cloud_active(control, "one")
                            && control.cloud.lock().unwrap().property_reads.len() > reads
                    })
                    .await;
                    let work = control.context.live.borrow().devices["one"]
                        .work
                        .clone()
                        .unwrap();
                    until(|| work.refresh_inflight.borrow().is_empty()).await;
                    assert_eq!(one.snapshot().power, Power::Off);
                    if notify_after_send {
                        cloud.property("one", 2, true);
                        until(|| one.snapshot().power == Power::On).await;
                        cloud.property("one", 2, false);
                        until(|| one.snapshot().power == Power::Off).await;
                    }
                    local.release("one", 2, accepted("one", 2, 0)).await;
                    assert_eq!(
                        first.await.unwrap().power,
                        if notify_after_send {
                            Power::Off
                        } else {
                            Power::On
                        }
                    );
                    assert_eq!(
                        second.await.unwrap().power,
                        if notify_after_send {
                            Power::On
                        } else {
                            Power::Off
                        }
                    );
                    assert_eq!(local.count("rpcReq", "one", 2), 1);
                    let cloud = control.cloud.lock().unwrap();
                    assert_eq!(cloud.property_writes.len(), 1);
                    assert_eq!(cloud.property_writes[0].3, notify_after_send);
                })
                .await;
            })
            .await;
        }));
    }
}

#[test]
fn a_real_cloud_online_recovery_reopens_one_exhausted_subscription_window() {
    let (rig, listener, peer) = with_cloud_peer(&["one"]);
    let property = "device/one/up/properties_changed/2/1";
    peer.state
        .borrow_mut()
        .reject
        .insert(property.into(), SubscribeReasonCode::QuotaExceeded);
    block_on(rig.run(|c| async move {
        future::or(peer.run(&listener), async {
            until(|| {
                peer.count(property) == 4
                    && c.context.live.borrow().devices["one"]
                        .work
                        .as_ref()
                        .unwrap()
                        .cloud_exhausted
                        .get()
            })
            .await;
            c.clock
                .automatic
                .store(false, std::sync::atomic::Ordering::Relaxed);
            peer.state.borrow_mut().reject.remove(property);
            peer.online("one", true);
            async_io::Timer::after(Duration::from_millis(20)).await;
            assert_eq!(peer.count(property), 4);
            peer.online("one", false);
            until(|| {
                !c.context.live.borrow().devices["one"]
                    .cloud_access
                    .as_ref()
                    .unwrap()
                    .online
            })
            .await;
            peer.online("one", true);
            until(|| cloud_active(&c, "one")).await;
            assert_eq!(peer.count(property), 5);
            assert!(!c.context.http_paused.get());
        })
        .await;
    }));
}
