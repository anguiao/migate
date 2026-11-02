use super::*;

#[test]
fn direct_command_refreshes_even_when_cloud_notifications_are_ready() {
    let did = "308540429";
    let (rig, peer) = setup(did);
    let (rig, listener, cloud) = with_cloud_rig(rig);
    block_on(rig.run(|control| async move {
        future::or(cloud.run(&listener), async {
            let ac = function(&control.context.registry, did, "air-conditioner:2");
            let work = control.context.live.borrow().devices[did]
                .admitted
                .clone()
                .unwrap();
            let bindings = work.native.device().functions()[0].observation_bindings();
            until(|| {
                work.function_observer("air-conditioner:2")
                    .is_some_and(|source| work.source_ready("air-conditioner:2", &source))
                    && control.context.direct_candidate(did).is_ok()
                    && bindings.iter().all(|binding| {
                        !work
                            .refresh
                            .borrow()
                            .contains("air-conditioner:2", *binding)
                    })
                    && ac.snapshot().air_conditioner().unwrap().temperature.value == Some(24)
            })
            .await;
            let before = peer
                .requests
                .lock()
                .unwrap()
                .iter()
                .filter(|request| request["method"] == "get_prop")
                .count();
            ac.begin_air_conditioner(AirConditionerCommand::Temperature(26))
                .unwrap()
                .accepted()
                .await
                .unwrap();
            until(|| ac.snapshot().air_conditioner().unwrap().temperature.value == Some(26)).await;
            assert_eq!(
                peer.requests
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|request| request["method"] == "get_prop")
                    .count(),
                before + 1
            );
            let status = control.context.diagnostics();
            let device = status
                .devices
                .iter()
                .find(|device| device.did == did)
                .unwrap();
            assert_eq!(
                device.functions[0].notification_source,
                Some(PathKind::Cloud)
            );
            assert_eq!(
                device.commands[0].last.as_ref().unwrap().actual_path,
                Some(PathKind::Direct)
            );
        })
        .await;
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
fn mcn02_partial_acceptance_reads_once_after_all_sent_steps() {
    use crate::xiaomi::adapter::StepReceipt;
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
                bindings.iter().all(|binding| {
                    !work
                        .refresh
                        .borrow()
                        .contains("air-conditioner:2", *binding)
                })
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
