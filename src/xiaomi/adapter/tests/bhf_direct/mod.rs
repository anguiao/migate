use super::*;
use crate::{
    device::{Command, CommandError, FanCommand, TemperatureSourceState},
    storage::LanRecord,
    xiaomi::{
        adapter::model::{LanState, PathKind},
        capability::PropertyBinding,
        lan::{Token, tests::support::LegacyDevice},
    },
};
use std::sync::atomic::Ordering;

mod control;
mod cooperation;
mod operations;
mod recovery;

const DID: &str = "743765151";
const MODEL: &str = "yeelink.bhf_light.v13";
const BLOW: &str = "fan:3:2";
const VENTILATION: &str = "fan:3:4";
const HEATER: &str = "heater:3";
const BLOW_POWER: PropertyBinding = PropertyBinding { siid: 3, piid: 2 };
const HEATING: PropertyBinding = PropertyBinding { siid: 3, piid: 3 };
const VENTILATION_POWER: PropertyBinding = PropertyBinding { siid: 3, piid: 4 };

fn v13() -> Value {
    serde_json::from_str(include_str!(
        "../../../../../tests/fixtures/miot/yeelink-bhf-light-v13.json"
    ))
    .unwrap()
}

fn setup() -> (Rig, LegacyDevice) {
    setup_instance(v13())
}

fn setup_instance(instance: Value) -> (Rig, LegacyDevice) {
    let rig =
        Rig::custom_lights(&[(DID, MODEL, instance)]).with_stall_timeout(Duration::from_secs(20));
    let token = Token::parse("00112233445566778899aabbccddeeff").unwrap();
    let peer = LegacyDevice::bhf_v13(token.clone(), DID.parse().unwrap());
    rig.store
        .lan()
        .save(&LanRecord {
            scope: rig
                .store
                .devices()
                .load_configuration()
                .unwrap()
                .scope
                .unwrap(),
            did: DID.into(),
            model: MODEL.into(),
            address: "127.0.0.1".parse().unwrap(),
            token,
        })
        .unwrap();
    rig.network.lan_port.set(peer.address.port());
    rig.clock.automatic.store(true, Ordering::Relaxed);
    (rig, peer)
}

fn function(c: &Control, id: &str) -> crate::device::Function {
    super::support::function(&c.context, DID, id)
}

fn work(c: &Control) -> Rc<crate::xiaomi::adapter::work::DeviceWork> {
    c.context.directory.borrow().devices[DID]
        .work
        .as_ref()
        .unwrap()
        .clone()
}

#[track_caller]
fn eventually(predicate: impl FnMut() -> bool) -> impl Future<Output = ()> {
    until_with_timeout(Duration::from_secs(8), predicate)
}

fn standard_peer() -> Rc<Peer> {
    let peer = Rc::new(Peer::default());
    for (binding, value) in [
        (PropertyBinding { siid: 2, piid: 1 }, json!(false)),
        (PropertyBinding { siid: 2, piid: 2 }, json!(0)),
        (PropertyBinding { siid: 2, piid: 3 }, json!(50)),
        (BLOW_POWER, json!(true)),
        (PropertyBinding { siid: 3, piid: 4 }, json!(true)),
        (HEATING, json!(false)),
        (PropertyBinding { siid: 3, piid: 8 }, json!(25)),
        (PropertyBinding { siid: 3, piid: 1 }, json!(0)),
    ] {
        peer.value(DID, binding, value);
    }
    peer
}

fn hub_writes(peer: &Peer) -> Vec<(u32, u32, Value)> {
    peer.state
        .borrow()
        .seen
        .iter()
        .filter(|(operation, _)| operation == "rpcReq")
        .map(|(_, request)| {
            let item = &request["rpc"]["params"][0];
            (
                item["siid"].as_u64().unwrap() as u32,
                item["piid"].as_u64().unwrap() as u32,
                item["value"].clone(),
            )
        })
        .collect()
}

fn direct_methods(peer: &LegacyDevice, method: &str) -> Vec<Value> {
    peer.requests
        .lock()
        .unwrap()
        .iter()
        .filter(|request| request["method"] == method)
        .cloned()
        .collect()
}
