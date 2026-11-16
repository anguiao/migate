use super::*;
use crate::device::Power;

#[test]
fn light_notifications_survive_a_rejected_heat_level_subscription() {
    let rig = Rig::custom_lights(&[(DID, "yeelink.bhf_light.v13", v13())]);
    block_on(rig.run(|c| async move {
        c.announce();
        let hub = standard_peer();
        hub.state.borrow_mut().sub_reject.insert((DID.into(), 3, 5));
        let control = &c;
        c.peer(hub, |hub| async move {
            let light = function(control, "light:2");
            let device = work(control);
            eventually(|| {
                device.function_observer("light:2").is_some()
                    && device.function_observer(BLOW).is_some()
                    && light.snapshot().power() == Some(Power::Off)
            })
            .await;
            assert!(device.function_observer(HEATER).is_none());
            hub.notify_value(
                DID,
                PropertyBinding { siid: 2, piid: 1 },
                json!(true),
                false,
            );
            eventually(|| light.snapshot().power() == Some(Power::On)).await;
        })
        .await;
    }));
}

#[test]
fn initial_reads_include_the_shared_group_until_success_or_failure() {
    for valid in [true, false] {
        let (rig, listener, cloud) = rig();
        let source = rig.source.source_handle();
        let (release, gate) = flume::bounded(1);
        {
            let mut state = rig.cloud.lock().unwrap();
            state.rpc_gate = Some(gate);
            if !valid {
                state.rpc_results.insert("get_prop".into(), json!([]));
            }
        }
        block_on(rig.run(|c| async move {
            future::or(cloud.run(&listener), async {
                let config = c.store.devices().load_configuration().unwrap();
                let device = work(&c);
                eventually(|| {
                    let refresh = device.refresh.borrow();
                    !c.cloud.lock().unwrap().rpc_reads.is_empty()
                        && device.native.device().functions().iter().all(|function| {
                            function
                                .observation_bindings()
                                .into_iter()
                                .all(|binding| refresh.initial_complete(function.id(), binding))
                        })
                })
                .await;
                assert!(!source.initial_reads_ready(&config));
                c.cloud.lock().unwrap().rpc_gate = None;
                release.send(()).unwrap();
                eventually(|| source.initial_reads_ready(&config)).await;
                assert_eq!(device.shared_feedback().source.is_some(), valid);
            })
            .await;
        }));
    }
}

#[test]
fn a_malformed_fan_switch_is_a_property_read_failure() {
    let (rig, listener, cloud) = rig();
    rig.cloud
        .lock()
        .unwrap()
        .raw_values
        .insert((DID.into(), 3, 2), json!("on"));
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            let device = work(&c);
            eventually(|| {
                device
                    .refresh
                    .borrow()
                    .property_problem(BLOW, BLOW_POWER)
                    .is_some()
            })
            .await;
            assert!(device.refresh.borrow().initial_complete(BLOW, BLOW_POWER));
            assert_eq!(
                function(&c, BLOW).snapshot().fan().unwrap().power.value,
                None
            );
        })
        .await;
    }));
}
