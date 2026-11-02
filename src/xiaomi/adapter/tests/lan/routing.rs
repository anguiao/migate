use super::*;

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
    let result = block_on(super::super::super::lan::set_properties(
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
    let incomplete = block_on(super::super::super::lan::set_properties(
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
        "../../../../../tests/fixtures/miot/dmaker-p5c.json"
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
fn hub_restoration_keeps_sent_direct_receipt_and_degraded_hub_uses_direct_then_cloud() {
    use crate::device::FanCommand;
    use std::sync::atomic::Ordering;

    let did = "656878122";
    let instance = serde_json::from_str(include_str!(
        "../../../../../tests/fixtures/miot/dmaker-p5c.json"
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
                    Some(super::super::super::routing::ControlPath::Direct(_))
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
                                Some(super::super::super::routing::ControlPath::Local(_))
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
                            Some(super::super::super::routing::ControlPath::Direct(_))
                        ));
                        assert!(matches!(
                            context.read_path(did),
                            Some(super::super::super::routing::ControlPath::Direct(_))
                        ));
                        context.stop_lan(did);
                        assert!(matches!(
                            context.control_path(did),
                            Some(super::super::super::routing::ControlPath::Cloud(_))
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
                            Some(super::super::super::routing::ControlPath::Local(_))
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
        "../../../../../tests/fixtures/miot/dmaker-p5c.json"
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
fn old_direct_read_cannot_override_a_new_cloud_read() {
    use crate::xiaomi::capability::PropertyBinding;
    use std::sync::atomic::Ordering;

    let did = "656878122";
    let instance = serde_json::from_str(include_str!(
        "../../../../../tests/fixtures/miot/dmaker-p5c.json"
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
        "../../../../../tests/fixtures/miot/dmaker-p5c.json"
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
