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
                    && work(&c).fan_mode_observation("main").value == Some(1)
                    && fan(&c).swing.value == Some(false)
                    && [POWER, MODE, SPEED, SWING]
                        .iter()
                        .all(|binding| !work(&c).refresh.borrow().contains("main", *binding))
            })
            .await;
            let function = function(&c.context.registry, "one", "main");
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
                    until(|| fan(control).power.value == Some(false) && fan(control).speed.value == Some(40) && work(control).fan_mode_observation("main").value == Some(1) && fan(control).swing.value == Some(false)).await;
                    peer.state.borrow_mut().hold_writes = true;
                    let function = function(&control.context.registry, "one", "main");
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
                            && work(&c).fan_mode_observation("main").value == Some(1)
                            && fan(&c).swing.value == Some(false)
                    })
                    .await;
                    let function = function(&c.context.registry, "one", "main");
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
