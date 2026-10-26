//! Native state observations through real local and cloud source sessions.
use super::super::{model::SubscriptionBinding, notifications::NotificationSource};
use super::*;
use crate::device::{Availability, StateInput, StateSnapshot};
use crate::xiaomi::capability::{EventBinding, PropertyBinding};

const CONTACT: PropertyBinding = PropertyBinding { siid: 2, piid: 2 };
const DURATION: PropertyBinding = PropertyBinding {
    siid: 2,
    piid: 1024,
};
const MOTION: EventBinding = EventBinding {
    siid: 2,
    eiid: 1008,
};
const ILLUMINANCE: PropertyBinding = PropertyBinding {
    siid: 2,
    piid: 1005,
};
const OCCUPANCY: PropertyBinding = PropertyBinding { siid: 2, piid: 1 };

fn state_rig(model: &str, fixture: &str) -> Rig {
    Rig::custom_lights(&[("one", model, serde_json::from_str(fixture).unwrap())])
}
fn contact_rig() -> Rig {
    state_rig(
        "isa.magnet.dw2hl",
        include_str!("../../../../../tests/fixtures/miot/isa-dw2hl.json"),
    )
}
fn motion_rig() -> Rig {
    state_rig(
        "xiaomi.motion.pir1",
        include_str!("../../../../../tests/fixtures/miot/xiaomi-pir1.json"),
    )
}
fn occupancy_rig() -> Rig {
    state_rig(
        "linp.sensor_occupy.hb01",
        include_str!("../../../../../tests/fixtures/miot/linp-hb01.json"),
    )
}
fn state(c: &Control, id: &str) -> StateSnapshot {
    function(&c.context.registry, "one", id)
        .snapshot()
        .state_sensor()
        .unwrap()
}
fn work(c: &Control) -> Rc<super::super::work::DeviceWork> {
    c.context.live.borrow().devices["one"]
        .admitted
        .as_ref()
        .unwrap()
        .clone()
}
fn event(peer: &Peer, retain: bool, source_time: Option<i64>) {
    let client = peer.state.borrow().client_id.clone();
    let mut payload = json!({"did":"one","siid":MOTION.siid,"eiid":MOTION.eiid,"arguments":[{"piid":1005,"value":999.0}]});
    if let Some(time) = source_time {
        payload["timestamp"] = json!(time);
    }
    let mut publish = Publish::new(
        format!(
            "{client}/appMsg/notify/iot/one/event/{}.{}",
            MOTION.siid, MOTION.eiid
        ),
        QoS::AtMostOnce,
        encode_mips(900, None, &payload),
    );
    publish.retain = retain;
    peer.send(Packet::Publish(publish));
}
async fn ready(c: &Control, id: &str) {
    until(|| {
        c.context
            .diagnostics()
            .devices
            .iter()
            .find(|d| d.did == "one")
            .is_some_and(|d| {
                d.functions
                    .iter()
                    .find(|f| f.function == id)
                    .is_some_and(|f| f.notification_source == Some(PathKind::Local))
            })
    })
    .await;
}

fn reads(peer: &Peer, binding: PropertyBinding) -> usize {
    peer.state
        .borrow()
        .seen
        .iter()
        .filter(|(op, value)| {
            op == "get"
                && value["did"] == "one"
                && value["siid"] == binding.siid
                && value["piid"] == binding.piid
        })
        .count()
}
fn illumination(c: &Control) -> Option<f64> {
    function(&c.context.registry, "one", "illuminance")
        .snapshot()
        .measurement()
        .unwrap()
        .value
}
async fn settled(c: &Control, id: &str, binding: PropertyBinding) {
    ready(c, id).await;
    until(|| !work(c).refresh.borrow().contains(id, binding)).await;
}
fn motion_peer() -> Rc<Peer> {
    let peer = Rc::new(Peer::default());
    peer.value("one", DURATION, json!(300));
    peer.value("one", ILLUMINANCE, json!(7.5));
    peer
}

fn held_contact_rig() -> Rig {
    let mut rig = contact_rig();
    rig.source.request_timeout = Duration::from_secs(2);
    rig
}
fn held_contact_peer() -> Rc<Peer> {
    let peer = Rc::new(Peer::default());
    peer.value("one", CONTACT, json!(true));
    peer.state
        .borrow_mut()
        .sub_timeout
        .insert(("one".into(), 2, 2));
    peer
}
fn accept_contact(peer: &Peer) {
    let sub = peer.state.borrow_mut().held_subs.remove(0);
    peer.send(Packet::SubAck(SubAck::new(
        sub.pkid,
        vec![SubscribeReasonCode::QoS2],
    )));
}

fn cloud_rig(rig: Rig) -> (Rig, Async<TcpListener>, Rc<CloudPeer>) {
    let listener = Async::<TcpListener>::bind(([127, 0, 0, 1], 0)).unwrap();
    rig.source.context.notification_connector.set(
        super::super::notifications::NotificationConnector::Plaintext(
            listener.get_ref().local_addr().unwrap(),
        ),
    );
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.online.insert("one".into());
        cloud.raw_values.insert(("one".into(), 2, 1024), json!(300));
        cloud.raw_values.insert(("one".into(), 2, 1005), json!(7.5));
    }
    (rig, listener, Rc::new(CloudPeer::default()))
}
fn cloud_event(peer: &CloudPeer, source_time: Option<i64>) {
    let mut payload = json!({"params":{"did":"one","siid":2,"eiid":1008,"arguments":[]}});
    if let Some(time) = source_time {
        payload["timestamp"] = json!(time);
    }
    peer.send(Packet::Publish(Publish::new(
        "device/one/up/event_occured/2/1008",
        QoS::AtMostOnce,
        payload.to_string(),
    )));
}
async fn cloud_ready(c: &Control, id: &str) {
    until(|| {
        c.context
            .diagnostics()
            .devices
            .iter()
            .find(|d| d.did == "one")
            .is_some_and(|d| {
                d.functions
                    .iter()
                    .find(|f| f.function == id)
                    .is_some_and(|f| f.notification_source == Some(PathKind::Cloud))
            })
    })
    .await;
}
fn cloud_reads(c: &Control, binding: PropertyBinding) -> usize {
    c.cloud
        .lock()
        .unwrap()
        .property_reads
        .iter()
        .filter(|(did, siid, piid)| did == "one" && *siid == binding.siid && *piid == binding.piid)
        .count()
}

mod baseline;
mod catalog;
mod observations;
mod ordering;
mod sources;
mod terminal;
