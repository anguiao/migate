use super::*;
use crate::{
    storage::LanRecord,
    xiaomi::{
        cloud::{CloudDeviceInfo, LanCandidate, LanCredentials},
        lan::{Token, test_support::FakeDevice},
    },
};

fn retired_queued_lan_task_does_not_probe(recovery: bool) {
    use crate::xiaomi::lan::{Client, Failure, RequestError, SendStage};
    use std::sync::atomic::Ordering;

    let did = "656878122";
    let instance = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/dmaker-p5c.json"
    ))
    .unwrap();
    let rig = Rig::custom_lights(&[(did, "dmaker.fan.p5c", instance)]);
    let token = Token::parse("00112233445566778899aabbccddeeff").unwrap();
    let peer = FakeDevice::new(token.clone(), token.clone());
    let record = LanRecord {
        scope: rig
            .store
            .devices()
            .load_configuration()
            .unwrap()
            .scope
            .unwrap(),
        did: did.into(),
        model: "dmaker.fan.p5c".into(),
        address: "127.0.0.1".parse().unwrap(),
        token: token.clone(),
    };
    rig.store.lan().save(&record).unwrap();
    let context = rig.source.context.clone();
    context.lan_port.set(peer.address.port());
    if recovery {
        let client = Client::new(did.parse().unwrap(), peer.address, token);
        {
            let mut live = context.live.borrow_mut();
            let lan = &mut live.devices.get_mut(did).unwrap().lan;
            lan.active = Some((record, client.clone()));
            lan.status = Some(LanStatus {
                state: LanState::Ready,
                address: Some("127.0.0.1".parse().unwrap()),
                problem: None,
                recovery: RecoveryStatus::None,
            });
        }
        context
            .lan_business_failure(
                did,
                &client,
                &RequestError {
                    stage: SendStage::Sent,
                    failure: Failure::Timeout,
                },
            )
            .unwrap();
    } else {
        context.reconcile_lan(did).unwrap();
    }
    assert!(
        context.live.borrow().devices[did]
            .lan
            .status
            .as_ref()
            .is_some_and(|status| status.state == LanState::Authenticating)
    );
    context.remove_source_slot(did);
    assert!(!context.live.borrow().devices.contains_key(did));
    block_on(future::or(
        async {
            context.tasks.run().await.unwrap();
            panic!("task runner stopped");
        },
        async {
            async_io::Timer::after(Duration::from_millis(100)).await;
        },
    ));
    assert_eq!(peer.packets.load(Ordering::Relaxed), 0);
}

#[test]
fn removed_device_cancels_queued_recovery_before_first_poll() {
    retired_queued_lan_task_does_not_probe(true);
}

#[test]
fn removed_device_cancels_queued_authentication_before_first_poll() {
    retired_queued_lan_task_does_not_probe(false);
}

#[test]
fn direct_property_batches_correlate_results_and_preserve_each_rejection() {
    use crate::xiaomi::{
        capability::PropertyBinding,
        lan::{Client, Failure},
    };
    use std::sync::atomic::Ordering;

    let token = Token::parse("00112233445566778899aabbccddeeff").unwrap();
    let peer = FakeDevice::new(token.clone(), token.clone());
    peer.reverse_write_results.store(true, Ordering::Relaxed);
    peer.write_codes.lock().unwrap().insert((2, 4), -4004);
    let client = Client::new(656878122, peer.address, token);
    let properties = [
        (PropertyBinding { siid: 2, piid: 1 }, json!(true)),
        (PropertyBinding { siid: 2, piid: 4 }, json!(true)),
    ];
    let result = block_on(super::super::lan::set_properties(
        &client,
        "656878122",
        &properties,
        || true,
        || {},
    ))
    .unwrap();
    assert!(!result.protocol_error);
    assert_eq!(result.items[0], Ok(()));
    assert!(matches!(result.items[1], Err(ref error) if error.failure == Failure::Rejected(-4004)));
    peer.omit_write_result.store(true, Ordering::Relaxed);
    let incomplete = block_on(super::super::lan::set_properties(
        &client,
        "656878122",
        &properties,
        || true,
        || {},
    ))
    .unwrap();
    assert!(incomplete.protocol_error);
}

#[test]
fn offline_p5c_reads_and_controls_existing_fan_bindings_and_preserves_rejection() {
    use crate::device::{CommandError, FanCommand};
    use std::sync::atomic::Ordering;

    let instance = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/dmaker-p5c.json"
    ))
    .unwrap();
    let rig = Rig::custom_lights(&[("656878122", "dmaker.fan.p5c", instance)]);
    let scope = rig
        .store
        .devices()
        .load_configuration()
        .unwrap()
        .scope
        .unwrap();
    let token = Token::parse("00112233445566778899aabbccddeeff").unwrap();
    let peer = FakeDevice::new(token.clone(), token.clone());
    peer.values.lock().unwrap().insert((2, 1), json!(false));
    rig.store
        .lan()
        .save(&LanRecord {
            scope,
            did: "656878122".into(),
            model: "dmaker.fan.p5c".into(),
            address: "127.0.0.1".parse().unwrap(),
            token,
        })
        .unwrap();
    rig.source.context.lan_port.set(peer.address.port());
    rig.clock.automatic.store(true, Ordering::Relaxed);
    block_on(rig.run(|control| {
        Box::pin(async move {
            let did = "656878122";
            until(|| {
                control.context.diagnostics().devices.iter().any(|device| {
                    device.did == did
                        && device.selected_control == Some(PathKind::Direct)
                        && device.selected_read == Some(PathKind::Direct)
                })
            })
            .await;
            until(|| {
                let snapshot = function(&control.context.registry, did, "fan:2")
                    .snapshot()
                    .fan()
                    .unwrap();
                snapshot.power.value == Some(false)
                    && snapshot.speed.value == Some(40)
                    && snapshot.swing.value == Some(false)
            })
            .await;
            let fan = function(&control.context.registry, did, "fan:2");
            for command in [
                FanCommand::Power(true),
                FanCommand::Power(false),
                FanCommand::Speed(55),
                FanCommand::Swing(true),
            ] {
                fan.begin_fan(command).unwrap().accepted().await.unwrap();
            }
            let writes = peer
                .writes
                .lock()
                .unwrap()
                .iter()
                .map(|item| {
                    (
                        item["siid"].clone(),
                        item["piid"].clone(),
                        item["value"].clone(),
                    )
                })
                .collect::<Vec<_>>();
            assert_eq!(
                writes,
                vec![
                    (json!(2), json!(1), json!(true)),
                    (json!(2), json!(1), json!(false)),
                    (json!(2), json!(1), json!(true)),
                    (json!(2), json!(3), json!(0)),
                    (json!(8), json!(1), json!(55)),
                    (json!(2), json!(4), json!(true)),
                ]
            );
            peer.reject_write.store(true, Ordering::Relaxed);
            assert_eq!(
                fan.begin_fan(FanCommand::Swing(false))
                    .unwrap()
                    .accepted()
                    .await,
                Err(CommandError::Rejected("device code -4004".into()))
            );
            let status = control
                .context
                .diagnostics()
                .devices
                .into_iter()
                .find(|device| device.did == did)
                .unwrap();
            assert_eq!(status.selected_control, Some(PathKind::Direct));
            assert_eq!(status.lan.unwrap().state, LanState::Ready);
            assert_eq!(
                status.commands[0].last.as_ref().unwrap().actual_path,
                Some(PathKind::Direct)
            );
        })
    }));
}

#[test]
fn cached_scope_restarts_offline_and_executes_real_reads_and_control() {
    use crate::device::FanCommand;
    use std::sync::atomic::Ordering;

    let did = "656878122";
    let instance = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/dmaker-p5c.json"
    ))
    .unwrap();
    let mut rig = Rig::custom_lights(&[(did, "dmaker.fan.p5c", instance)]);
    let before = rig.store.devices().load_configuration().unwrap();
    let token = Token::parse("00112233445566778899aabbccddeeff").unwrap();
    let peer = FakeDevice::new(token.clone(), token.clone());
    rig.store
        .lan()
        .save(&LanRecord {
            scope: before.scope.clone().unwrap(),
            did: did.into(),
            model: "dmaker.fan.p5c".into(),
            address: "127.0.0.1".parse().unwrap(),
            token,
        })
        .unwrap();
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
    rig.clock.automatic.store(true, Ordering::Relaxed);
    block_on(rig.run(|control| {
        Box::pin(async move {
            let fan = function(&control.context.registry, did, "fan:2");
            until(|| {
                let snapshot = fan.snapshot().fan().unwrap();
                snapshot.power.value == Some(true)
                    && snapshot.speed.value == Some(40)
                    && snapshot.swing.value == Some(false)
            })
            .await;
            fan.begin_fan(FanCommand::Power(false))
                .unwrap()
                .accepted()
                .await
                .unwrap();
            assert!(peer.reads.load(Ordering::Relaxed) >= 4);
            assert_eq!(peer.writes.lock().unwrap()[0]["value"], json!(false));
            let status = control
                .context
                .diagnostics()
                .devices
                .into_iter()
                .find(|status| status.did == did)
                .unwrap();
            assert_eq!(status.selected_control, Some(PathKind::Direct));
            assert_eq!(status.selected_read, Some(PathKind::Direct));
            assert_eq!(
                control.store.devices().load_configuration().unwrap(),
                before
            );
            let work = control.context.live.borrow().devices[did]
                .admitted
                .clone()
                .unwrap();
            until(|| {
                [(2, 1), (2, 3), (2, 4), (8, 1)].iter().all(|(siid, piid)| {
                    !work.refresh.borrow().contains(
                        "fan:2",
                        crate::xiaomi::capability::PropertyBinding {
                            siid: *siid,
                            piid: *piid,
                        },
                    )
                })
            })
            .await;
            control.clock.automatic.store(false, Ordering::Relaxed);
            let packets = peer.packets.load(Ordering::Relaxed);
            control.clock.advance(Duration::from_secs(120));
            async_io::Timer::after(Duration::from_millis(50)).await;
            assert_eq!(peer.packets.load(Ordering::Relaxed), packets);
        })
    }));
}

#[test]
fn sent_timeout_is_not_replayed_and_only_failed_device_recovers_with_new_baseline() {
    use crate::device::{CommandError, FanCommand};
    use std::sync::atomic::Ordering;

    let instance: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/dmaker-p5c.json"
    ))
    .unwrap();
    let dids = ["656878122", "656878123"];
    let rig = Rig::custom_lights(&[
        (dids[0], "dmaker.fan.p5c", instance.clone()),
        (dids[1], "dmaker.fan.p5c", instance),
    ]);
    let scope = rig
        .store
        .devices()
        .load_configuration()
        .unwrap()
        .scope
        .unwrap();
    let token = Token::parse("00112233445566778899aabbccddeeff").unwrap();
    let first = FakeDevice::new(token.clone(), token.clone());
    let second = FakeDevice::with_did(token.clone(), token.clone(), 656878123);
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
                model: "dmaker.fan.p5c".into(),
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
                })
            })
            .await;
            until(|| {
                dids.iter().all(|did| {
                    let state = function(&control.context.registry, did, "fan:2")
                        .snapshot()
                        .fan()
                        .unwrap();
                    state.power.value == Some(true)
                        && state.speed.value == Some(40)
                        && state.swing.value == Some(false)
                })
            })
            .await;
            control.clock.automatic.store(false, Ordering::Relaxed);
            first.drop_write.store(true, Ordering::Relaxed);
            let fan1 = function(&control.context.registry, dids[0], "fan:2");
            let fan2 = function(&control.context.registry, dids[1], "fan:2");
            assert_eq!(
                fan1.begin_fan(FanCommand::Power(false))
                    .unwrap()
                    .accepted()
                    .await,
                Err(CommandError::Timeout)
            );
            assert_eq!(first.writes.lock().unwrap().len(), 1);
            assert!(control.cloud.lock().unwrap().numeric_writes.is_empty());
            let statuses = control.context.diagnostics().devices;
            let failed = statuses
                .iter()
                .find(|device| device.did == dids[0])
                .unwrap();
            let healthy = statuses
                .iter()
                .find(|device| device.did == dids[1])
                .unwrap();
            assert_eq!(failed.selected_control, Some(PathKind::Cloud));
            assert_eq!(healthy.selected_control, Some(PathKind::Direct));
            let last = failed.commands[0].last.as_ref().unwrap();
            assert_eq!(last.actual_path, Some(PathKind::Direct));
            assert!(
                last.result_text().contains("uncertain"),
                "{}",
                last.result_text()
            );
            fan2.begin_fan(FanCommand::Swing(true))
                .unwrap()
                .accepted()
                .await
                .unwrap();
            assert_eq!(second.writes.lock().unwrap().len(), 1);
            first.drop_write.store(false, Ordering::Relaxed);
            first.values.lock().unwrap().insert((2, 1), json!(false));
            until(|| {
                control.context.live.borrow().devices[dids[0]]
                    .lan
                    .status
                    .as_ref()
                    .is_some_and(|status| matches!(status.recovery, RecoveryStatus::Scheduled(_)))
            })
            .await;
            control.clock.advance(Duration::from_secs(1));
            until(|| control.context.direct_candidate(dids[0]).is_ok()).await;
            control.clock.automatic.store(true, Ordering::Relaxed);
            until(|| fan1.snapshot().fan().unwrap().power.value == Some(false)).await;
            assert_eq!(first.writes.lock().unwrap().len(), 1);
            assert!(control.cloud.lock().unwrap().numeric_writes.is_empty());
        })
    }));
}

#[test]
fn hub_restoration_keeps_sent_direct_receipt_and_degraded_hub_uses_direct_then_cloud() {
    use crate::device::FanCommand;
    use std::sync::atomic::Ordering;

    let did = "656878122";
    let instance = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/dmaker-p5c.json"
    ))
    .unwrap();
    let rig = Rig::custom_lights(&[(did, "dmaker.fan.p5c", instance)]);
    let scope = rig
        .store
        .devices()
        .load_configuration()
        .unwrap()
        .scope
        .unwrap();
    let token = Token::parse("00112233445566778899aabbccddeeff").unwrap();
    let direct = FakeDevice::new(token.clone(), token.clone());
    rig.store
        .lan()
        .save(&LanRecord {
            scope,
            did: did.into(),
            model: "dmaker.fan.p5c".into(),
            address: "127.0.0.1".parse().unwrap(),
            token,
        })
        .unwrap();
    rig.source.context.lan_port.set(direct.address.port());
    rig.clock.automatic.store(true, Ordering::Relaxed);
    rig.cloud.lock().unwrap().online.insert(did.into());
    block_on(rig.run(|control| {
        Box::pin(async move {
            until(|| {
                matches!(
                    control.context.control_path(did),
                    Some(super::super::routing::ControlPath::Direct(_))
                )
            })
            .await;
            until(|| {
                function(&control.context.registry, did, "fan:2")
                    .snapshot()
                    .fan()
                    .unwrap()
                    .power
                    .value
                    == Some(true)
            })
            .await;
            direct.hold_write.store(true, Ordering::Relaxed);
            let action = function(&control.context.registry, did, "fan:2")
                .begin_fan(FanCommand::Power(false))
                .unwrap();
            until(|| direct.writes.lock().unwrap().len() == 1).await;
            control.announce();
            let gateway = Rc::new(Peer::default());
            let context = control.context.clone();
            control
                .peer(gateway, |gateway| {
                    Box::pin(async move {
                        until(|| {
                            matches!(
                                context.control_path(did),
                                Some(super::super::routing::ControlPath::Local(_))
                            )
                        })
                        .await;
                        direct.hold_write.store(false, Ordering::Relaxed);
                        action.accepted().await.unwrap();
                        assert_eq!(direct.writes.lock().unwrap().len(), 1);
                        assert_eq!(gateway.count("rpcReq", did, 0), 0);
                        context.priority.set(PathPriority::CloudFirst);
                        assert!(matches!(
                            context.control_path(did),
                            Some(super::super::routing::ControlPath::Direct(_))
                        ));
                        assert!(matches!(
                            context.read_path(did),
                            Some(super::super::routing::ControlPath::Direct(_))
                        ));
                        context.stop_lan(did);
                        assert!(matches!(
                            context.control_path(did),
                            Some(super::super::routing::ControlPath::Cloud(_))
                        ));
                        context
                            .live
                            .borrow_mut()
                            .devices
                            .get_mut(did)
                            .unwrap()
                            .cloud_access
                            .as_mut()
                            .unwrap()
                            .online = false;
                        assert!(matches!(
                            context.control_path(did),
                            Some(super::super::routing::ControlPath::Local(_))
                        ));
                        assert!(context.read_path(did).is_none());
                    })
                })
                .await;
        })
    }));
}

#[test]
fn direct_withdrawn_during_hello_reselects_cloud_without_a_direct_write() {
    use crate::{device::FanCommand, xiaomi::lan::Client};
    use std::sync::atomic::Ordering;

    let did = "656878122";
    let instance = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/dmaker-p5c.json"
    ))
    .unwrap();
    let rig = Rig::custom_lights(&[(did, "dmaker.fan.p5c", instance)]);
    let scope = rig
        .store
        .devices()
        .load_configuration()
        .unwrap()
        .scope
        .unwrap();
    let token = Token::parse("00112233445566778899aabbccddeeff").unwrap();
    let peer = FakeDevice::new(token.clone(), token.clone());
    let record = LanRecord {
        scope,
        did: did.into(),
        model: "dmaker.fan.p5c".into(),
        address: "127.0.0.1".parse().unwrap(),
        token: token.clone(),
    };
    rig.store.lan().save(&record).unwrap();
    rig.source.context.lan_port.set(peer.address.port());
    rig.clock.automatic.store(true, Ordering::Relaxed);
    rig.cloud.lock().unwrap().online.insert(did.into());
    block_on(rig.run(|control| {
        Box::pin(async move {
            until(|| {
                control.context.direct_candidate(did).is_ok()
                    && control.context.cloud_candidate(did, true).is_ok()
            })
            .await;
            until(|| {
                let work = control.context.live.borrow().devices[did]
                    .admitted
                    .clone()
                    .unwrap();
                let fan = function(&control.context.registry, did, "fan:2");
                fan.snapshot().fan().unwrap().power.value == Some(true)
                    && [(2, 1), (2, 3), (2, 4), (8, 1)].iter().all(|(siid, piid)| {
                        !work.refresh.borrow().contains(
                            "fan:2",
                            crate::xiaomi::capability::PropertyBinding {
                                siid: *siid,
                                piid: *piid,
                            },
                        )
                    })
            })
            .await;
            control.clock.automatic.store(false, Ordering::Relaxed);
            let mut fresh = Client::new(656878122, peer.address, token);
            fresh.set_timeout(Duration::from_millis(500));
            control
                .context
                .live
                .borrow_mut()
                .devices
                .get_mut(did)
                .unwrap()
                .lan
                .active = Some((record, fresh));
            peer.hold_hello.store(true, Ordering::Relaxed);
            let packets = peer.packets.load(Ordering::Relaxed);
            let action = function(&control.context.registry, did, "fan:2")
                .begin_fan(FanCommand::Power(false))
                .unwrap();
            until(|| peer.packets.load(Ordering::Relaxed) > packets).await;
            control
                .context
                .live
                .borrow_mut()
                .devices
                .get_mut(did)
                .unwrap()
                .lan
                .active = None;
            peer.hold_hello.store(false, Ordering::Relaxed);
            action.accepted().await.unwrap();
            assert!(peer.writes.lock().unwrap().is_empty());
            assert_eq!(control.cloud.lock().unwrap().numeric_writes.len(), 1);
            let status = control
                .context
                .diagnostics()
                .devices
                .into_iter()
                .find(|status| status.did == did)
                .unwrap();
            assert_eq!(
                status.commands[0].last.as_ref().unwrap().actual_path,
                Some(PathKind::Cloud)
            );
        })
    }));
}

#[test]
fn credential_replacement_does_not_discard_an_already_sent_receipt() {
    credential_replacement_result(false);
}

#[test]
fn old_client_timeout_does_not_degrade_replacement_client() {
    credential_replacement_result(true);
}

fn credential_replacement_result(drop_reply: bool) {
    use crate::device::{CommandError, FanCommand};
    use std::sync::atomic::Ordering;

    let did = "656878122";
    let instance = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/dmaker-p5c.json"
    ))
    .unwrap();
    let rig = Rig::custom_lights(&[(did, "dmaker.fan.p5c", instance)]);
    let scope = rig
        .store
        .devices()
        .load_configuration()
        .unwrap()
        .scope
        .unwrap();
    let old_token = Token::parse("00112233445566778899aabbccddeeff").unwrap();
    let new_token = Token::parse("11223344556677889900aabbccddeeff").unwrap();
    let old = FakeDevice::new(old_token.clone(), old_token.clone());
    let new = FakeDevice::new(new_token.clone(), new_token.clone());
    let old_address = "127.0.0.2".parse().unwrap();
    let new_address = "127.0.0.3".parse().unwrap();
    rig.store
        .lan()
        .save(&LanRecord {
            scope: scope.clone(),
            did: did.into(),
            model: "dmaker.fan.p5c".into(),
            address: old_address,
            token: old_token,
        })
        .unwrap();
    rig.source
        .context
        .lan_test_endpoints
        .borrow_mut()
        .insert(old_address, old.address);
    rig.source
        .context
        .lan_test_endpoints
        .borrow_mut()
        .insert(new_address, new.address);
    rig.source
        .context
        .lan_timeout
        .set(Duration::from_millis(800));
    rig.clock.automatic.store(true, Ordering::Relaxed);
    block_on(rig.run(|control| {
        Box::pin(async move {
            until(|| control.context.direct_candidate(did).is_ok()).await;
            until(|| {
                function(&control.context.registry, did, "fan:2")
                    .snapshot()
                    .fan()
                    .unwrap()
                    .power
                    .value
                    == Some(true)
            })
            .await;
            old.hold_write.store(true, Ordering::Relaxed);
            let action = function(&control.context.registry, did, "fan:2")
                .begin_fan(FanCommand::Power(false))
                .unwrap();
            until(|| old.writes.lock().unwrap().len() == 1).await;
            control
                .context
                .live
                .borrow_mut()
                .devices
                .get_mut(did)
                .unwrap()
                .cloud_access = Some(CloudDeviceInfo {
                did: did.into(),
                model: "dmaker.fan.p5c".into(),
                instance_type: "fan".into(),
                online: false,
                lan: LanCredentials::Candidate(LanCandidate {
                    address: new_address,
                    token: new_token.clone(),
                }),
            });
            control.context.changed.notify(usize::MAX);
            until(|| {
                control
                    .store
                    .lan()
                    .load(&scope, did)
                    .unwrap()
                    .is_some_and(|record| {
                        record.address == new_address && record.token == new_token
                    })
            })
            .await;
            old.drop_write.store(drop_reply, Ordering::Relaxed);
            old.hold_write.store(false, Ordering::Relaxed);
            if drop_reply {
                assert_eq!(action.accepted().await, Err(CommandError::Timeout));
            } else {
                action.accepted().await.unwrap();
            }
            assert_eq!(old.writes.lock().unwrap().len(), 1);
            assert!(new.writes.lock().unwrap().is_empty());
            let status = control
                .context
                .diagnostics()
                .devices
                .into_iter()
                .find(|status| status.did == did)
                .unwrap();
            assert_eq!(
                status.commands[0].last.as_ref().unwrap().actual_path,
                Some(PathKind::Direct)
            );
            assert_eq!(status.selected_control, Some(PathKind::Direct));
            assert_eq!(status.lan.unwrap().state, LanState::Ready);
        })
    }));
}

#[test]
fn old_direct_read_cannot_override_a_new_cloud_read() {
    use crate::xiaomi::capability::PropertyBinding;
    use std::sync::atomic::Ordering;

    let did = "656878122";
    let instance = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/dmaker-p5c.json"
    ))
    .unwrap();
    let rig = Rig::custom_lights(&[(did, "dmaker.fan.p5c", instance)]);
    let scope = rig
        .store
        .devices()
        .load_configuration()
        .unwrap()
        .scope
        .unwrap();
    let token = Token::parse("00112233445566778899aabbccddeeff").unwrap();
    let peer = FakeDevice::new(token.clone(), token.clone());
    rig.store
        .lan()
        .save(&LanRecord {
            scope,
            did: did.into(),
            model: "dmaker.fan.p5c".into(),
            address: "127.0.0.1".parse().unwrap(),
            token,
        })
        .unwrap();
    rig.source.context.lan_port.set(peer.address.port());
    rig.clock.automatic.store(true, Ordering::Relaxed);
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.online.insert(did.into());
        for (siid, piid, value) in [
            (2, 1, json!(true)),
            (2, 3, json!(1)),
            (2, 4, json!(false)),
            (8, 1, json!(40)),
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
            let fan = function(&control.context.registry, did, "fan:2");
            until(|| fan.snapshot().fan().unwrap().power.value == Some(true)).await;
            let work = control.context.live.borrow().devices[did]
                .admitted
                .clone()
                .unwrap();
            let power = PropertyBinding { siid: 2, piid: 1 };
            until(|| !work.refresh.borrow().contains("fan:2", power)).await;
            peer.hold_read.store(true, Ordering::Relaxed);
            let reads = peer.reads.load(Ordering::Relaxed);
            work.refresh_property("fan:2", power);
            until(|| peer.reads.load(Ordering::Relaxed) > reads).await;
            peer.values.lock().unwrap().insert((2, 1), json!(false));
            control
                .context
                .live
                .borrow_mut()
                .devices
                .get_mut(did)
                .unwrap()
                .lan
                .active = None;
            control.context.refresh_changed.notify(usize::MAX);
            control.context.changed.notify(usize::MAX);
            until(|| {
                control
                    .cloud
                    .lock()
                    .unwrap()
                    .property_reads
                    .iter()
                    .any(|entry| entry == &(did.into(), 2, 1))
            })
            .await;
            until(|| work.property_source("fan:2", power) == Some(PathKind::Cloud)).await;
            peer.hold_read.store(false, Ordering::Relaxed);
            async_io::Timer::after(Duration::from_millis(40)).await;
            assert_eq!(fan.snapshot().fan().unwrap().power.value, Some(true));
            assert_eq!(work.property_source("fan:2", power), Some(PathKind::Cloud));
        })
    }));
}

#[test]
fn a_held_direct_read_does_not_block_the_next_read_for_another_device() {
    use crate::xiaomi::capability::PropertyBinding;
    use std::sync::atomic::Ordering;

    let dids = ["656878122", "656878123"];
    let instance: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/dmaker-p5c.json"
    ))
    .unwrap();
    let rig = Rig::custom_lights(&[
        (dids[0], "dmaker.fan.p5c", instance.clone()),
        (dids[1], "dmaker.fan.p5c", instance),
    ]);
    let scope = rig
        .store
        .devices()
        .load_configuration()
        .unwrap()
        .scope
        .unwrap();
    let token = Token::parse("00112233445566778899aabbccddeeff").unwrap();
    let first = FakeDevice::new(token.clone(), token.clone());
    let second = FakeDevice::with_did(token.clone(), token.clone(), 656878123);
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
                model: "dmaker.fan.p5c".into(),
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
    rig.clock.automatic.store(true, Ordering::Relaxed);
    block_on(rig.run(|control| {
        Box::pin(async move {
            until(|| {
                dids.iter()
                    .all(|did| control.context.direct_candidate(did).is_ok())
            })
            .await;
            let works = dids.map(|did| {
                control.context.live.borrow().devices[did]
                    .admitted
                    .clone()
                    .unwrap()
            });
            let power = PropertyBinding { siid: 2, piid: 1 };
            let speed = PropertyBinding { siid: 8, piid: 1 };
            let bindings = [
                power,
                PropertyBinding { siid: 2, piid: 3 },
                PropertyBinding { siid: 2, piid: 4 },
                speed,
            ];
            until(|| {
                works.iter().all(|work| {
                    bindings
                        .iter()
                        .all(|binding| !work.refresh.borrow().contains("fan:2", *binding))
                })
            })
            .await;
            first.hold_read.store(true, Ordering::Relaxed);
            let first_before = first.reads.load(Ordering::Relaxed);
            let second_before = second.reads.load(Ordering::Relaxed);
            works[0].refresh_property("fan:2", power);
            works[1].refresh_property("fan:2", power);
            until(|| {
                first.reads.load(Ordering::Relaxed) > first_before
                    && second.reads.load(Ordering::Relaxed) > second_before
            })
            .await;
            until(|| !works[1].refresh.borrow().contains("fan:2", power)).await;
            let completed = second.reads.load(Ordering::Relaxed);
            works[1].refresh_property("fan:2", speed);
            async_io::Timer::after(Duration::from_millis(100)).await;
            let progressed = second.reads.load(Ordering::Relaxed) > completed;
            first.hold_read.store(false, Ordering::Relaxed);
            assert!(
                progressed,
                "one held device must not block another device's subsequent read"
            );
        })
    }));
}

#[test]
fn cached_credentials_restore_offline_and_failed_candidate_never_replaces_them() {
    let instance = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/dmaker-p5c.json"
    ))
    .unwrap();
    let rig = Rig::custom_lights(&[("656878122", "dmaker.fan.p5c", instance)]);
    let before = rig.store.devices().load_configuration().unwrap();
    let scope = before.scope.clone().unwrap();
    let token = Token::parse("00112233445566778899aabbccddeeff").unwrap();
    let replacement = Token::parse("11223344556677889900aabbccddeeff").unwrap();
    let peer = FakeDevice::new(token.clone(), replacement.clone());
    let record = LanRecord {
        scope: scope.clone(),
        did: "656878122".into(),
        model: "dmaker.fan.p5c".into(),
        address: "127.0.0.1".parse().unwrap(),
        token: token.clone(),
    };
    rig.store.lan().save(&record).unwrap();
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
    let (adapter, _) = XiaomiAdapter::build(
        rig.store.clone(),
        rig.source.cloud.clone(),
        rig.discovery.clone(),
        rig.clock.clone(),
    )
    .unwrap();
    let context = adapter.context.clone();
    assert!(
        context
            .status
            .borrow()
            .authentication
            .certificate
            .unwrap()
            .not_after
            < rig.clock.wall()
    );
    context.lan_port.set(peer.address.port());
    context
        .lan_discovery
        .configure_test(peer.address, Duration::from_millis(20));
    block_on(async {
        future::or(
            async {
                future::or(
                    async {
                        adapter.run(|_| {}).await.unwrap();
                        panic!("adapter stopped");
                    },
                    async {
                        until(|| {
                            context.diagnostics().devices.iter().any(|device| {
                                device.did == record.did
                                    && device
                                        .lan
                                        .as_ref()
                                        .is_some_and(|status| status.state == LanState::Ready)
                            })
                        })
                        .await;
                        let status = context
                            .diagnostics()
                            .devices
                            .into_iter()
                            .find(|device| device.did == record.did)
                            .unwrap();
                        assert!(
                            status.selected_control.is_some(),
                            "authenticated LAN must permit offline control"
                        );
                        assert!(
                            status.selected_read.is_some(),
                            "authenticated LAN must permit offline reads"
                        );
                        assert_eq!(
                            rig.store.lan().load(&scope, &record.did).unwrap(),
                            Some(record.clone())
                        );
                        assert_eq!(rig.store.devices().load_configuration().unwrap(), before);
                        let received = peer.packets.load(std::sync::atomic::Ordering::Relaxed);
                        rig.clock.advance(Duration::from_secs(120));
                        async_io::Timer::after(Duration::from_millis(40)).await;
                        assert_eq!(
                            peer.packets.load(std::sync::atomic::Ordering::Relaxed),
                            received
                        );
                        let mut info = CloudDeviceInfo {
                            did: record.did.clone(),
                            model: record.model.clone(),
                            instance_type: "fan".into(),
                            online: false,
                            lan: LanCredentials::Candidate(LanCandidate {
                                address: record.address,
                                token: replacement,
                            }),
                        };
                        context
                            .live
                            .borrow_mut()
                            .devices
                            .get_mut(&record.did)
                            .unwrap()
                            .cloud_access = Some(info.clone());
                        context.changed.notify(usize::MAX);
                        until(|| {
                            context.diagnostics().devices.iter().any(|device| {
                                device
                                    .lan
                                    .as_ref()
                                    .is_some_and(|status| status.problem.is_some())
                            })
                        })
                        .await;
                        let status = context
                            .diagnostics()
                            .devices
                            .into_iter()
                            .find(|device| device.did == record.did)
                            .unwrap();
                        assert_eq!(status.lan.unwrap().state, LanState::Ready);
                        assert_eq!(
                            rig.store.lan().load(&scope, &record.did).unwrap(),
                            Some(record.clone())
                        );
                        info.lan = LanCredentials::NotIp;
                        context
                            .live
                            .borrow_mut()
                            .devices
                            .get_mut(&record.did)
                            .unwrap()
                            .cloud_access = Some(info);
                        context.changed.notify(usize::MAX);
                        until(|| {
                            context.live.borrow().devices[&record.did]
                                .lan
                                .status
                                .as_ref()
                                .is_some_and(|status| status.state == LanState::Ineligible)
                        })
                        .await;
                        assert_eq!(rig.store.lan().load(&scope, &record.did).unwrap(), None);
                        let physical = context
                            .registry
                            .get(&DeviceKey::new("xiaomi", &record.did))
                            .unwrap();
                        assert!(
                            matches!(physical.availability(), Availability::Unavailable(_)),
                            "an ineligible device without another path must become unavailable"
                        );
                    },
                )
                .await;
            },
            async {
                async_io::Timer::after(Duration::from_secs(3)).await;
                panic!("LAN scenario stalled");
            },
        )
        .await;
    });
}

#[test]
fn failed_device_has_one_backoff_recovery_and_no_healthy_polling() {
    use std::sync::atomic::Ordering;
    let instance = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/dmaker-p5c.json"
    ))
    .unwrap();
    let rig = Rig::custom_lights(&[("656878122", "dmaker.fan.p5c", instance)]);
    let scope = rig
        .store
        .devices()
        .load_configuration()
        .unwrap()
        .scope
        .unwrap();
    let token = Token::parse("00112233445566778899aabbccddeeff").unwrap();
    let peer = FakeDevice::new(token.clone(), token.clone());
    peer.drop_business.store(true, Ordering::Relaxed);
    let record = LanRecord {
        scope,
        did: "656878122".into(),
        model: "dmaker.fan.p5c".into(),
        address: "127.0.0.1".parse().unwrap(),
        token,
    };
    rig.store.lan().save(&record).unwrap();
    let (adapter, _) = XiaomiAdapter::build(
        rig.store.clone(),
        rig.source.cloud.clone(),
        rig.discovery.clone(),
        rig.clock.clone(),
    )
    .unwrap();
    let context = adapter.context.clone();
    context.lan_port.set(peer.address.port());
    context.lan_timeout.set(Duration::from_millis(40));
    context
        .lan_discovery
        .configure_test(peer.address, Duration::from_millis(20));
    block_on(async {
        future::or(
            async {
                future::or(
                    async {
                        future::or(super::super::lan::run(context.clone()), context.tasks.run())
                            .await
                            .unwrap();
                        panic!("LAN runner stopped");
                    },
                    async {
                        let scheduled = |delay| {
                            context.live.borrow().devices[&record.did]
                                .lan
                                .status
                                .as_ref()
                                .is_some_and(|status| {
                                    status.recovery
                                        == RecoveryStatus::Scheduled(rig.clock.wall() + delay)
                                })
                        };
                        until(|| scheduled(1)).await;
                        let packets = peer.packets.load(Ordering::Relaxed);
                        for _ in 0..5 {
                            context.changed.notify(usize::MAX);
                        }
                        async_io::Timer::after(Duration::from_millis(30)).await;
                        assert_eq!(peer.packets.load(Ordering::Relaxed), packets);
                        rig.clock.advance(Duration::from_secs(1));
                        until(|| scheduled(2)).await;
                        assert!(peer.packets.load(Ordering::Relaxed) > packets);
                        peer.drop_business.store(false, Ordering::Relaxed);
                        rig.clock.advance(Duration::from_secs(2));
                        until(|| {
                            context.live.borrow().devices[&record.did]
                                .lan
                                .status
                                .as_ref()
                                .is_some_and(|status| status.state == LanState::Ready)
                        })
                        .await;
                        let packets = peer.packets.load(Ordering::Relaxed);
                        rig.clock.advance(Duration::from_secs(120));
                        async_io::Timer::after(Duration::from_millis(40)).await;
                        assert_eq!(peer.packets.load(Ordering::Relaxed), packets);
                    },
                )
                .await;
            },
            async {
                async_io::Timer::after(Duration::from_secs(3)).await;
                panic!("LAN recovery stalled");
            },
        )
        .await;
    });
}

#[test]
fn enrollment_requires_successful_read_and_revocation_survives_restart() {
    use std::sync::atomic::Ordering;
    let instance = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/dmaker-p5c.json"
    ))
    .unwrap();
    let rig = Rig::custom_lights(&[("656878122", "dmaker.fan.p5c", instance)]);
    let scope = rig
        .store
        .devices()
        .load_configuration()
        .unwrap()
        .scope
        .unwrap();
    let token = Token::parse("00112233445566778899aabbccddeeff").unwrap();
    let replacement = Token::parse("11223344556677889900aabbccddeeff").unwrap();
    let peer = FakeDevice::new(token.clone(), replacement.clone());
    peer.reject_read.store(true, Ordering::Relaxed);
    let context = rig.source.context.clone();
    context.lan_port.set(peer.address.port());
    context.lan_timeout.set(Duration::from_millis(100));
    context
        .lan_discovery
        .configure_test(peer.address, Duration::from_millis(20));
    context
        .live
        .borrow_mut()
        .devices
        .get_mut("656878122")
        .unwrap()
        .cloud_access = Some(CloudDeviceInfo {
        did: "656878122".into(),
        model: "dmaker.fan.p5c".into(),
        instance_type: "fan".into(),
        online: false,
        lan: LanCredentials::Candidate(LanCandidate {
            address: "127.0.0.1".parse().unwrap(),
            token: token.clone(),
        }),
    });
    block_on(async {
        future::or(
            async {
                future::or(
                    async {
                        future::or(super::super::lan::run(context.clone()), context.tasks.run())
                            .await
                            .unwrap();
                        panic!("LAN runner stopped");
                    },
                    async {
                        until(|| {
                            context.live.borrow().devices["656878122"]
                                .lan
                                .status
                                .as_ref()
                                .is_some_and(|status| status.state == LanState::Failed)
                        })
                        .await;
                        assert_eq!(rig.store.lan().load(&scope, "656878122").unwrap(), None);
                        let packets = peer.packets.load(Ordering::Relaxed);
                        context.changed.notify(usize::MAX);
                        async_io::Timer::after(Duration::from_millis(40)).await;
                        assert_eq!(peer.packets.load(Ordering::Relaxed), packets);
                        peer.reject_read.store(false, Ordering::Relaxed);
                        peer.accept_replacement.store(true, Ordering::Relaxed);
                        context
                            .live
                            .borrow_mut()
                            .devices
                            .get_mut("656878122")
                            .unwrap()
                            .cloud_access
                            .as_mut()
                            .unwrap()
                            .lan = LanCredentials::Candidate(LanCandidate {
                            address: "127.0.0.1".parse().unwrap(),
                            token: replacement.clone(),
                        });
                        context.changed.notify(usize::MAX);
                        until(|| {
                            context.live.borrow().devices["656878122"]
                                .lan
                                .status
                                .as_ref()
                                .is_some_and(|status| status.state == LanState::Ready)
                        })
                        .await;
                        let record = rig.store.lan().load(&scope, "656878122").unwrap().unwrap();
                        assert_eq!(record.token, replacement);
                        context
                            .live
                            .borrow_mut()
                            .devices
                            .get_mut("656878122")
                            .unwrap()
                            .cloud_forbidden = true;
                        context.changed.notify(usize::MAX);
                        async_io::Timer::after(Duration::from_millis(30)).await;
                        assert!(
                            context.live.borrow().devices["656878122"]
                                .lan
                                .status
                                .as_ref()
                                .is_some_and(|status| status.state == LanState::Ready)
                        );
                        context.revoke("656878122", "test revocation").unwrap();
                        assert_eq!(rig.store.lan().load(&scope, "656878122").unwrap(), None);
                        let (restored, _) = XiaomiAdapter::build(
                            rig.store.clone(),
                            rig.source.cloud.clone(),
                            rig.discovery.clone(),
                            rig.clock.clone(),
                        )
                        .unwrap();
                        assert!(
                            restored
                                .context
                                .live
                                .borrow()
                                .devices
                                .get("656878122")
                                .is_none_or(|slot| slot.admitted.is_none())
                        );
                    },
                )
                .await;
            },
            async {
                async_io::Timer::after(Duration::from_secs(3)).await;
                panic!("LAN enrollment stalled");
            },
        )
        .await;
    });
}

#[test]
fn discovered_new_address_is_saved_only_after_authenticated_read() {
    let instance = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/dmaker-p5c.json"
    ))
    .unwrap();
    let rig = Rig::custom_lights(&[("656878122", "dmaker.fan.p5c", instance)]);
    let scope = rig
        .store
        .devices()
        .load_configuration()
        .unwrap()
        .scope
        .unwrap();
    let token = Token::parse("00112233445566778899aabbccddeeff").unwrap();
    let peer = FakeDevice::new(token.clone(), token.clone());
    let record = LanRecord {
        scope: scope.clone(),
        did: "656878122".into(),
        model: "dmaker.fan.p5c".into(),
        address: "127.0.0.2".parse().unwrap(),
        token,
    };
    rig.store.lan().save(&record).unwrap();
    let (adapter, _) = XiaomiAdapter::build(
        rig.store.clone(),
        rig.source.cloud.clone(),
        rig.discovery.clone(),
        rig.clock.clone(),
    )
    .unwrap();
    let context = adapter.context.clone();
    context.lan_port.set(peer.address.port());
    context.lan_timeout.set(Duration::from_millis(40));
    context
        .lan_discovery
        .configure_test(peer.address, Duration::from_millis(20));
    block_on(async {
        future::or(
            async {
                future::or(
                    async {
                        future::or(super::super::lan::run(context.clone()), context.tasks.run())
                            .await
                            .unwrap();
                        panic!("LAN runner stopped");
                    },
                    async {
                        until(|| {
                            context.live.borrow().devices[&record.did]
                                .lan
                                .status
                                .as_ref()
                                .is_some_and(|status| status.state == LanState::Ready)
                        })
                        .await;
                        let saved = rig.store.lan().load(&scope, &record.did).unwrap().unwrap();
                        assert_eq!(saved.address, std::net::Ipv4Addr::LOCALHOST);
                        assert_eq!(saved.token, record.token);
                    },
                )
                .await;
            },
            async {
                async_io::Timer::after(Duration::from_secs(3)).await;
                panic!("LAN address recovery stalled");
            },
        )
        .await;
    });
}

fn occupied_address_recovers(old: &FakeDevice, expected_old_business: usize, token: Token) {
    use std::sync::atomic::Ordering;
    let instance = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/dmaker-p5c.json"
    ))
    .unwrap();
    let rig = Rig::custom_lights(&[("656878122", "dmaker.fan.p5c", instance)]);
    let scope = rig
        .store
        .devices()
        .load_configuration()
        .unwrap()
        .scope
        .unwrap();
    let target = FakeDevice::new(token.clone(), token.clone());
    target.hold_read.store(true, Ordering::Relaxed);
    let record = LanRecord {
        scope: scope.clone(),
        did: "656878122".into(),
        model: "dmaker.fan.p5c".into(),
        address: "127.0.0.2".parse().unwrap(),
        token,
    };
    rig.store.lan().save(&record).unwrap();
    let (adapter, _) = XiaomiAdapter::build(
        rig.store.clone(),
        rig.source.cloud.clone(),
        rig.discovery.clone(),
        rig.clock.clone(),
    )
    .unwrap();
    let context = adapter.context.clone();
    context.lan_port.set(target.address.port());
    context.lan_timeout.set(Duration::from_millis(500));
    context
        .lan_test_endpoints
        .borrow_mut()
        .insert(record.address, old.address);
    context
        .lan_discovery
        .configure_test(target.address, Duration::from_millis(20));
    block_on(async {
        future::or(
            async {
                future::or(
                    async {
                        future::or(super::super::lan::run(context.clone()), context.tasks.run())
                            .await
                            .unwrap();
                        panic!("LAN runner stopped");
                    },
                    async {
                        until(|| target.reads.load(Ordering::Relaxed) > 0).await;
                        assert!(old.packets.load(Ordering::Relaxed) > 0);
                        assert_eq!(old.business.load(Ordering::Relaxed), expected_old_business);
                        assert_eq!(old.reads.load(Ordering::Relaxed), 0);
                        assert_eq!(
                            rig.store.lan().load(&scope, &record.did).unwrap(),
                            Some(record.clone())
                        );
                        target.hold_read.store(false, Ordering::Relaxed);
                        until(|| {
                            context.live.borrow().devices[&record.did]
                                .lan
                                .status
                                .as_ref()
                                .is_some_and(|status| status.state == LanState::Ready)
                        })
                        .await;
                        let saved = rig.store.lan().load(&scope, &record.did).unwrap().unwrap();
                        assert_eq!(saved.address, std::net::Ipv4Addr::LOCALHOST);
                        assert_eq!(saved.token, record.token);
                    },
                )
                .await;
            },
            async {
                async_io::Timer::after(Duration::from_secs(3)).await;
                panic!("occupied-address recovery stalled");
            },
        )
        .await;
    });
}

#[test]
fn wrong_did_at_old_address_receives_no_business_and_discovery_recovers() {
    let token = Token::parse("00112233445566778899aabbccddeeff").unwrap();
    let old = FakeDevice::with_did(token.clone(), token.clone(), 123456789);
    occupied_address_recovers(&old, 0, token);
}

#[test]
fn authenticated_identity_mismatch_at_old_address_still_discovers_target() {
    let token = Token::parse("00112233445566778899aabbccddeeff").unwrap();
    let other = Token::parse("11223344556677889900aabbccddeeff").unwrap();
    let old = FakeDevice::new(other, token.clone());
    occupied_address_recovers(&old, 1, token);
}

#[test]
fn identity_mismatch_without_discovery_stops_without_retries() {
    use std::{net::UdpSocket, sync::atomic::Ordering};
    let instance = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/dmaker-p5c.json"
    ))
    .unwrap();
    let rig = Rig::custom_lights(&[("656878122", "dmaker.fan.p5c", instance)]);
    let scope = rig
        .store
        .devices()
        .load_configuration()
        .unwrap()
        .scope
        .unwrap();
    let token = Token::parse("00112233445566778899aabbccddeeff").unwrap();
    let other = Token::parse("11223344556677889900aabbccddeeff").unwrap();
    let old = FakeDevice::new(other, token.clone());
    let discovery = UdpSocket::bind("127.0.0.1:0").unwrap();
    discovery
        .set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    let record = LanRecord {
        scope: scope.clone(),
        did: "656878122".into(),
        model: "dmaker.fan.p5c".into(),
        address: "127.0.0.2".parse().unwrap(),
        token,
    };
    rig.store.lan().save(&record).unwrap();
    let (adapter, _) = XiaomiAdapter::build(
        rig.store.clone(),
        rig.source.cloud.clone(),
        rig.discovery.clone(),
        rig.clock.clone(),
    )
    .unwrap();
    let context = adapter.context.clone();
    context
        .lan_test_endpoints
        .borrow_mut()
        .insert(record.address, old.address);
    context.lan_timeout.set(Duration::from_millis(500));
    context
        .lan_discovery
        .configure_test(discovery.local_addr().unwrap(), Duration::from_millis(20));
    block_on(async {
        future::or(
            async {
                future::or(
                    async {
                        future::or(super::super::lan::run(context.clone()), context.tasks.run())
                            .await
                            .unwrap();
                        panic!("LAN runner stopped");
                    },
                    async {
                        until(|| {
                            context.live.borrow().devices[&record.did]
                                .lan
                                .status
                                .as_ref()
                                .is_some_and(|status| {
                                    status.state == LanState::Failed
                                        && status.recovery == RecoveryStatus::None
                                })
                        })
                        .await;
                        let mut datagram = [0; 64];
                        let (length, _) = discovery
                            .recv_from(&mut datagram)
                            .expect("identity mismatch must trigger discovery");
                        assert_eq!(length, 32);
                        assert_eq!(&datagram[..2], &[0x21, 0x31]);
                        context.changed.notify(usize::MAX);
                        rig.clock.advance(Duration::from_secs(120));
                        async_io::Timer::after(Duration::from_millis(40)).await;
                        assert!(
                            discovery.recv_from(&mut datagram).is_err(),
                            "identity mismatch must not keep scanning"
                        );
                        assert_eq!(old.business.load(Ordering::Relaxed), 1);
                        assert_eq!(old.reads.load(Ordering::Relaxed), 0);
                        assert_eq!(
                            rig.store.lan().load(&scope, &record.did).unwrap(),
                            Some(record.clone())
                        );
                    },
                )
                .await;
            },
            async {
                async_io::Timer::after(Duration::from_secs(3)).await;
                panic!("identity mismatch scenario stalled");
            },
        )
        .await;
    });
}
