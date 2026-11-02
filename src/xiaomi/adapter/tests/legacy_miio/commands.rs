use super::*;

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
                super::super::super::model::LanProtocol::LegacyMcn02
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
fn invalid_legacy_write_is_rejected_before_any_udp_send() {
    use crate::xiaomi::{capability::PropertyBinding, lan::Client};
    let token = Token::parse("00112233445566778899aabbccddeeff").unwrap();
    let peer = LegacyDevice::new(token.clone(), 308540429);
    let client = Client::new(308540429, peer.address, token);
    let mapping = super::super::super::legacy_miio::mapping("lumi.acpartner.mcn02").unwrap();
    let error = block_on(super::super::super::legacy_miio::set_property(
        mapping,
        &client,
        &[(PropertyBinding { siid: 2, piid: 3 }, json!(24.5))],
        || true,
        || panic!("must not send"),
    ))
    .unwrap_err();
    assert!(matches!(
        error,
        super::super::super::legacy_miio::WriteError::Mapping
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
    let mapping = super::super::super::legacy_miio::mapping("lumi.acpartner.mcn02").unwrap();
    let sent = std::cell::Cell::new(false);
    let error = block_on(super::super::super::legacy_miio::set_property(
        mapping,
        &client,
        &[(PropertyBinding { siid: 2, piid: 1 }, json!(true))],
        || true,
        || sent.set(true),
    ))
    .unwrap_err();
    assert!(sent.get());
    assert!(
        matches!(error, super::super::super::legacy_miio::WriteError::Request(error) if error.stage == SendStage::Sent && error.failure == Failure::InvalidResponse)
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
