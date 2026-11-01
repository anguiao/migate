use super::*;
use crate::device::{CommandError, FanCommand};
use futures_lite::future::poll_once;

#[test]
fn cloud_speed_waits_for_each_single_property_receipt() {
    let (rig, listener, cloud) = cloud_rig();
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            until(|| {
                fan(&c).power.value == Some(false)
                    && fan(&c).speed.value == Some(40)
                    && work(&c).fan_mode_observation("fan:2").value == Some(1)
                    && fan(&c).swing.value == Some(false)
                    && [POWER, MODE, SPEED, SWING]
                        .iter()
                        .all(|binding| !work(&c).refresh.borrow().contains("fan:2", *binding))
            })
            .await;
            let function = function(&c.context.registry, "one", "fan:2");
            let before = function.snapshot();
            let (release, gate) = flume::bounded(1);
            c.cloud
                .lock()
                .unwrap()
                .write_gates
                .insert("one".into(), gate);
            let action = function.begin_fan(FanCommand::Speed(40)).unwrap();
            until(|| {
                let cloud = c.cloud.lock().unwrap();
                !cloud.numeric_writes.is_empty() || !cloud.property_writes.is_empty()
            })
            .await;
            assert_eq!(c.cloud.lock().unwrap().numeric_writes.len(), 1);
            assert!(poll_once(action.accepted()).await.is_none());
            release.send(()).unwrap();
            until(|| c.cloud.lock().unwrap().numeric_writes.len() == 2).await;
            assert!(poll_once(action.accepted()).await.is_none());
            release.send(()).unwrap();
            until(|| c.cloud.lock().unwrap().numeric_writes.len() == 3).await;
            assert!(poll_once(action.accepted()).await.is_none());
            release.send(()).unwrap();
            action.accepted().await.unwrap();
            let cloud = c.cloud.lock().unwrap();
            let actual = cloud
                .numeric_writes
                .iter()
                .map(|(items, _)| {
                    assert_eq!(items.len(), 1);
                    (
                        items[0]["siid"].clone(),
                        items[0]["piid"].clone(),
                        items[0]["value"].clone(),
                    )
                })
                .collect::<Vec<_>>();
            assert_eq!(
                actual,
                vec![
                    (json!(2), json!(1), json!(true)),
                    (json!(2), json!(3), json!(0)),
                    (json!(8), json!(1), json!(40))
                ]
            );
            assert_eq!(function.snapshot(), before);
        })
        .await;
    }));
}

#[test]
fn each_local_step_failure_or_timeout_stops_the_sequence() {
    for failure in 0..3 {
        for timeout in [false, true] {
            block_on(rig().run(|c| async move {
                c.announce();
                let peer = peer();
                peer.value("one", POWER, json!(false));
                let control = &c;
                c.peer(peer, |peer| async move {
                    until(|| fan(control).power.value == Some(false) && fan(control).speed.value == Some(40) && work(control).fan_mode_observation("fan:2").value == Some(1) && fan(control).swing.value == Some(false)).await;
                    peer.state.borrow_mut().hold_writes = true;
                    let function = function(&control.context.registry, "one", "fan:2");
                    let before = function.snapshot().fan();
                    let action = function.begin_fan(FanCommand::Speed(56)).unwrap();
                    let steps = [POWER, MODE, SPEED];
                    for (index, binding) in steps.iter().copied().enumerate().take(failure + 1) {
                        until(|| peer.count("rpcReq", "one", 0) == index + 1).await;
                        if index < failure || !timeout {
                            let code = if index == failure { -704 } else { 0 };
                            peer.release_property("one", binding, json!({"result":[{"did":"one","siid":binding.siid,"piid":binding.piid,"code":code}]})).await;
                        }
                    }
                    let result = action.accepted().await;
                    if timeout {
                        assert_eq!(result, Err(CommandError::Timeout));
                    } else {
                        assert!(matches!(result, Err(CommandError::Rejected(_))), "{result:?}");
                    }
                    for _ in 0..30 { future::yield_now().await; }
                    assert_eq!(peer.count("rpcReq", "one", 0), failure + 1);
                    assert!(control.cloud.lock().unwrap().numeric_writes.is_empty());
                    assert_eq!(function.snapshot().fan(), before);
                    let last = control.context.diagnostics().devices[0].commands[0].last.clone().unwrap();
                    assert_eq!(last.result, Some(result.map(|_| ())));
                    assert_eq!(last.properties.len(), failure + 1);
                    assert_eq!(last.properties.iter().map(|property| property.path.unwrap()).collect::<Vec<_>>(), vec![PathKind::Local; failure + 1]);
                }).await;
            }));
        }
    }
}

#[test]
fn each_cloud_step_failure_or_timeout_stops_the_sequence() {
    for failure in 0..3 {
        for timeout in [false, true] {
            let (rig, listener, cloud) = cloud_rig();
            block_on(rig.run(|c| async move {
                future::or(cloud.run(&listener), async {
                    until(|| {
                        fan(&c).power.value == Some(false)
                            && fan(&c).speed.value == Some(40)
                            && work(&c).fan_mode_observation("fan:2").value == Some(1)
                            && fan(&c).swing.value == Some(false)
                    })
                    .await;
                    let function = function(&c.context.registry, "one", "fan:2");
                    let before = function.snapshot().fan();
                    let (release, gate) = flume::bounded(1);
                    let steps = [POWER, MODE, SPEED];
                    {
                        let mut cloud = c.cloud.lock().unwrap();
                        cloud.write_gates.insert("one".into(), gate);
                        if !timeout {
                            let failed = steps[failure];
                            cloud
                                .property_codes
                                .insert(("one".into(), failed.siid, failed.piid), -704);
                        }
                    }
                    let action = function.begin_fan(FanCommand::Speed(56)).unwrap();
                    for index in 0..=failure {
                        until(|| c.cloud.lock().unwrap().numeric_writes.len() == index + 1).await;
                        if index < failure || !timeout {
                            release.send(()).unwrap();
                        }
                    }
                    let result = action.accepted().await;
                    if timeout {
                        assert_eq!(result, Err(CommandError::Timeout));
                    } else {
                        assert!(
                            matches!(result, Err(CommandError::Rejected(_))),
                            "{result:?}"
                        );
                    }
                    for _ in 0..30 {
                        future::yield_now().await;
                    }
                    assert_eq!(c.cloud.lock().unwrap().numeric_writes.len(), failure + 1);
                    assert_eq!(function.snapshot().fan(), before);
                    let last = c.context.diagnostics().devices[0].commands[0]
                        .last
                        .clone()
                        .unwrap();
                    assert_eq!(last.result, Some(result.map(|_| ())));
                    assert_eq!(last.properties.len(), failure + 1);
                    assert_eq!(
                        last.properties
                            .iter()
                            .map(|property| property.path.unwrap())
                            .collect::<Vec<_>>(),
                        vec![PathKind::Cloud; failure + 1]
                    );
                })
                .await;
            }));
        }
    }
}

#[test]
fn partial_fan_acceptance_reads_only_after_the_last_step_and_keeps_each_receipt() {
    use crate::xiaomi::adapter::{CommandFeedback, StepReceipt};
    block_on(rig().run(|c| async move {
        c.announce();
        let peer = peer();
        peer.value("one", POWER, json!(false));
        peer.state
            .borrow_mut()
            .sub_reject
            .insert(("one".into(), MODE.siid, MODE.piid));
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| {
                fan(control).power.value == Some(false)
                    && fan(control).speed.value == Some(40)
                    && work(control)
                        .function_subscription_problem("fan:2")
                        .is_some()
                    && [POWER, MODE, SPEED, SWING]
                        .iter()
                        .all(|binding| !work(control).refresh.borrow().contains("fan:2", *binding))
            })
            .await;
            let before = peer.count("get", "one", 0);
            peer.state.borrow_mut().hold_writes = true;
            let action = function(&control.context.registry, "one", "fan:2")
                .begin_fan(FanCommand::Speed(56))
                .unwrap();
            until(|| peer.count("rpcReq", "one", 0) == 1).await;
            peer.release_property(
                "one",
                POWER,
                json!({"result":[{"did":"one","siid":2,"piid":1,"code":0}]}),
            )
            .await;
            until(|| peer.count("rpcReq", "one", 0) == 2).await;
            assert_eq!(peer.count("get", "one", 0), before);
            peer.release_property(
                "one",
                MODE,
                json!({"result":[{"did":"one","siid":2,"piid":3,"code":-704}]}),
            )
            .await;
            assert!(matches!(
                action.accepted().await,
                Err(CommandError::Rejected(_))
            ));
            until(|| {
                control.context.diagnostics().devices[0].commands[0]
                    .last
                    .as_ref()
                    .is_some_and(|last| last.feedback == Some(CommandFeedback::Complete))
            })
            .await;
            assert_eq!(peer.count("get", "one", 0), before + 4);
            let last = control.context.diagnostics().devices[0].commands[0]
                .last
                .clone()
                .unwrap();
            assert_eq!(last.steps.len(), 2);
            assert_eq!(last.steps[0].receipt, StepReceipt::Accepted);
            assert!(matches!(last.steps[1].receipt, StepReceipt::Rejected(_)));
            let seen = &peer.state.borrow().seen;
            let last_write = seen
                .iter()
                .rposition(|(operation, _)| operation == "rpcReq")
                .unwrap();
            assert_eq!(
                seen[last_write + 1..]
                    .iter()
                    .filter(|(operation, _)| operation == "get")
                    .count(),
                4
            );
        })
        .await;
    }));
}

#[test]
fn fan_partial_acceptance_then_timeout_reads_over_the_cloud_fallback() {
    use crate::xiaomi::adapter::{CommandFeedback, StepReceipt};
    let (rig, listener, cloud) = cloud_rig();
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            c.announce();
            let peer = peer();
            peer.value("one", POWER, json!(false));
            peer.state
                .borrow_mut()
                .sub_reject
                .insert(("one".into(), MODE.siid, MODE.piid));
            let control = &c;
            let cloud = &cloud;
            c.peer(peer, |peer| async move {
                until(|| {
                    fan(control).power.value == Some(false)
                        && work(control)
                            .function_subscription_problem("fan:2")
                            .is_some()
                        && work(control).function_observer("fan:2").is_some()
                        && [POWER, MODE, SPEED, SWING].iter().all(|binding| {
                            !work(control).refresh.borrow().contains("fan:2", *binding)
                        })
                })
                .await;
                control
                    .clock
                    .automatic
                    .store(false, std::sync::atomic::Ordering::Relaxed);
                cloud.send(Packet::Disconnect(Disconnect::new()));
                until(|| work(control).function_observer("fan:2").is_none()).await;
                let before = control.cloud.lock().unwrap().property_reads.len();
                peer.state.borrow_mut().hold_writes = true;
                let action = function(&control.context.registry, "one", "fan:2")
                    .begin_fan(FanCommand::Speed(56))
                    .unwrap();
                until(|| peer.count("rpcReq", "one", 0) == 1).await;
                peer.release_property(
                    "one",
                    POWER,
                    json!({"result":[{"did":"one","siid":2,"piid":1,"code":0}]}),
                )
                .await;
                until(|| peer.count("rpcReq", "one", 0) == 2).await;
                assert_eq!(control.cloud.lock().unwrap().property_reads.len(), before);
                assert_eq!(action.accepted().await, Err(CommandError::Timeout));
                until(|| {
                    control.context.diagnostics().devices[0].commands[0]
                        .last
                        .as_ref()
                        .is_some_and(|last| last.feedback == Some(CommandFeedback::Complete))
                })
                .await;
                assert_eq!(
                    control.cloud.lock().unwrap().property_reads.len(),
                    before + 4
                );
                let status = control.context.diagnostics();
                let last = status.devices[0].commands[0].last.as_ref().unwrap();
                assert_eq!(last.steps[0].receipt, StepReceipt::Accepted);
                assert!(matches!(last.steps[1].receipt, StepReceipt::Uncertain(_)));
                assert_eq!(peer.count("rpcReq", "one", 0), 2);
            })
            .await;
        })
        .await;
    }));
}
