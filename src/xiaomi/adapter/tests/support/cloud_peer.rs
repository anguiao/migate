use super::*;
use futures_lite::io::{AsyncReadExt, AsyncWriteExt};

pub(in super::super) struct CloudBroker {
    pub(in super::super) stream: Async<std::net::TcpStream>,
    pub(in super::super) frames: crate::xiaomi::mqtt::Frames,
}
impl CloudBroker {
    pub(in super::super) async fn accept(listener: &Async<TcpListener>) -> Self {
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
    pub(in super::super) async fn next(&mut self) -> Packet {
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
    pub(in super::super) async fn send(&mut self, packet: Packet) {
        self.stream
            .write_all(&crate::xiaomi::mqtt::encode_packet(&packet).unwrap())
            .await
            .unwrap();
        self.stream.flush().await.unwrap();
    }
    pub(in super::super) async fn ack(&mut self, subscribe: Subscribe) {
        self.send(Packet::SubAck(SubAck::new(
            subscribe.pkid,
            vec![SubscribeReasonCode::QoS2],
        )))
        .await;
    }
    pub(in super::super) async fn property(&mut self, did: &str, siid: u32, value: bool) {
        self.send(Packet::Publish(Publish::new(
            format!("device/{did}/up/properties_changed/{siid}/1"),
            QoS::AtMostOnce,
            json!({"params":{"did":did,"siid":siid,"piid":1,"value":value}}).to_string(),
        )))
        .await;
    }
}
#[derive(Default)]
pub(in super::super) struct CloudPeerState {
    pub(in super::super) connects: Vec<String>,
    pub(in super::super) connacks: std::collections::VecDeque<ConnectReturnCode>,
    pub(in super::super) seen: Vec<Packet>,
    pub(in super::super) held: Vec<Subscribe>,
    pub(in super::super) peak_held: usize,
    pub(in super::super) hold: BTreeSet<String>,
    pub(in super::super) reject: BTreeMap<String, SubscribeReasonCode>,
    pub(in super::super) send: Vec<Packet>,
}
#[derive(Default)]
pub(in super::super) struct CloudPeer {
    pub(in super::super) state: RefCell<CloudPeerState>,
    pub(in super::super) changed: Event,
}
impl CloudPeer {
    pub(in super::super) fn send(&self, packet: Packet) {
        self.state.borrow_mut().send.push(packet);
        self.changed.notify(usize::MAX);
    }
    pub(in super::super) fn property(&self, did: &str, siid: u32, value: bool) {
        self.property_at(did, siid, 1, json!(value));
    }
    pub(in super::super) fn property_at(&self, did: &str, siid: u32, piid: u32, value: Value) {
        self.send(Packet::Publish(Publish::new(
            format!("device/{did}/up/properties_changed/{siid}/{piid}"),
            QoS::AtMostOnce,
            json!({"params":{"did":did,"siid":siid,"piid":piid,"value":value}}).to_string(),
        )));
    }
    pub(in super::super) fn qos2(&self, did: &str, siid: u32, value: bool, pkid: u16) {
        let mut publish = Publish::new(
            format!("device/{did}/up/properties_changed/{siid}/1"),
            QoS::ExactlyOnce,
            json!({"params":{"did":did,"siid":siid,"piid":1,"value":value}}).to_string(),
        );
        publish.pkid = pkid;
        self.send(Packet::Publish(publish));
    }
    pub(in super::super) fn online(&self, did: &str, online: bool) {
        self.send(Packet::Publish(Publish::new(
            format!(
                "device/{did}/state/{}",
                if online { "online" } else { "offline" }
            ),
            QoS::AtMostOnce,
            json!({"device_id":did,"event":if online {"online"} else {"offline"}}).to_string(),
        )));
    }
    pub(in super::super) fn release(&self, path: &str) {
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
    pub(in super::super) fn count(&self, path: &str) -> usize {
        self.state.borrow().seen.iter().filter(|packet|matches!(packet,Packet::Subscribe(subscribe) if subscribe.filters[0].path==path)).count()
    }
    pub(in super::super) fn received(&self, pkid: u16, complete: bool) -> bool {
        self.state.borrow().seen.iter().any(|packet| {
            if complete {
                matches!(packet,Packet::PubComp(ack) if ack.pkid==pkid)
            } else {
                matches!(packet,Packet::PubRec(ack) if ack.pkid==pkid)
            }
        })
    }
    pub(in super::super) async fn run(&self, listener: &Async<TcpListener>) {
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
                            let mut state = self.state.borrow_mut();
                            state.held.push(subscribe);
                            state.peak_held = state.peak_held.max(state.held.len());
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
    pub(in super::super) async fn next_optional(&mut self) -> Option<Packet> {
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
pub(in super::super) fn with_cloud_peer(
    members: &[&str],
) -> (Rig, Async<TcpListener>, Rc<CloudPeer>) {
    with_cloud_rig(Rig::new(true, members))
}
pub(in super::super) fn with_cloud_sensors(
    members: &[&str],
) -> (Rig, Async<TcpListener>, Rc<CloudPeer>) {
    with_cloud_rig(Rig::sensors(members))
}
fn with_cloud_rig(rig: Rig) -> (Rig, Async<TcpListener>, Rc<CloudPeer>) {
    let members = rig.cloud.lock().unwrap().members.clone();
    rig.cloud.lock().unwrap().online.extend(members);
    let listener = Async::<TcpListener>::bind(([127, 0, 0, 1], 0)).unwrap();
    rig.source.context.notification_connector.set(
        crate::xiaomi::adapter::notifications::NotificationConnector::Plaintext(
            listener.get_ref().local_addr().unwrap(),
        ),
    );
    (rig, listener, Rc::new(CloudPeer::default()))
}
pub(in super::super) fn cloud_active(c: &Control, did: &str) -> bool {
    c.context.live.borrow().devices[did]
        .admitted
        .as_ref()
        .is_some_and(|work| work.test_observer_is_cloud())
}
