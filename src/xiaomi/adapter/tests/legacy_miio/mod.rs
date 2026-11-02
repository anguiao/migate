use super::*;
use crate::{
    device::{
        AirConditionerCommand,
        air_conditioner::{AirConditionerFanSpeed, AirConditionerMode, AirConditionerTarget},
    },
    storage::LanRecord,
    xiaomi::lan::{Token, test_support::LegacyDevice},
};
use std::sync::atomic::Ordering;

fn instance() -> Value {
    serde_json::from_str(include_str!(
        "../../../../../tests/fixtures/miot/lumi-mcn02-synthetic.json"
    ))
    .unwrap()
}

fn setup(did: &str) -> (Rig, LegacyDevice) {
    setup_with_description(did, instance())
}

fn setup_with_description(did: &str, description: Value) -> (Rig, LegacyDevice) {
    let rig = Rig::custom_lights(&[(did, "lumi.acpartner.mcn02", description)]);
    let token = Token::parse("00112233445566778899aabbccddeeff").unwrap();
    let peer = LegacyDevice::new(token.clone(), did.parse().unwrap());
    let scope = rig
        .store
        .devices()
        .load_configuration()
        .unwrap()
        .scope
        .unwrap();
    rig.store
        .lan()
        .save(&LanRecord {
            scope,
            did: did.into(),
            model: "lumi.acpartner.mcn02".into(),
            address: "127.0.0.1".parse().unwrap(),
            token,
        })
        .unwrap();
    rig.source.context.lan_port.set(peer.address.port());
    rig.clock.automatic.store(true, Ordering::Relaxed);
    (rig, peer)
}

mod commands;
mod feedback;
mod polling;
mod routing;
