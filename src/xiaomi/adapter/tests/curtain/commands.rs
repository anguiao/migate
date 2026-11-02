use super::*;

#[test]
fn curtain_local_commands_write_one_quantized_property_without_refresh() {
    use crate::device::CurtainCommand;
    let mut description: Value = instance();
    description["services"][1]["properties"][3]["value-range"] = json!([0, 100, 5]);
    let rig = Rig::custom_lights(&[("one", "xiaomi.curtain.acn010", description)]);
    block_on(rig.run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(peer(), |peer| async move {
            settled(control).await;
            let function = function(&control.context.registry, "one", "curtain:2");
            let before = function.snapshot();
            let reads = peer.count("get", "one", 0);
            for command in [
                CurtainCommand::Target(100.0),
                CurtainCommand::Target(0.0),
                CurtainCommand::Target(27.4),
            ] {
                function
                    .begin_curtain(command)
                    .unwrap()
                    .accepted()
                    .await
                    .unwrap();
            }
            for line in ["list", "status one"] {
                let output = terminal(control, line).await;
                assert!(
                    output.contains("target 27.4%; properties 2.4=25; actual path hub; accepted."),
                    "{output}"
                );
            }
            function
                .begin_curtain(CurtainCommand::Stop)
                .unwrap()
                .accepted()
                .await
                .unwrap();
            for _ in 0..20 {
                future::yield_now().await;
            }
            let writes = peer
                .state
                .borrow()
                .seen
                .iter()
                .filter(|(op, _)| op == "rpcReq")
                .map(|(_, payload)| payload["rpc"]["params"].as_array().unwrap().clone())
                .collect::<Vec<_>>();
            assert_eq!(writes.len(), 4);
            let actual = writes
                .iter()
                .map(|items| {
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
                    (json!(2), json!(4), json!(100)),
                    (json!(2), json!(4), json!(0)),
                    (json!(2), json!(4), json!(25)),
                    (json!(2), json!(1), json!(2))
                ]
            );
            assert_eq!(function.snapshot(), before);
            assert_eq!(peer.count("get", "one", 0), reads);
            let records = control
                .context
                .diagnostics()
                .devices
                .into_iter()
                .find(|device| device.did == "one")
                .unwrap()
                .commands;
            let last = records[0].last.as_ref().unwrap();
            assert_eq!(last.target.to_string(), "stop");
            assert_eq!(
                last.properties,
                vec![super::super::model::CommandProperty {
                    binding: PropertyBinding { siid: 2, piid: 1 },
                    value: json!(2),
                    path: Some(PathKind::Local),
                }]
            );
            assert_eq!(last.actual_path, Some(PathKind::Local));
            assert_eq!(last.result, Some(Ok(())));
        })
        .await;
    }));
}

#[test]
fn curtain_cloud_commands_write_single_values_and_retain_the_request_in_diagnostics() {
    use crate::device::CurtainCommand;
    let (rig, listener, cloud) = cloud_rig();
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            settled(&c).await;
            let function = function(&c.context.registry, "one", "curtain:2");
            let before = function.snapshot();
            assert_eq!(function.matter_curtain().1, Some(25));
            let reads = c.cloud.lock().unwrap().property_reads.len();
            function
                .begin_curtain(CurtainCommand::Target(32.6))
                .unwrap()
                .accepted()
                .await
                .unwrap();
            for _ in 0..20 {
                future::yield_now().await;
            }
            let writes = c.cloud.lock().unwrap().numeric_writes.clone();
            assert_eq!(writes.len(), 1);
            assert_eq!(writes[0].0.len(), 1);
            assert_eq!(writes[0].0[0]["siid"], 2);
            assert_eq!(writes[0].0[0]["piid"], 4);
            assert_eq!(writes[0].0[0]["value"], 33);
            assert_eq!(function.snapshot().curtain(), before.curtain());
            assert_eq!(function.snapshot().availability, before.availability);
            assert_eq!(function.matter_curtain().1, Some(75));
            assert_eq!(c.cloud.lock().unwrap().property_reads.len(), reads);
            let records = c
                .context
                .diagnostics()
                .devices
                .into_iter()
                .find(|device| device.did == "one")
                .unwrap()
                .commands;
            let last = records[0].last.as_ref().unwrap();
            assert_eq!(last.target.to_string(), "32.6%");
            assert_eq!(
                last.properties,
                vec![super::super::model::CommandProperty {
                    binding: TARGET,
                    value: json!(33),
                    path: Some(PathKind::Cloud),
                }]
            );
            assert_eq!(last.actual_path, Some(PathKind::Cloud));
            assert_eq!(last.result, Some(Ok(())));
        })
        .await;
    }));
}

#[test]
fn curtain_local_stop_waits_for_old_receipt_and_other_device_is_independent() {
    use crate::device::{CommandError, CurtainCommand};
    let description: Value = instance();
    let rig = Rig::custom_lights(&[
        ("one", "xiaomi.curtain.acn010", description.clone()),
        ("two", "xiaomi.curtain.acn010", description),
    ]);
    block_on(rig.run(|c| async move {
        c.announce();
        let peer = peer();
        for (binding, value) in [(STATUS, 2), (CURRENT, 20), (TARGET, 60)] {
            peer.value("two", binding, json!(value));
        }
        let control = &c;
        c.peer(peer, |peer| async move {
            settled(control).await;
            until(|| {
                function(&control.context.registry, "two", "curtain:2")
                    .snapshot()
                    .availability
                    == Availability::Available
            })
            .await;
            peer.state.borrow_mut().hold_writes = true;
            let one = function(&control.context.registry, "one", "curtain:2");
            let two = function(&control.context.registry, "two", "curtain:2");
            let before = one.snapshot();
            let first = one.begin_curtain(CurtainCommand::Target(10.0)).unwrap();
            until(|| peer.count("rpcReq", "one", 0) == 1).await;
            let stale = one.begin_curtain(CurtainCommand::Target(20.0)).unwrap();
            let latest = one.begin_curtain(CurtainCommand::Target(30.0)).unwrap();
            assert_eq!(stale.accepted().await, Err(CommandError::Superseded));
            let stop = one.begin_curtain(CurtainCommand::Stop).unwrap();
            assert_eq!(latest.accepted().await, Err(CommandError::Superseded));
            let after = one.begin_curtain(CurtainCommand::Target(90.0)).unwrap();
            let other = two.begin_curtain(CurtainCommand::Target(55.0)).unwrap();
            until(|| peer.count("rpcReq", "two", 0) == 1).await;
            assert_eq!(peer.count("rpcReq", "one", 0), 1);
            for line in ["list", "status one"] {
                let output = terminal(control, line).await;
                assert!(
                    output.contains("queued curtain commands: stop, 90%."),
                    "{output}"
                );
                assert!(
                    output.contains(
                        "target 10%; properties 2.4=10; actual path hub; awaiting result."
                    ),
                    "{output}"
                );
            }
            peer.release_property(
                "two",
                TARGET,
                json!({"result":[{"did":"two","siid":2,"piid":4,"code":0}]}),
            )
            .await;
            other.accepted().await.unwrap();
            peer.release_property(
                "one",
                TARGET,
                json!({"result":[{"did":"one","siid":2,"piid":4,"code":0}]}),
            )
            .await;
            first.accepted().await.unwrap();
            until(|| peer.count("rpcReq", "one", 0) == 2).await;
            let motor = PropertyBinding { siid: 2, piid: 1 };
            peer.release_property(
                "one",
                motor,
                json!({"result":[{"did":"one","siid":2,"piid":1,"code":0}]}),
            )
            .await;
            stop.accepted().await.unwrap();
            until(|| peer.count("rpcReq", "one", 0) == 3).await;
            peer.release_property(
                "one",
                TARGET,
                json!({"result":[{"did":"one","siid":2,"piid":4,"code":0}]}),
            )
            .await;
            after.accepted().await.unwrap();
            let writes = peer
                .state
                .borrow()
                .seen
                .iter()
                .filter(|(op, value)| op == "rpcReq" && value["did"] == "one")
                .map(|(_, value)| value["rpc"]["params"][0].clone())
                .collect::<Vec<_>>();
            assert_eq!(
                writes
                    .iter()
                    .map(|item| (item["piid"].clone(), item["value"].clone()))
                    .collect::<Vec<_>>(),
                vec![
                    (json!(4), json!(10)),
                    (json!(1), json!(2)),
                    (json!(4), json!(90))
                ]
            );
            assert_eq!(one.snapshot(), before);
        })
        .await;
    }));
}

#[test]
fn curtain_without_ready_notifications_reads_a_single_post_command_snapshot() {
    use crate::device::CurtainCommand;
    block_on(rig().run(|c| async move {
        c.announce();
        let peer = peer();
        peer.state
            .borrow_mut()
            .sub_reject
            .insert(("one".into(), CURRENT.siid, CURRENT.piid));
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| {
                curtain(control).current_position.value == Some(37)
                    && work(control)
                        .function_subscription_problem("curtain:2")
                        .is_some()
                    && [STATUS, CURRENT, TARGET].iter().all(|binding| {
                        !work(control)
                            .refresh
                            .borrow()
                            .contains("curtain:2", *binding)
                    })
            })
            .await;
            assert!(work(control).function_observer("curtain:2").is_none());
            peer.value("one", STATUS, json!(1));
            peer.value("one", CURRENT, json!(48));
            peer.value("one", TARGET, json!(90));
            peer.state.borrow_mut().hold_writes = true;
            let before = peer.count("get", "one", 0);
            let action = function(&control.context.registry, "one", "curtain:2")
                .begin_curtain(CurtainCommand::Target(90.0))
                .unwrap();
            until(|| peer.count("rpcReq", "one", 0) == 1).await;
            assert_eq!(peer.count("get", "one", 0), before);
            peer.release_property(
                "one",
                TARGET,
                json!({"result":[{"did":"one","siid":2,"piid":4,"code":0}]}),
            )
            .await;
            action.accepted().await.unwrap();
            async_io::Timer::after(Duration::from_millis(500)).await;
            assert_eq!(peer.count("get", "one", 0), before + 3);
            assert_eq!(curtain(control).current_position.value, Some(48));
            assert_eq!(curtain(control).status.value, Some(CurtainMotion::Closing));
        })
        .await;
    }));
}

#[test]
fn post_command_read_failure_retries_without_changing_acceptance() {
    use crate::device::CurtainCommand;
    use crate::xiaomi::adapter::StepReceipt;
    block_on(rig().run(|c| async move {
        c.announce();
        let peer = peer();
        peer.state
            .borrow_mut()
            .sub_reject
            .insert(("one".into(), 2, 3));
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| {
                curtain(control).current_position.value == Some(37)
                    && work(control)
                        .function_subscription_problem("curtain:2")
                        .is_some()
                    && [STATUS, CURRENT, TARGET].iter().all(|binding| {
                        !work(control)
                            .refresh
                            .borrow()
                            .contains("curtain:2", *binding)
                    })
            })
            .await;
            control
                .clock
                .automatic
                .store(false, std::sync::atomic::Ordering::Relaxed);
            peer.state
                .borrow_mut()
                .read_fail
                .insert(("one".into(), 2, 3));
            let before = peer.count("get", "one", 0);
            let action = function(&control.context.registry, "one", "curtain:2")
                .begin_curtain(CurtainCommand::Target(90.0))
                .unwrap();
            action.accepted().await.unwrap();
            until(|| {
                let failed = work(control)
                    .refresh
                    .borrow()
                    .property_problem("curtain:2", CURRENT)
                    .is_some();
                if !failed {
                    control.clock.advance(Duration::from_millis(200));
                }
                failed
            })
            .await;
            let last = control.context.diagnostics().devices[0].commands[0]
                .last
                .clone()
                .unwrap();
            assert_eq!(last.result, Some(Ok(())));
            assert_eq!(last.steps[0].receipt, StepReceipt::Accepted);
            peer.state
                .borrow_mut()
                .read_fail
                .remove(&("one".into(), 2, 3));
            control.clock.advance(Duration::from_secs(3));
            until(|| {
                let finished = [STATUS, CURRENT, TARGET].iter().all(|binding| {
                    !work(control)
                        .refresh
                        .borrow()
                        .contains("curtain:2", *binding)
                });
                if !finished {
                    control.clock.advance(Duration::from_millis(200));
                }
                finished
            })
            .await;
            assert!(peer.count("get", "one", 0) >= before + 4);
            assert!(
                work(control)
                    .refresh
                    .borrow()
                    .property_problem("curtain:2", CURRENT)
                    .is_none()
            );
            assert_eq!(peer.count("rpcReq", "one", 0), 1);
        })
        .await;
    }));
}

#[test]
fn rejected_curtain_write_does_not_request_a_one_shot_read() {
    use crate::device::CurtainCommand;
    use crate::xiaomi::adapter::StepReceipt;
    block_on(rig().run(|c| async move {
        c.announce();
        let peer = peer();
        peer.state
            .borrow_mut()
            .sub_reject
            .insert(("one".into(), 2, 3));
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| {
                curtain(control).current_position.value == Some(37)
                    && work(control)
                        .function_subscription_problem("curtain:2")
                        .is_some()
                    && [STATUS, CURRENT, TARGET].iter().all(|binding| {
                        !work(control)
                            .refresh
                            .borrow()
                            .contains("curtain:2", *binding)
                    })
            })
            .await;
            peer.state.borrow_mut().hold_writes = true;
            let before = peer.count("get", "one", 0);
            let action = function(&control.context.registry, "one", "curtain:2")
                .begin_curtain(CurtainCommand::Target(90.0))
                .unwrap();
            until(|| peer.count("rpcReq", "one", 0) == 1).await;
            peer.release_property(
                "one",
                TARGET,
                json!({"result":[{"did":"one","siid":2,"piid":4,"code":-704}]}),
            )
            .await;
            assert!(action.accepted().await.is_err());
            async_io::Timer::after(Duration::from_millis(100)).await;
            assert_eq!(peer.count("get", "one", 0), before);
            let last = control.context.diagnostics().devices[0].commands[0]
                .last
                .clone()
                .unwrap();
            assert!(matches!(last.steps[0].receipt, StepReceipt::Rejected(_)));
        })
        .await;
    }));
}

#[test]
fn old_inflight_read_cannot_satisfy_or_overwrite_the_post_write_snapshot() {
    use crate::device::CurtainCommand;
    block_on(rig().run(|c| async move {
        c.announce();
        let peer = peer();
        peer.state
            .borrow_mut()
            .sub_reject
            .insert(("one".into(), 2, 3));
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| {
                curtain(control).current_position.value == Some(37)
                    && work(control)
                        .function_subscription_problem("curtain:2")
                        .is_some()
                    && [STATUS, CURRENT, TARGET].iter().all(|binding| {
                        !work(control)
                            .refresh
                            .borrow()
                            .contains("curtain:2", *binding)
                    })
            })
            .await;
            peer.state.borrow_mut().hold_reads = true;
            let before = peer.count("get", "one", 0);
            work(control).refresh_property("curtain:2", CURRENT);
            until(|| peer.count("get", "one", 0) == before + 1).await;
            peer.value("one", CURRENT, json!(48));
            function(&control.context.registry, "one", "curtain:2")
                .begin_curtain(CurtainCommand::Target(90.0))
                .unwrap()
                .accepted()
                .await
                .unwrap();
            peer.state.borrow_mut().hold_reads = false;
            peer.release_property("one", CURRENT, json!({"value":10}))
                .await;
            until(|| {
                [STATUS, CURRENT, TARGET].iter().all(|binding| {
                    !work(control)
                        .refresh
                        .borrow()
                        .contains("curtain:2", *binding)
                })
            })
            .await;
            assert_eq!(curtain(control).current_position.value, Some(48));
            assert_eq!(peer.count("get", "one", 0), before + 4);
        })
        .await;
    }));
}

#[test]
fn accepted_command_without_a_read_route_refreshes_after_recovery() {
    use crate::device::CurtainCommand;
    use crate::xiaomi::adapter::PathPriority;
    block_on(rig().run(|c| async move {
        c.announce();
        let peer = peer();
        peer.state
            .borrow_mut()
            .sub_reject
            .insert(("one".into(), 2, 3));
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| {
                curtain(control).current_position.value == Some(37)
                    && work(control)
                        .function_subscription_problem("curtain:2")
                        .is_some()
                    && [STATUS, CURRENT, TARGET].iter().all(|binding| {
                        !work(control)
                            .refresh
                            .borrow()
                            .contains("curtain:2", *binding)
                    })
            })
            .await;
            peer.state.borrow_mut().hold_writes = true;
            let before = peer.count("get", "one", 0);
            let action = function(&control.context.registry, "one", "curtain:2")
                .begin_curtain(CurtainCommand::Target(90.0))
                .unwrap();
            until(|| peer.count("rpcReq", "one", 0) == 1).await;
            control.context.priority.set(PathPriority::CloudFirst);
            assert!(control.context.read_path_for("one", "curtain:2").is_none());
            peer.release_property(
                "one",
                TARGET,
                json!({"result":[{"did":"one","siid":2,"piid":4,"code":0}]}),
            )
            .await;
            action.accepted().await.unwrap();
            let last = control.context.diagnostics().devices[0].commands[0]
                .last
                .clone()
                .unwrap();
            assert_eq!(last.result, Some(Ok(())));
            assert!([STATUS, CURRENT, TARGET].iter().all(|binding| {
                work(control)
                    .refresh
                    .borrow()
                    .contains("curtain:2", *binding)
            }));
            control.context.priority.set(PathPriority::LocalFirst);
            work(control).refresh_changed.notify(usize::MAX);
            until(|| peer.count("get", "one", 0) == before + 3).await;
        })
        .await;
    }));
}

#[test]
fn cloud_curtain_control_without_notifications_reads_one_snapshot() {
    use crate::device::CurtainCommand;
    use crate::xiaomi::adapter::StepReceipt;
    let (rig, listener, cloud) = cloud_rig();
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            settled(&c).await;
            c.clock
                .automatic
                .store(false, std::sync::atomic::Ordering::Relaxed);
            cloud.send(Packet::Disconnect(Disconnect::new()));
            until(|| {
                work(&c).function_observer("curtain:2").is_none()
                    && [STATUS, CURRENT, TARGET]
                        .iter()
                        .all(|binding| !work(&c).refresh.borrow().contains("curtain:2", *binding))
            })
            .await;
            {
                let mut http = c.cloud.lock().unwrap();
                http.raw_values.insert(("one".into(), 2, 2), json!(1));
                http.raw_values.insert(("one".into(), 2, 3), json!(40));
                http.raw_values.insert(("one".into(), 2, 4), json!(90));
            }
            let before = c.cloud.lock().unwrap().property_reads.len();
            let action = function(&c.context.registry, "one", "curtain:2")
                .begin_curtain(CurtainCommand::Target(90.0))
                .unwrap();
            action.accepted().await.unwrap();
            until(|| {
                c.cloud.lock().unwrap().property_reads.len() == before + 3
                    && [STATUS, CURRENT, TARGET]
                        .iter()
                        .all(|binding| !work(&c).refresh.borrow().contains("curtain:2", *binding))
            })
            .await;
            assert_eq!(c.cloud.lock().unwrap().property_reads.len(), before + 3);
            assert_eq!(curtain(&c).current_position.value, Some(40));
            assert_eq!(curtain(&c).status.value, Some(CurtainMotion::Closing));
            let status = c.context.diagnostics();
            let last = status.devices[0].commands[0].last.as_ref().unwrap();
            assert_eq!(last.actual_path, Some(PathKind::Cloud));
            assert_eq!(last.steps[0].receipt, StepReceipt::Accepted);
            c.clock.advance(Duration::from_secs(5));
            for _ in 0..100 {
                future::yield_now().await;
            }
            assert_eq!(c.cloud.lock().unwrap().property_reads.len(), before + 3);
        })
        .await;
    }));
}
