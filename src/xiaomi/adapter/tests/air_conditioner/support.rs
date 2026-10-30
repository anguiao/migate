use super::*;

pub(super) const POWER: PropertyBinding = PropertyBinding { siid: 3, piid: 1 };
pub(super) const MODE: PropertyBinding = PropertyBinding { siid: 3, piid: 2 };
pub(super) const TEMP: PropertyBinding = PropertyBinding { siid: 3, piid: 4 };
pub(super) const SPEED: PropertyBinding = PropertyBinding { siid: 4, piid: 2 };
pub(super) const SWING: PropertyBinding = PropertyBinding { siid: 4, piid: 4 };
pub(super) const ID: &str = "air-conditioner:3";

pub(super) fn rig() -> Rig {
    let instance = serde_json::from_str(include_str!(
        "../../../../../tests/fixtures/miot/lumi-mcn04.json"
    ))
    .unwrap();
    Rig::custom_lights(&[("one", "generic.ac.model", instance)])
}
pub(super) fn work(c: &Control) -> Rc<super::work::DeviceWork> {
    c.context.live.borrow().devices["one"]
        .admitted
        .as_ref()
        .unwrap()
        .clone()
}
pub(super) fn snapshot(c: &Control) -> AirConditionerSnapshot {
    function(&c.context.registry, "one", ID)
        .snapshot()
        .air_conditioner()
        .unwrap()
}
pub(super) fn peer() -> Rc<Peer> {
    let peer = Rc::new(Peer::default());
    for (binding, value) in [
        (POWER, json!(false)),
        (MODE, json!(0)),
        (TEMP, json!(24.0)),
        (SPEED, json!(0)),
        (SWING, json!(false)),
    ] {
        peer.value("one", binding, value);
    }
    peer
}
pub(super) fn writes(peer: &Peer) -> Vec<(Value, Value, Value)> {
    peer.state
        .borrow()
        .seen
        .iter()
        .filter(|(op, value)| op == "rpcReq" && value["did"] == "one")
        .map(|(_, value)| {
            let item = &value["rpc"]["params"][0];
            (
                item["siid"].clone(),
                item["piid"].clone(),
                item["value"].clone(),
            )
        })
        .collect()
}
pub(super) async fn accept(peer: &Peer, binding: PropertyBinding) {
    peer.release_property(
        "one",
        binding,
        json!({"result":[{"did":"one","siid":binding.siid,"piid":binding.piid,"code":0}]}),
    )
    .await;
}

pub(super) fn restrictions(cool_temperatures: Vec<i16>) -> AirConditionerConstraints {
    AirConditionerConstraints::new(
        TemperatureGrid::new(16, 30, 1).unwrap(),
        [
            (
                Mode::Cool,
                cool_temperatures,
                vec![Speed::Auto, Speed::Low, Speed::High],
            ),
            (Mode::Heat, vec![24, 26], vec![Speed::Low, Speed::High]),
            (Mode::Auto, vec![], vec![Speed::Auto, Speed::Low]),
            (Mode::Dry, vec![], vec![Speed::Low]),
        ],
    )
    .unwrap()
}

pub(super) const MCN_STATE: PropertyBinding = PropertyBinding { siid: 8, piid: 7 };
pub(super) const MCN_RANGE: PropertyBinding = PropertyBinding { siid: 8, piid: 6 };

pub(super) fn mcn04_rig() -> Rig {
    let instance = serde_json::from_str(include_str!(
        "../../../../../tests/fixtures/miot/lumi-mcn04.json"
    ))
    .unwrap();
    Rig::custom_lights(&[("one", "lumi.acpartner.mcn04", instance)])
}
pub(super) fn mcn04_range() -> Value {
    json!(
        json!({"models":[
            {"mode":0,"speeds":[0,1,2,3,99],"temps":[16,24,26,30,99],"directs":[1],"type":2},
            {"mode":1,"speeds":[0,1,2,3],"temps":[24,26],"directs":[1],"type":2},
            {"mode":2,"speeds":[0,1,2,3],"temps":[],"directs":[1],"type":2},
            {"mode":3,"speeds":[0,1,2,3],"temps":[24,26],"directs":[1],"type":2},
            {"mode":4,"speeds":[1],"temps":[24,26],"directs":[1],"type":2}
        ]})
        .to_string()
    )
}
pub(super) fn mcn04_peer() -> Rc<Peer> {
    let peer = Rc::new(Peer::default());
    peer.value("one", MCN_STATE, json!("P0_M0_T24_S1_D1"));
    peer.value("one", MCN_RANGE, mcn04_range());
    peer
}
