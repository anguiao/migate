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
    json!({"type":"urn:miot-spec-v2:device:air-conditioner:0000A004:lumi-mcn02:1", "services":[
        {"iid":2,"type":"urn:miot-spec-v2:service:air-conditioner:0000780F:lumi-mcn02:1", "properties":[
            {"iid":1,"type":"urn:miot-spec-v2:property:on:00000006:lumi-mcn02:1","format":"bool","access":["read","write","notify"]},
            {"iid":2,"type":"urn:miot-spec-v2:property:mode:00000008:lumi-mcn02:1","format":"uint8","access":["read","write","notify"],"value-list":[{"value":0,"description":"Auto"},{"value":1,"description":"Cool"},{"value":2,"description":"Dry"},{"value":3,"description":"Heat"},{"value":4,"description":"Fan"}]},
            {"iid":3,"type":"urn:miot-spec-v2:property:target-temperature:00000021:lumi-mcn02:1","format":"float","access":["read","write","notify"],"unit":"celsius","value-range":[16,30,1]}
        ]},
        {"iid":3,"type":"urn:miot-spec-v2:service:fan-control:00007809:lumi-mcn02:1", "properties":[
            {"iid":1,"type":"urn:miot-spec-v2:property:fan-level:00000016:lumi-mcn02:1","format":"uint8","access":["read","write","notify"],"value-list":[{"value":0,"description":"Auto"},{"value":1,"description":"Low"},{"value":2,"description":"Medium"},{"value":3,"description":"High"}]},
            {"iid":2,"type":"urn:miot-spec-v2:property:vertical-swing:00000018:lumi-mcn02:1","format":"bool","access":["read","write","notify"]}
        ]}
    ]})
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
