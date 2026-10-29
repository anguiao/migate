//! Curtain integration through the existing local and cloud transports.
use super::*;
use crate::device::{CurtainMotion, CurtainSnapshot};
use crate::xiaomi::{
    adapter::notifications::{NotificationConnector, NotificationSource},
    capability::PropertyBinding,
};

const STATUS: PropertyBinding = PropertyBinding { siid: 2, piid: 2 };
const CURRENT: PropertyBinding = PropertyBinding { siid: 2, piid: 3 };
const TARGET: PropertyBinding = PropertyBinding { siid: 2, piid: 4 };

fn instance() -> Value {
    serde_json::from_str(include_str!(
        "../../../../../tests/fixtures/miot/xiaomi-acn010.json"
    ))
    .unwrap()
}
fn rig() -> Rig {
    Rig::custom_lights(&[("one", "xiaomi.curtain.acn010", instance())])
}
fn work(c: &Control) -> Rc<super::work::DeviceWork> {
    c.context.live.borrow().devices["one"]
        .admitted
        .as_ref()
        .unwrap()
        .clone()
}
fn curtain(c: &Control) -> CurtainSnapshot {
    function(&c.context.registry, "one", "curtain:2")
        .snapshot()
        .curtain()
        .unwrap()
}
fn peer() -> Rc<Peer> {
    let peer = Rc::new(Peer::default());
    peer.value("one", STATUS, json!(0));
    peer.value("one", CURRENT, json!(37));
    peer.value("one", TARGET, json!(80));
    peer
}

async fn settled(c: &Control) {
    until(|| {
        let value = curtain(c);
        value.current_position.value.is_some()
            && value.target_position.value.is_some()
            && value.status.value.is_some()
            && work(c).function_observer("curtain:2").is_some()
            && [STATUS, CURRENT, TARGET]
                .iter()
                .all(|binding| !work(c).refresh.borrow().contains("curtain:2", *binding))
    })
    .await;
}

fn cloud_rig() -> (Rig, Async<TcpListener>, Rc<CloudPeer>) {
    let rig = rig();
    let listener = Async::<TcpListener>::bind(([127, 0, 0, 1], 0)).unwrap();
    rig.source
        .context
        .notification_connector
        .set(NotificationConnector::Plaintext(
            listener.get_ref().local_addr().unwrap(),
        ));
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.online.insert("one".into());
        for (binding, value) in [(STATUS, 2), (CURRENT, 25), (TARGET, 75)] {
            cloud
                .raw_values
                .insert(("one".into(), binding.siid, binding.piid), json!(value));
        }
    }
    (rig, listener, Rc::new(CloudPeer::default()))
}

async fn terminal(c: &Control, command: &str) -> String {
    let status = crate::terminal::AuthStatus::new(
        c.context.status.borrow().authentication.clone(),
        "/bin/migate".into(),
        "/data".into(),
    );
    status.update_adapter(c.context.diagnostics());
    crate::terminal::bridge::Context::committed(c.publication.clone(), &status)
        .handle_line(command, 0)
        .await
        .unwrap()
        .unwrap()
}

mod commands;
mod diagnostics;
mod lifecycle;
mod observations;
mod sources;
