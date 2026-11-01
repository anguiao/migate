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

#[test]
fn mcn02_offline_direct_batches_feedback_and_uses_legacy_commands() {
    let did = "308540429";
    let (rig, peer) = setup(did);
    block_on(rig.run(|control| {
        Box::pin(async move {
            let ac = function(&control.context.registry, did, "air-conditioner:2");
            until(|| {
                let state = ac.snapshot().air_conditioner().unwrap();
                state.power.value == Some(false)
                    && state.mode.value == Some(AirConditionerMode::Cool)
                    && state.temperature.value == Some(24)
                    && state.speed.value == Some(AirConditionerFanSpeed::Auto)
                    && state.swing.value == Some(false)
            })
            .await;
            let requests = peer.requests.lock().unwrap().clone();
            let reads = requests
                .iter()
                .filter(|request| request["method"] == "get_prop")
                .collect::<Vec<_>>();
            assert_eq!(
                reads.len(),
                2,
                "one authentication read and one merged feedback read: {requests:?}"
            );
            assert!(reads.iter().all(|request| request["params"]
                == json!(["power", "mode", "tar_temp", "fan_level", "ver_swing"])));
            let before = ac.snapshot();
            peer.hold_read.store(true, Ordering::Relaxed);
            ac.begin_air_conditioner(AirConditionerCommand::Temperature(26))
                .unwrap()
                .accepted()
                .await
                .unwrap();
            assert_eq!(
                ac.snapshot(),
                before,
                "a successful command receipt is not a state observation"
            );
            peer.hold_read.store(false, Ordering::Relaxed);
            until(|| ac.snapshot().air_conditioner().unwrap().temperature.value == Some(26)).await;
            ac.begin_air_conditioner(AirConditionerCommand::Combined(AirConditionerTarget {
                mode: Some(AirConditionerMode::Heat),
                temperature: Some(27),
                speed: Some(AirConditionerFanSpeed::High),
                swing: Some(true),
            }))
            .unwrap()
            .accepted()
            .await
            .unwrap();
            let writes = peer
                .requests
                .lock()
                .unwrap()
                .iter()
                .filter(|request| request["method"].as_str().unwrap().starts_with("set_"))
                .map(|request| (request["method"].clone(), request["params"].clone()))
                .collect::<Vec<_>>();
            assert_eq!(
                writes,
                vec![
                    (json!("set_tar_temp"), json!([26])),
                    (json!("set_power"), json!(["on"])),
                    (json!("set_mode"), json!(["heat"])),
                    (json!("set_tar_temp"), json!([27])),
                    (json!("set_fan_level"), json!(["large_fan"])),
                    (json!("set_ver_swing"), json!(["on"]))
                ]
            );
            let status = control
                .context
                .diagnostics()
                .devices
                .into_iter()
                .find(|status| status.did == did)
                .unwrap();
            assert_eq!(status.selected_control, Some(PathKind::Direct));
        })
    }));
}

#[test]
fn mcn02_rejection_preserves_accepted_steps_and_never_sends_later_steps() {
    use crate::device::CommandError;
    let did = "308540429";
    let (rig, peer) = setup(did);
    block_on(rig.run(|control| {
        Box::pin(async move {
            let ac = function(&control.context.registry, did, "air-conditioner:2");
            until(|| ac.snapshot().air_conditioner().unwrap().power.value == Some(false)).await;
            peer.rejected
                .lock()
                .unwrap()
                .insert("set_mode".into(), -4004);
            peer.hold_read.store(true, Ordering::Relaxed);
            let result = ac
                .begin_air_conditioner(AirConditionerCommand::Combined(AirConditionerTarget {
                    mode: Some(AirConditionerMode::Heat),
                    temperature: Some(27),
                    speed: Some(AirConditionerFanSpeed::High),
                    swing: Some(true),
                }))
                .unwrap()
                .accepted()
                .await;
            assert_eq!(
                result,
                Err(CommandError::Rejected("device code -4004".into()))
            );
            let writes = peer
                .requests
                .lock()
                .unwrap()
                .iter()
                .filter(|r| r["method"].as_str().unwrap().starts_with("set_"))
                .map(|r| r["method"].clone())
                .collect::<Vec<_>>();
            assert_eq!(writes, vec![json!("set_power"), json!("set_mode")]);
            assert_eq!(peer.values.lock().unwrap()[0], json!("on"));
            assert_eq!(
                ac.snapshot().air_conditioner().unwrap().power.value,
                Some(false)
            );
            peer.hold_read.store(false, Ordering::Relaxed);
            until(|| ac.snapshot().air_conditioner().unwrap().power.value == Some(true)).await;
            assert!(control.context.direct_candidate(did).is_ok());
            let before = peer.requests.lock().unwrap().len();
            let result = ac.begin_air_conditioner(AirConditionerCommand::Temperature(15));
            if let Ok(action) = result {
                assert!(action.accepted().await.is_err());
            }
            assert_eq!(peer.requests.lock().unwrap().len(), before);
            let status = control
                .context
                .diagnostics()
                .devices
                .into_iter()
                .find(|status| status.did == did)
                .unwrap();
            assert_eq!(
                status.lan.unwrap().protocol,
                super::super::model::LanProtocol::LegacyMcn02
            );
        })
    }));
}

#[test]
fn two_mcn02_devices_share_mapping_and_sent_timeout_is_not_replayed() {
    use crate::device::CommandError;
    let dids = ["308540429", "584519248"];
    let rig = Rig::custom_lights(&[
        (dids[0], "lumi.acpartner.mcn02", instance()),
        (dids[1], "lumi.acpartner.mcn02", instance()),
    ]);
    let scope = rig
        .store
        .devices()
        .load_configuration()
        .unwrap()
        .scope
        .unwrap();
    let token = Token::parse("00112233445566778899aabbccddeeff").unwrap();
    let first = LegacyDevice::new(token.clone(), dids[0].parse().unwrap());
    let second = LegacyDevice::new(token.clone(), dids[1].parse().unwrap());
    for (did, address, endpoint) in [
        (dids[0], "127.0.0.2", first.address),
        (dids[1], "127.0.0.3", second.address),
    ] {
        let address = address.parse().unwrap();
        rig.store
            .lan()
            .save(&LanRecord {
                scope: scope.clone(),
                did: did.into(),
                model: "lumi.acpartner.mcn02".into(),
                address,
                token: token.clone(),
            })
            .unwrap();
        rig.source
            .context
            .lan_test_endpoints
            .borrow_mut()
            .insert(address, endpoint);
    }
    rig.source
        .context
        .lan_timeout
        .set(Duration::from_millis(100));
    rig.clock.automatic.store(true, Ordering::Relaxed);
    rig.cloud
        .lock()
        .unwrap()
        .online
        .extend(dids.iter().map(|did| (*did).to_owned()));
    block_on(rig.run(|control| {
        Box::pin(async move {
            until(|| {
                dids.iter().all(|did| {
                    control.context.direct_candidate(did).is_ok()
                        && control.context.cloud_candidate(did, true).is_ok()
                        && function(&control.context.registry, did, "air-conditioner:2")
                            .snapshot()
                            .air_conditioner()
                            .unwrap()
                            .power
                            .value
                            == Some(false)
                })
            })
            .await;
            control.clock.automatic.store(false, Ordering::Relaxed);
            first.drop_write.store(true, Ordering::Relaxed);
            let ac1 = function(&control.context.registry, dids[0], "air-conditioner:2");
            let ac2 = function(&control.context.registry, dids[1], "air-conditioner:2");
            assert_eq!(
                ac1.begin_air_conditioner(AirConditionerCommand::Power(true))
                    .unwrap()
                    .accepted()
                    .await,
                Err(CommandError::Timeout)
            );
            assert_eq!(
                first
                    .requests
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|r| r["method"] == "set_power")
                    .count(),
                1
            );
            assert!(control.cloud.lock().unwrap().numeric_writes.is_empty());
            let status = control
                .context
                .diagnostics()
                .devices
                .into_iter()
                .find(|device| device.did == dids[0])
                .unwrap();
            assert!(
                status.commands[0]
                    .last
                    .as_ref()
                    .unwrap()
                    .result_text()
                    .contains("uncertain")
            );
            assert!(control.context.direct_candidate(dids[0]).is_err());
            assert!(control.context.direct_candidate(dids[1]).is_ok());
            ac2.begin_air_conditioner(AirConditionerCommand::Speed(AirConditionerFanSpeed::Auto))
                .unwrap()
                .accepted()
                .await
                .unwrap();
            let writes = second
                .requests
                .lock()
                .unwrap()
                .iter()
                .filter(|r| r["method"].as_str().unwrap().starts_with("set_"))
                .map(|r| (r["method"].clone(), r["params"].clone()))
                .collect::<Vec<_>>();
            assert_eq!(
                writes,
                vec![
                    (json!("set_power"), json!(["on"])),
                    (json!("set_fan_level"), json!(["auto_fan"]))
                ]
            );
            first.drop_write.store(false, Ordering::Relaxed);
            first.values.lock().unwrap()[0] = json!("on");
            control.clock.advance(Duration::from_secs(1));
            until(|| control.context.direct_candidate(dids[0]).is_ok()).await;
            control.clock.automatic.store(true, Ordering::Relaxed);
            until(|| ac1.snapshot().air_conditioner().unwrap().power.value == Some(true)).await;
            assert_eq!(
                first
                    .requests
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|r| r["method"] == "set_power")
                    .count(),
                1
            );
            assert!(control.cloud.lock().unwrap().numeric_writes.is_empty());
        })
    }));
}

#[test]
fn mcn02_saved_scope_rebuilds_offline_with_the_same_identity_and_controls() {
    let did = "584519248";
    use crate::device::FunctionKey;
    let sensor: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/miaomiaoce-t6.json"
    ))
    .unwrap();
    let mut rig = Rig::custom_lights(&[
        (did, "lumi.acpartner.mcn02", instance()),
        ("sensor", "miaomiaoce.sensor_ht.t6", sensor),
    ]);
    let token = Token::parse("00112233445566778899aabbccddeeff").unwrap();
    let peer = LegacyDevice::new(token.clone(), did.parse().unwrap());
    let mut before = rig.store.devices().load_configuration().unwrap();
    before.temperature_bindings.insert(
        FunctionKey::new(DeviceKey::new("xiaomi", did), "air-conditioner:2"),
        FunctionKey::new(DeviceKey::new("xiaomi", "sensor"), "temperature:3:1001"),
    );
    rig.store
        .devices()
        .commit_configuration(&before, &[])
        .unwrap();
    rig.store
        .lan()
        .save(&LanRecord {
            scope: before.scope.clone().unwrap(),
            did: did.into(),
            model: "lumi.acpartner.mcn02".into(),
            address: "127.0.0.1".parse().unwrap(),
            token,
        })
        .unwrap();
    rig.clock.automatic.store(true, Ordering::Relaxed);
    let mut tokens = rig.store.xiaomi().load().unwrap().unwrap().tokens;
    tokens.refresh_at = 1;
    tokens.expires_at = 1;
    rig.store.xiaomi().update_tokens(&tokens).unwrap();
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.token_status = 503;
        cloud.home_status = 503;
        cloud.cert_status = 503;
    }
    rig.clock.advance(Duration::from_secs(7200));
    let (restored, ready) = XiaomiAdapter::build(
        rig.store.clone(),
        rig.source.cloud.clone(),
        rig.discovery.clone(),
        rig.clock.clone(),
    )
    .unwrap();
    restored.context.lan_port.set(peer.address.port());
    rig.source = restored;
    rig.ready = ready;
    block_on(rig.run(|control| {
        Box::pin(async move {
            let ac = function(&control.context.registry, did, "air-conditioner:2");
            until(|| {
                let state = ac.snapshot().air_conditioner().unwrap();
                state.power.value == Some(false)
                    && state.mode.value == Some(AirConditionerMode::Cool)
                    && state.temperature.value == Some(24)
            })
            .await;
            assert!(!matches!(
                ac.snapshot().ambient_temperature(),
                Some(crate::device::AmbientTemperature::Unbound) | None
            ));
            ac.begin_air_conditioner(AirConditionerCommand::Swing(true))
                .unwrap()
                .accepted()
                .await
                .unwrap();
            assert!(
                peer.requests
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|r| r["method"] == "set_ver_swing" && r["params"] == json!(["on"]))
            );
            assert_eq!(
                control.store.devices().load_configuration().unwrap(),
                before
            );
        })
    }));
}

#[test]
fn invalid_legacy_feedback_isolated_per_property_and_newer_notification_wins() {
    use crate::{device::AirConditionerValue, xiaomi::capability::PropertyBinding};
    let did = "308540429";
    let (rig, peer) = setup(did);
    block_on(rig.run(|control| {
        Box::pin(async move {
            let ac = function(&control.context.registry, did, "air-conditioner:2");
            until(|| ac.snapshot().air_conditioner().unwrap().temperature.value == Some(24)).await;
            let work = control.context.live.borrow().devices[did]
                .admitted
                .clone()
                .unwrap();
            let mode = PropertyBinding { siid: 2, piid: 2 };
            let power = PropertyBinding { siid: 2, piid: 1 };
            let temp = PropertyBinding { siid: 2, piid: 3 };
            until(|| !work.refresh.borrow().contains("air-conditioner:2", temp)).await;
            peer.hold_read.store(true, Ordering::Relaxed);
            {
                let mut values = peer.values.lock().unwrap();
                values[0] = json!("on");
                values[1] = json!("unsupported");
                values[2] = json!(27);
            }
            let count = peer
                .requests
                .lock()
                .unwrap()
                .iter()
                .filter(|r| r["method"] == "get_prop")
                .count();
            for binding in [power, mode, temp] {
                work.refresh_property("air-conditioner:2", binding);
            }
            until(|| {
                peer.requests
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|r| r["method"] == "get_prop")
                    .count()
                    > count
            })
            .await;
            let parsed = work
                .native
                .device()
                .functions()
                .iter()
                .find(|f| f.id() == "air-conditioner:2")
                .unwrap();
            work.observe_air_conditioner(
                parsed,
                power,
                AirConditionerValue::Power(false),
                Observation {
                    source: PathKind::Cloud,
                    source_time: Some(123),
                    received_at: control.clock.unix_now(),
                },
                Origin::Notification,
            );
            peer.hold_read.store(false, Ordering::Relaxed);
            until(|| {
                ac.snapshot().air_conditioner().unwrap().temperature.value == Some(27)
                    && work
                        .refresh
                        .borrow()
                        .property_problem("air-conditioner:2", mode)
                        .is_some()
            })
            .await;
            let state = ac.snapshot().air_conditioner().unwrap();
            assert_eq!(state.power.value, Some(false));
            assert_eq!(state.mode.value, Some(AirConditionerMode::Cool));
            assert_eq!(
                work.property_source("air-conditioner:2", power),
                Some(PathKind::Cloud)
            );
            assert!(control.context.direct_candidate(did).is_ok());
            control.clock.automatic.store(false, Ordering::Relaxed);
            peer.values.lock().unwrap()[1] = json!("heat");
            control.clock.advance(Duration::from_secs(4));
            until(|| {
                ac.snapshot().air_conditioner().unwrap().mode.value
                    == Some(AirConditionerMode::Heat)
                    && work
                        .refresh
                        .borrow()
                        .property_problem("air-conditioner:2", mode)
                        .is_none()
            })
            .await;
            assert_eq!(
                peer.requests
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|r| r["method"] == "get_prop")
                    .count(),
                count + 2
            );
        })
    }));
}

#[test]
fn mcn02_hub_and_cloud_continue_using_standard_miot_properties() {
    use crate::xiaomi::capability::PropertyBinding;
    let did = "308540429";
    let (rig, direct) = setup(did);
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.online.insert(did.into());
        for (siid, piid, value) in [
            (2, 1, json!(false)),
            (2, 2, json!(1)),
            (2, 3, json!(24.0)),
            (3, 1, json!(0)),
            (3, 2, json!(false)),
        ] {
            cloud.raw_values.insert((did.into(), siid, piid), value);
        }
    }
    block_on(rig.run(|control| {
        Box::pin(async move {
            until(|| {
                control.context.direct_candidate(did).is_ok()
                    && control.context.cloud_candidate(did, true).is_ok()
            })
            .await;
            control.announce();
            let hub = Rc::new(Peer::default());
            for (siid, piid, value) in [
                (2, 1, json!(false)),
                (2, 2, json!(1)),
                (2, 3, json!(24.0)),
                (3, 1, json!(0)),
                (3, 2, json!(false)),
            ] {
                hub.value(did, PropertyBinding { siid, piid }, value);
            }
            let c = &control;
            control
                .peer(hub, |hub| {
                    Box::pin(async move {
                        until(|| {
                            matches!(
                                c.context.control_path_for(did, "air-conditioner:2"),
                                Some(super::super::routing::ControlPath::Local(_))
                            )
                        })
                        .await;
                        let ac = function(&c.context.registry, did, "air-conditioner:2");
                        until(|| {
                            ac.snapshot().air_conditioner().unwrap().temperature.value == Some(24)
                        })
                        .await;
                        ac.begin_air_conditioner(AirConditionerCommand::Temperature(26))
                            .unwrap()
                            .accepted()
                            .await
                            .unwrap();
                        let hub_writes = hub
                            .state
                            .borrow()
                            .seen
                            .iter()
                            .filter(|(op, _)| op == "rpcReq")
                            .map(|(_, request)| request["rpc"]["params"][0].clone())
                            .collect::<Vec<_>>();
                        assert_eq!(hub_writes.len(), 1);
                        assert_eq!(
                            (
                                hub_writes[0]["siid"].clone(),
                                hub_writes[0]["piid"].clone(),
                                hub_writes[0]["value"].clone()
                            ),
                            (json!(2), json!(3), json!(26.0))
                        );
                        assert!(
                            !direct
                                .requests
                                .lock()
                                .unwrap()
                                .iter()
                                .any(|r| r["method"].as_str().unwrap().starts_with("set_"))
                        );
                        c.context.stop_lan(did);
                        c.context.priority.set(PathPriority::CloudFirst);
                        assert!(matches!(
                            c.context.control_path_for(did, "air-conditioner:2"),
                            Some(super::super::routing::ControlPath::Cloud(_))
                        ));
                        ac.begin_air_conditioner(AirConditionerCommand::Temperature(27))
                            .unwrap()
                            .accepted()
                            .await
                            .unwrap();
                        let cloud_writes = &c.cloud.lock().unwrap().numeric_writes;
                        assert_eq!(cloud_writes.len(), 1);
                        assert_eq!(
                            (
                                cloud_writes[0].0[0]["siid"].clone(),
                                cloud_writes[0].0[0]["piid"].clone(),
                                cloud_writes[0].0[0]["value"].clone()
                            ),
                            (json!(2), json!(3), json!(27.0))
                        );
                        assert!(
                            !direct
                                .requests
                                .lock()
                                .unwrap()
                                .iter()
                                .any(|r| r["method"].as_str().unwrap().starts_with("set_"))
                        );
                    })
                })
                .await;
        })
    }));
}

#[test]
fn invalid_legacy_write_is_rejected_before_any_udp_send() {
    use crate::xiaomi::{capability::PropertyBinding, lan::Client};
    let token = Token::parse("00112233445566778899aabbccddeeff").unwrap();
    let peer = LegacyDevice::new(token.clone(), 308540429);
    let client = Client::new(308540429, peer.address, token);
    let mapping = super::super::legacy_miio::mapping("lumi.acpartner.mcn02").unwrap();
    let error = block_on(super::super::legacy_miio::set_property(
        mapping,
        &client,
        &[(PropertyBinding { siid: 2, piid: 3 }, json!(24.5))],
        || true,
        || panic!("must not send"),
    ))
    .unwrap_err();
    assert!(matches!(
        error,
        super::super::legacy_miio::WriteError::Mapping
    ));
    assert!(peer.requests.lock().unwrap().is_empty());
}

#[test]
fn malformed_legacy_ack_is_an_uncertain_sent_response() {
    use crate::xiaomi::{
        capability::PropertyBinding,
        lan::{Client, Failure, SendStage},
    };
    let token = Token::parse("00112233445566778899aabbccddeeff").unwrap();
    let peer = LegacyDevice::new(token.clone(), 308540429);
    *peer.ack.lock().unwrap() = json!(["unexpected"]);
    let client = Client::new(308540429, peer.address, token);
    let mapping = super::super::legacy_miio::mapping("lumi.acpartner.mcn02").unwrap();
    let sent = std::cell::Cell::new(false);
    let error = block_on(super::super::legacy_miio::set_property(
        mapping,
        &client,
        &[(PropertyBinding { siid: 2, piid: 1 }, json!(true))],
        || true,
        || sent.set(true),
    ))
    .unwrap_err();
    assert!(sent.get());
    assert!(
        matches!(error, super::super::legacy_miio::WriteError::Request(error) if error.stage == SendStage::Sent && error.failure == Failure::InvalidResponse)
    );
    assert_eq!(
        peer.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r["method"] == "set_power")
            .count(),
        1
    );
}

#[test]
fn mcn02_does_not_become_ready_when_read_only_authentication_is_rejected() {
    let did = "308540429";
    let (rig, peer) = setup(did);
    peer.rejected
        .lock()
        .unwrap()
        .insert("get_prop".into(), -4004);
    block_on(rig.run(|control| {
        Box::pin(async move {
            until(|| {
                control.context.diagnostics().devices.iter().any(|device| {
                    device.did == did
                        && device
                            .lan
                            .as_ref()
                            .is_some_and(|lan| lan.state == LanState::Failed)
                })
            })
            .await;
            assert!(control.context.direct_candidate(did).is_err());
            let requests = peer.requests.lock().unwrap();
            assert_eq!(
                requests
                    .iter()
                    .map(|request| request["method"].clone())
                    .collect::<Vec<_>>(),
                [json!("miIO.info"), json!("get_prop")]
            );
            let status = control
                .context
                .diagnostics()
                .devices
                .into_iter()
                .find(|device| device.did == did)
                .unwrap();
            let lan = status.lan.unwrap();
            assert!(lan.problem.unwrap().contains("-4004"));
            assert_eq!(lan.recovery, RecoveryStatus::None);
        })
    }));
}

#[test]
fn invalid_legacy_ack_is_shown_as_uncertain_by_the_command_diagnostics() {
    use crate::xiaomi::adapter::StepReceipt;
    let did = "308540429";
    let (rig, peer) = setup(did);
    block_on(rig.run(|control| {
        Box::pin(async move {
            let ac = function(&control.context.registry, did, "air-conditioner:2");
            until(|| ac.snapshot().air_conditioner().unwrap().temperature.value == Some(24)).await;
            control.clock.automatic.store(false, Ordering::Relaxed);
            *peer.ack.lock().unwrap() = json!(["invalid"]);
            let result = ac
                .begin_air_conditioner(AirConditionerCommand::Temperature(26))
                .unwrap()
                .accepted()
                .await;
            assert!(result.is_err());
            let status = control.context.diagnostics();
            let device = status
                .devices
                .iter()
                .find(|device| device.did == did)
                .unwrap();
            let last = device.commands[0].last.as_ref().unwrap();
            assert_eq!(last.actual_path, Some(PathKind::Direct));
            assert!(matches!(last.steps[0].receipt, StepReceipt::Uncertain(_)));
            assert!(last.feedback.is_none());
            let auth = crate::terminal::AuthStatus::new(
                control.context.status.borrow().authentication.clone(),
                "/bin/migate".into(),
                "/data".into(),
            );
            auth.update_adapter(status);
            let terminal =
                crate::terminal::bridge::Context::committed(control.publication.clone(), &auth);
            let text = terminal
                .handle_line(&format!("status {did}"), 0)
                .await
                .unwrap()
                .unwrap();
            assert!(text.contains("result uncertain"), "{text}");
            assert_eq!(
                peer.requests
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|r| r["method"] == "set_tar_temp")
                    .count(),
                1
            );
        })
    }));
}

#[test]
fn mcn02_partial_acceptance_reads_once_after_all_sent_steps() {
    use crate::xiaomi::adapter::{CommandFeedback, StepReceipt};
    let did = "308540429";
    let (rig, peer) = setup(did);
    block_on(rig.run(|control| {
        Box::pin(async move {
            let ac = function(&control.context.registry, did, "air-conditioner:2");
            until(|| ac.snapshot().air_conditioner().unwrap().temperature.value == Some(24)).await;
            let work = control.context.live.borrow().devices[did]
                .admitted
                .clone()
                .unwrap();
            let bindings = work.native.device().functions()[0].observation_bindings();
            until(|| {
                bindings.iter().all(|binding| {
                    !work
                        .refresh
                        .borrow()
                        .contains("air-conditioner:2", *binding)
                })
            })
            .await;
            let before = peer
                .requests
                .lock()
                .unwrap()
                .iter()
                .filter(|r| r["method"] == "get_prop")
                .count();
            peer.hold_read.store(true, Ordering::Relaxed);
            peer.rejected
                .lock()
                .unwrap()
                .insert("set_mode".into(), -4004);
            let result = ac
                .begin_air_conditioner(AirConditionerCommand::Combined(AirConditionerTarget {
                    mode: Some(AirConditionerMode::Heat),
                    temperature: Some(27),
                    speed: Some(AirConditionerFanSpeed::High),
                    swing: Some(true),
                }))
                .unwrap()
                .accepted()
                .await;
            assert!(result.is_err());
            until(|| {
                peer.requests
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|r| r["method"] == "get_prop")
                    .count()
                    == before + 1
            })
            .await;
            let requests = peer.requests.lock().unwrap().clone();
            let writes = requests
                .iter()
                .filter_map(|r| r["method"].as_str())
                .filter(|method| method.starts_with("set_"))
                .collect::<Vec<_>>();
            assert_eq!(writes, ["set_power", "set_mode"]);
            let last_write = requests
                .iter()
                .rposition(|r| r["method"] == "set_mode")
                .unwrap();
            assert!(
                requests[last_write + 1..]
                    .iter()
                    .any(|r| r["method"] == "get_prop")
            );
            assert_eq!(
                ac.snapshot().air_conditioner().unwrap().power.value,
                Some(false)
            );
            peer.hold_read.store(false, Ordering::Relaxed);
            until(|| {
                control
                    .context
                    .diagnostics()
                    .devices
                    .iter()
                    .find(|device| device.did == did)
                    .and_then(|device| device.commands[0].last.as_ref())
                    .is_some_and(|last| last.feedback == Some(CommandFeedback::Complete))
            })
            .await;
            assert_eq!(
                ac.snapshot().air_conditioner().unwrap().power.value,
                Some(true)
            );
            let status = control.context.diagnostics();
            let last = status
                .devices
                .iter()
                .find(|device| device.did == did)
                .unwrap()
                .commands[0]
                .last
                .as_ref()
                .unwrap();
            assert_eq!(last.steps[0].receipt, StepReceipt::Accepted);
            assert!(matches!(last.steps[1].receipt, StepReceipt::Rejected(_)));
            assert_eq!(
                peer.requests
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|r| r["method"] == "get_prop")
                    .count(),
                before + 1
            );
        })
    }));
}

#[test]
fn cloud_only_function_retries_at_deadline_while_air_conditioner_uses_direct() {
    use super::super::routing::ControlPath;
    use crate::xiaomi::{
        capability::PropertyBinding,
        cloud::{CloudDeviceInfo, LanCredentials},
    };

    let did = "308540429";
    let mut description = instance();
    description["services"].as_array_mut().unwrap().push(json!({
        "iid": 4, "type": "urn:miot-spec-v2:service:light:00007802:example:1",
        "properties": [{"iid": 1, "type": "urn:miot-spec-v2:property:on:00000006:example:1", "format": "bool", "access": ["read", "write", "notify"]}]
    }));
    let (rig, _peer) = setup_with_description(did, description);
    let context = rig.source.context.clone();
    let clock = rig.clock.clone();
    clock.automatic.store(false, Ordering::Relaxed);
    context.reconcile_lan(did).unwrap();
    block_on(future::or(
        async {
            context.tasks.run().await.unwrap();
            panic!("task runner stopped")
        },
        async {
            future::or(until(|| context.direct_candidate(did).is_ok()), async {
                async_io::Timer::after(Duration::from_secs(1)).await;
                panic!("direct probe stalled")
            })
            .await;
        },
    ));
    context
        .live
        .borrow_mut()
        .devices
        .get_mut(did)
        .unwrap()
        .cloud_access = Some(CloudDeviceInfo {
        did: did.into(),
        model: "lumi.acpartner.mcn02".into(),
        instance_type: "test".into(),
        online: true,
        lan: LanCredentials::Unknown,
    });
    context.adopt_credentials(rig.store.xiaomi().load().unwrap().unwrap());
    context.cloud_scope_valid.set(true);
    let binding = PropertyBinding { siid: 4, piid: 1 };
    let key = (did.to_owned(), binding.siid, binding.piid);
    rig.cloud
        .lock()
        .unwrap()
        .raw_values
        .insert(key.clone(), json!("invalid"));
    let work = context.live.borrow().devices[did].admitted.clone().unwrap();
    let read_count = || {
        rig.cloud
            .lock()
            .unwrap()
            .property_reads
            .iter()
            .filter(|read| **read == key)
            .count()
    };
    assert!(matches!(
        context.read_path(did),
        Some(ControlPath::Direct(_))
    ));
    assert!(matches!(
        context.read_path_for(did, "air-conditioner:2"),
        Some(ControlPath::Direct(_))
    ));
    assert!(matches!(
        context.read_path_for(did, "light:4"),
        Some(ControlPath::Cloud(_))
    ));
    work.refresh_property("light:4", binding);
    block_on(future::or(
        async {
            future::or(
                super::super::refresh::cloud(context.clone()),
                context.tasks.run(),
            )
            .await
            .unwrap();
            panic!("refresh runner stopped");
        },
        async {
            future::or(
                until(|| {
                    let state = work.refresh.borrow().snapshot();
                    state.inflight.is_empty()
                        && state
                            .pending
                            .iter()
                            .any(|read| read.id == "light:4" && read.attempts == 1)
                }),
                async {
                    async_io::Timer::after(Duration::from_secs(2)).await;
                    panic!("initial cloud read did not fail");
                },
            )
            .await;
            let pending = work
                .refresh
                .borrow()
                .snapshot()
                .pending
                .into_iter()
                .find(|read| read.id == "light:4")
                .unwrap();
            assert_eq!(pending.ready_at, Duration::from_secs(3));
            assert_eq!(read_count(), 1);
            // Let the scheduler consume the failed read's completion before advancing time.
            async_io::Timer::after(Duration::from_millis(40)).await;
            clock.advance(Duration::from_millis(2999));
            async_io::Timer::after(Duration::from_millis(20)).await;
            assert_eq!(read_count(), 1);
            rig.cloud
                .lock()
                .unwrap()
                .raw_values
                .insert(key.clone(), json!(true));
            clock.advance(Duration::from_millis(1));
            future::or(until(|| read_count() == 2), async {
                async_io::Timer::after(Duration::from_secs(1)).await;
                panic!("cloud retry was not woken at its deadline")
            })
            .await;
            future::or(
                until(|| {
                    work.physical()
                        .function("light:4")
                        .unwrap()
                        .snapshot()
                        .power()
                        == Some(Power::On)
                }),
                async {
                    async_io::Timer::after(Duration::from_secs(2)).await;
                    panic!("retried cloud reading did not settle");
                },
            )
            .await;
            assert!(
                work.refresh
                    .borrow()
                    .property_problem("light:4", binding)
                    .is_none()
            );
            assert!(matches!(
                context.read_path_for(did, "air-conditioner:2"),
                Some(ControlPath::Direct(_))
            ));
        },
    ));
}
