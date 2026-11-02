use super::*;

#[test]
fn standard_miot_direct_poll_recovers_an_external_fan_change() {
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
    block_on(rig.run(|control| async move {
        let work = control.context.live.borrow().devices[did]
            .admitted
            .clone()
            .unwrap();
        let bindings = work.native.device().functions()[0].observation_bindings();
        until(|| {
            control.context.direct_candidate(did).is_ok()
                && bindings
                    .iter()
                    .all(|binding| !work.refresh.borrow().contains("fan:2", *binding))
                && function(&control.context.registry, did, "fan:2")
                    .snapshot()
                    .fan()
                    .unwrap()
                    .speed
                    .value
                    .is_some()
        })
        .await;
        control.clock.automatic.store(false, Ordering::Relaxed);
        let before = peer.reads.load(Ordering::Relaxed);
        peer.values.lock().unwrap().insert((8, 1), json!(72));
        control.clock.advance(Duration::from_secs(17));
        control.clock.automatic.store(true, Ordering::Relaxed);
        until(|| {
            function(&control.context.registry, did, "fan:2")
                .snapshot()
                .fan()
                .unwrap()
                .speed
                .value
                == Some(72)
        })
        .await;
        assert!(peer.reads.load(Ordering::Relaxed) > before);
    }));
}

#[test]
fn p5c_direct_fan_command_reads_feedback_once_after_all_steps() {
    use crate::device::FanCommand;
    use crate::xiaomi::adapter::StepReceipt;
    use std::sync::atomic::Ordering;
    let did = "656878122";
    let instance = serde_json::from_str(include_str!(
        "../../../../../tests/fixtures/miot/dmaker-p5c.json"
    ))
    .unwrap();
    let rig = Rig::custom_lights(&[(did, "dmaker.fan.p5c", instance)]);
    let token = Token::parse("00112233445566778899aabbccddeeff").unwrap();
    let peer = FakeDevice::new(token.clone(), token.clone());
    peer.values.lock().unwrap().insert((2, 1), json!(false));
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
            did: did.into(),
            model: "dmaker.fan.p5c".into(),
            address: "127.0.0.1".parse().unwrap(),
            token,
        })
        .unwrap();
    rig.source.context.lan_port.set(peer.address.port());
    rig.clock.automatic.store(true, Ordering::Relaxed);
    block_on(rig.run(|control| {
        Box::pin(async move {
            let fan = function(&control.context.registry, did, "fan:2");
            until(|| fan.snapshot().fan().unwrap().speed.value == Some(40)).await;
            let work = control.context.live.borrow().devices[did]
                .admitted
                .clone()
                .unwrap();
            let bindings = work.native.device().functions()[0].observation_bindings();
            until(|| {
                bindings
                    .iter()
                    .all(|binding| !work.refresh.borrow().contains("fan:2", *binding))
            })
            .await;
            let before = peer.reads.load(Ordering::Relaxed);
            peer.hold_read.store(true, Ordering::Relaxed);
            fan.begin_fan(FanCommand::Speed(55))
                .unwrap()
                .accepted()
                .await
                .unwrap();
            until(|| peer.reads.load(Ordering::Relaxed) > before).await;
            assert_eq!(peer.writes.lock().unwrap().len(), 3);
            assert_eq!(fan.snapshot().fan().unwrap().power.value, Some(false));
            peer.hold_read.store(false, Ordering::Relaxed);
            until(|| {
                bindings
                    .iter()
                    .all(|binding| !work.refresh.borrow().contains("fan:2", *binding))
            })
            .await;
            assert_eq!(peer.reads.load(Ordering::Relaxed), before + 4);
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
            assert_eq!(last.actual_path, Some(PathKind::Direct));
            assert!(
                last.steps
                    .iter()
                    .all(|step| step.receipt == StepReceipt::Accepted)
            );
            assert_eq!(last.steps.len(), 3);
        })
    }));
}
