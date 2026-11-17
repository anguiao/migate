use super::*;

#[test]
fn commands_are_concurrent_across_channels_and_fifo_with_execution_time_toggle() {
    block_on(Rig::new(true, &["one", "multi"]).run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        let control = &c;
        c.peer(peer, |peer| async move {
            initialized(control, &peer, &["one", "multi"]).await;
            peer.state.borrow_mut().hold_writes = true;
            let a = function(&control.context, "one", "outlet:2");
            let b = function(&control.context, "multi", "outlet:3");
            let d = function(&control.context, "multi", "outlet:4");
            let mut first = Box::pin(a.execute(Command::On));
            let mut second = Box::pin(a.execute(Command::Toggle));
            let mut third = Box::pin(a.execute(Command::Off));
            let mut other = Box::pin(b.execute(Command::On));
            let mut channel = Box::pin(d.execute(Command::On));
            for command in [
                &mut first,
                &mut second,
                &mut third,
                &mut other,
                &mut channel,
            ] {
                assert!(poll_once(command.as_mut()).await.is_none());
            }
            until(|| {
                peer.count("rpcReq", "one", 2) == 1
                    && peer.count("rpcReq", "multi", 3) == 1
                    && peer.count("rpcReq", "multi", 4) == 1
            })
            .await;
            control.clock.advance(Duration::from_secs(6));
            assert!(poll_once(second.as_mut()).await.is_none());
            peer.release("multi", 4, accepted("multi", 4, 1)).await;
            assert_eq!(channel.await.unwrap().power(), Some(Power::On));
            peer.release("multi", 3, accepted("multi", 3, 0)).await;
            other.await.unwrap();
            peer.release("one", 2, accepted("one", 2, 0)).await;
            first.await.unwrap();
            until(|| peer.count("rpcReq", "one", 2) == 2).await;
            assert_eq!(
                peer.state
                    .borrow()
                    .seen
                    .iter()
                    .filter(|(op, value)| op == "rpcReq" && value["did"] == "one")
                    .nth(1)
                    .unwrap()
                    .1["rpc"]["params"][0]["value"],
                false
            );
            peer.release("one", 2, accepted("one", 2, 1)).await;
            second.await.unwrap();
            until(|| peer.count("rpcReq", "one", 2) == 3).await;
            peer.release("one", 2, accepted("one", 2, 0)).await;
            third.await.unwrap();
            assert_eq!(a.snapshot().power(), Some(Power::Off));
        })
        .await;
    }));
}

#[test]
fn an_offline_event_before_an_active_success_reply_preserves_acceptance() {
    block_on(Rig::new(true, &["one", "two"]).run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        let control = &c;
        c.peer(peer, |peer| async move {
            initialized(control, &peer, &["one", "two"]).await;
            peer.state.borrow_mut().hold_writes = true;
            let one = function(&control.context, "one", "outlet:2");
            let mut command = Box::pin(one.execute(Command::On));
            assert!(poll_once(command.as_mut()).await.is_none());
            until(|| peer.count("rpcReq", "one", 2) == 1).await;
            peer.state.borrow_mut().online.insert("one".into(), false);
            peer.list_changed(&["one"]);
            until(|| matches!(one.snapshot().availability, Availability::Unavailable(_))).await;
            assert!(poll_once(command.as_mut()).await.is_none());
            peer.release("one", 2, accepted("one", 2, 0)).await;
            let accepted = command.await.unwrap();
            assert_eq!(accepted.power(), Some(Power::On));
            assert!(matches!(
                accepted.availability,
                Availability::Unavailable(_)
            ));
            assert_eq!(
                function(&control.context, "two", "outlet:2")
                    .snapshot()
                    .availability,
                Availability::Available
            );
        })
        .await;
    }));
}

#[test]
fn a_late_success_for_an_obsolete_native_instance_does_not_override_the_current_value() {
    block_on(Rig::light("xiaomi.light.ceil04").run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        peer.value(
            "one",
            crate::xiaomi::capability::PropertyBinding { siid: 2, piid: 1 },
            json!(false),
        );
        peer.value(
            "one",
            crate::xiaomi::capability::PropertyBinding { siid: 2, piid: 2 },
            json!(50),
        );
        peer.value(
            "one",
            crate::xiaomi::capability::PropertyBinding { siid: 2, piid: 3 },
            json!(2700),
        );
        let control = &c;
        c.peer(peer, |peer| async move {
            initialized(control, &peer, &["one"]).await;
            peer.state.borrow_mut().hold_writes = true;
            let one = function(&control.context, "one", "light:2");
            let mut command = Box::pin(one.execute(Command::On));
            assert!(poll_once(command.as_mut()).await.is_none());
            until(|| peer.count("rpcReq", "one", 2) == 1).await;
            let before_physical = control
                .context
                .physical(&DeviceKey::new("xiaomi", "one"))
                .unwrap();
            let mut changed = control.context.directory.borrow().devices["one"]
                .work
                .as_ref()
                .unwrap()
                .native
                .clone();
            changed = crate::xiaomi::capability::NativeDescription::new(
                changed.model().to_owned(),
                changed.instance_type().to_owned(),
                super::support::device_with_temperature_grid(
                    changed.device(),
                    crate::device::NumericRange::new(2700, 6500, 1).unwrap(),
                ),
            )
            .unwrap();
            control
                .context
                .directory
                .borrow_mut()
                .devices
                .get_mut("one")
                .unwrap()
                .latest_description
                .as_mut()
                .unwrap()
                .native = Some(changed);
            control.context.reconcile("one").unwrap();
            let current_physical = loop {
                let current = control
                    .context
                    .physical(&DeviceKey::new("xiaomi", "one"))
                    .unwrap();
                if !before_physical.same_instance(&current) {
                    break current;
                }
                future::yield_now().await;
            };
            let current = current_physical.function("light:2").unwrap();
            until(|| current.snapshot().power() == Some(Power::Off)).await;
            peer.release("one", 2, accepted("one", 2, 0)).await;
            // A sent light receipt keeps its ownership and settles the
            // original request; it must not project into the new instance.
            assert!(
                command.await.is_ok(),
                "the original light receipt settles after the sink is retired"
            );
            assert!(matches!(
                one.snapshot().availability,
                Availability::Unavailable(_)
            ));
            assert_ne!(one.snapshot().power(), Some(Power::On));
            until(|| current.snapshot().availability == Availability::Available).await;
            assert_eq!(current.snapshot().power(), Some(Power::Off));
            peer.state.borrow_mut().hold_writes = false;
            current.execute(Command::On).await.unwrap();
            assert_eq!(peer.count("rpcReq", "one", 2), 2);
            assert!(
                peer.state.borrow().seen.iter().any(|(operation, value)| {
                    operation == "rpcReq"
                        && value["did"] == "one"
                        && value["rpc"]["params"][0]["siid"] == 2
                        && value["rpc"]["params"][0]["piid"] == 1
                        && value["rpc"]["params"][0]["value"] == true
                }),
                "the replacement instance sends its own absolute power target"
            );
            assert_eq!(
                current.snapshot().power(),
                Some(Power::Off),
                "light acceptance does not infer actual power before feedback"
            );
        })
        .await;
    }));
}

#[test]
fn restored_and_duplicate_description_sync_keeps_channel_commands() {
    let mut rig = Rig::new(true, &["multi"]);
    let stable = rig.data[1].clone();
    (rig.source, rig.ready) = rig.reopen_adapter();
    block_on(rig.run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        let control = &c;
        c.peer(peer, |peer| async move {
            initialized(control, &peer, &["multi"]).await;
            let key = DeviceKey::new("xiaomi", "multi");
            let physical = control.context.physical(&key).unwrap();
            let runtime_order = physical
                .functions()
                .iter()
                .map(|function| function.function_key().id.clone())
                .collect::<Vec<_>>();
            let cached_order = stable
                .device()
                .functions()
                .iter()
                .map(|function| function.id().to_owned())
                .collect::<Vec<_>>();
            assert_eq!(runtime_order, cached_order);
            for (id, siid) in [("outlet:3", 3), ("outlet:4", 4), ("outlet:9", 9)] {
                physical
                    .function(id)
                    .unwrap()
                    .execute(Command::On)
                    .await
                    .unwrap();
                assert_eq!(peer.count("rpcReq", "multi", siid), 1);
            }
            control
                .context
                .directory
                .borrow_mut()
                .devices
                .get_mut("multi")
                .unwrap()
                .latest_description
                .as_mut()
                .unwrap()
                .native = Some(stable.clone());
            control.context.reconcile("multi").unwrap();
            let current = control.context.physical(&key).unwrap();
            assert!(
                physical.same_instance(&current),
                "an identical duplicate description keeps the serving instance"
            );
            for (id, siid) in [("outlet:3", 3), ("outlet:4", 4), ("outlet:9", 9)] {
                physical
                    .function(id)
                    .unwrap()
                    .execute(Command::Off)
                    .await
                    .unwrap();
                assert_eq!(peer.count("rpcReq", "multi", siid), 2);
            }
        })
        .await;
    }));
}

#[test]
fn rejected_write_keeps_a_prior_inflight_read_valid() {
    use crate::xiaomi::capability::PropertyBinding;
    block_on(Rig::new(true, &["one"]).run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        let control = &c;
        c.peer(peer, |peer| async move {
            initialized(control, &peer, &["one"]).await;
            let work = control.context.directory.borrow().devices["one"]
                .work
                .clone()
                .unwrap();
            let binding = PropertyBinding { siid: 2, piid: 1 };
            until(|| !work.refresh.borrow().contains("outlet:2", binding)).await;
            peer.state.borrow_mut().hold_reads = true;
            peer.state.borrow_mut().hold_writes = true;
            let before = peer.count("get", "one", 2);
            work.refresh_property("outlet:2", binding);
            until(|| peer.count("get", "one", 2) == before + 1).await;
            let outlet = function(&control.context, "one", "outlet:2");
            let mut action = Box::pin(outlet.execute(Command::On));
            assert!(poll_once(action.as_mut()).await.is_none());
            until(|| {
                peer.state
                    .borrow()
                    .held
                    .iter()
                    .any(|(publish, _)| publish.topic.rsplit('/').next() == Some("rpcReq"))
            })
            .await;
            let index = peer
                .state
                .borrow()
                .held
                .iter()
                .position(|(publish, _)| publish.topic.rsplit('/').next() == Some("rpcReq"))
                .unwrap();
            let (publish, _) = peer.state.borrow_mut().held.remove(index);
            let client = peer.state.borrow().client_id.clone();
            peer.send(response(&client, &publish, accepted("one", 2, -704)));
            assert!(action.await.is_err());
            peer.release_property("one", binding, json!({"value":true}))
                .await;
            until(|| outlet.snapshot().power() == Some(Power::On)).await;
            assert_eq!(peer.count("get", "one", 2), before + 1);
        })
        .await;
    }));
}

#[test]
fn old_read_before_the_receipt_does_not_suppress_accepted_power() {
    use crate::xiaomi::capability::PropertyBinding;
    for pushless in [false, true] {
        block_on(Rig::new(true, &["one"]).run(|c| async move {
            c.announce();
            let peer = Rc::new(Peer::default());
            if pushless {
                peer.state.borrow_mut().pushless.insert("one".into());
            }
            let control = &c;
            c.peer(peer, |peer| async move {
                until(|| {
                    control
                        .context
                        .physical(&DeviceKey::new("xiaomi", "one"))
                        .and_then(|device| device.function("outlet:2"))
                        .is_some_and(|function| function.snapshot().power() == Some(Power::Off))
                })
                .await;
                let work = control.context.directory.borrow().devices["one"]
                    .work
                    .clone()
                    .unwrap();
                let binding = PropertyBinding { siid: 2, piid: 1 };
                until(|| {
                    work.function_observer("outlet:2").is_none() == pushless
                        && !work.refresh.borrow().contains("outlet:2", binding)
                })
                .await;
                peer.state.borrow_mut().hold_reads = true;
                peer.state.borrow_mut().hold_writes = true;
                let before = peer.count("get", "one", 2);
                work.refresh_property("outlet:2", binding);
                until(|| peer.count("get", "one", 2) == before + 1).await;
                let outlet = function(&control.context, "one", "outlet:2");
                let mut action = Box::pin(outlet.execute(Command::On));
                assert!(poll_once(action.as_mut()).await.is_none());
                until(|| peer.count("rpcReq", "one", 2) == 1).await;
                peer.state.borrow_mut().hold_reads = false;
                peer.release_property("one", binding, json!({"value":false}))
                    .await;
                until(|| !work.refresh.borrow().contains("outlet:2", binding)).await;
                peer.release("one", 2, accepted("one", 2, 0)).await;
                assert_eq!(action.await.unwrap().power(), Some(Power::On));
                if pushless {
                    until(|| peer.count("get", "one", 2) >= before + 2).await;
                }
            })
            .await;
        }));
    }
}

#[test]
fn outlet_without_notifications_uses_the_shared_post_write_feedback() {
    use crate::xiaomi::adapter::StepReceipt;
    block_on(Rig::new(true, &["one"]).run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        peer.state.borrow_mut().pushless.insert("one".into());
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| {
                control
                    .context
                    .physical(&DeviceKey::new("xiaomi", "one"))
                    .and_then(|device| device.function("outlet:2"))
                    .is_some_and(|function| function.snapshot().power() == Some(Power::Off))
            })
            .await;
            let work = control.context.directory.borrow().devices["one"]
                .work
                .clone()
                .unwrap();
            until(|| {
                !work.refresh.borrow().contains(
                    "outlet:2",
                    crate::xiaomi::capability::PropertyBinding { siid: 2, piid: 1 },
                )
            })
            .await;
            assert!(work.function_observer("outlet:2").is_none());
            let before = peer.count("get", "one", 2);
            function(&control.context, "one", "outlet:2")
                .execute(Command::On)
                .await
                .unwrap();
            until(|| peer.count("get", "one", 2) == before + 1).await;
            assert_eq!(peer.count("get", "one", 2), before + 1);
            let status = control.context.diagnostics();
            assert_eq!(
                status.devices[0].commands[0].last.as_ref().unwrap().steps[0].receipt,
                StepReceipt::Accepted
            );
        })
        .await;
    }));
}

#[test]
fn outlet_post_write_read_before_receipt_remains_the_observed_state() {
    use crate::xiaomi::capability::PropertyBinding;
    block_on(Rig::new(true, &["one"]).run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        peer.state.borrow_mut().pushless.insert("one".into());
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| {
                control
                    .context
                    .physical(&DeviceKey::new("xiaomi", "one"))
                    .and_then(|device| device.function("outlet:2"))
                    .is_some_and(|function| function.snapshot().power() == Some(Power::Off))
            })
            .await;
            let work = control.context.directory.borrow().devices["one"]
                .work
                .clone()
                .unwrap();
            let binding = PropertyBinding { siid: 2, piid: 1 };
            until(|| !work.refresh.borrow().contains("outlet:2", binding)).await;
            peer.state.borrow_mut().hold_writes = true;
            let before = peer.count("get", "one", 2);
            let outlet = function(&control.context, "one", "outlet:2");
            let mut action = Box::pin(outlet.execute(Command::On));
            assert!(poll_once(action.as_mut()).await.is_none());
            until(|| peer.count("rpcReq", "one", 2) == 1).await;
            work.refresh_property("outlet:2", binding);
            until(|| {
                peer.count("get", "one", 2) == before + 1
                    && !work.refresh.borrow().contains("outlet:2", binding)
            })
            .await;
            peer.release("one", 2, accepted("one", 2, 0)).await;
            action.await.unwrap();
            assert_eq!(outlet.snapshot().power(), Some(Power::Off));
            until(|| {
                peer.count("get", "one", 2) == before + 2
                    && !work.refresh.borrow().contains("outlet:2", binding)
            })
            .await;
        })
        .await;
    }));
}

#[test]
fn outlet_receipt_refreshes_again_after_a_post_write_inflight_read() {
    use crate::xiaomi::capability::PropertyBinding;
    block_on(Rig::new(true, &["one"]).run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        peer.state.borrow_mut().pushless.insert("one".into());
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| {
                control
                    .context
                    .physical(&DeviceKey::new("xiaomi", "one"))
                    .and_then(|device| device.function("outlet:2"))
                    .is_some_and(|function| function.snapshot().power() == Some(Power::Off))
            })
            .await;
            let work = control.context.directory.borrow().devices["one"]
                .work
                .clone()
                .unwrap();
            let binding = PropertyBinding { siid: 2, piid: 1 };
            until(|| !work.refresh.borrow().contains("outlet:2", binding)).await;
            peer.state.borrow_mut().hold_writes = true;
            peer.state.borrow_mut().hold_reads = true;
            let before = peer.count("get", "one", 2);
            let outlet = function(&control.context, "one", "outlet:2");
            let mut action = Box::pin(outlet.execute(Command::On));
            assert!(poll_once(action.as_mut()).await.is_none());
            until(|| peer.count("rpcReq", "one", 2) == 1).await;
            work.refresh_property("outlet:2", binding);
            until(|| peer.count("get", "one", 2) == before + 1).await;
            peer.release("one", 2, accepted("one", 2, 0)).await;
            assert_eq!(action.await.unwrap().power(), Some(Power::On));
            peer.state.borrow_mut().hold_reads = false;
            peer.release_property("one", binding, json!({"value":false}))
                .await;
            until(|| {
                peer.count("get", "one", 2) == before + 2
                    && !work.refresh.borrow().contains("outlet:2", binding)
            })
            .await;
            assert_eq!(outlet.snapshot().power(), Some(Power::Off));
        })
        .await;
    }));
}

#[test]
fn outlet_read_started_before_the_write_does_not_satisfy_feedback() {
    use crate::xiaomi::capability::PropertyBinding;
    block_on(Rig::new(true, &["one"]).run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        peer.state.borrow_mut().pushless.insert("one".into());
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| {
                control
                    .context
                    .physical(&DeviceKey::new("xiaomi", "one"))
                    .and_then(|device| device.function("outlet:2"))
                    .is_some_and(|function| function.snapshot().power() == Some(Power::Off))
            })
            .await;
            let work = control.context.directory.borrow().devices["one"]
                .work
                .clone()
                .unwrap();
            let binding = PropertyBinding { siid: 2, piid: 1 };
            until(|| !work.refresh.borrow().contains("outlet:2", binding)).await;
            peer.state.borrow_mut().hold_reads = true;
            let before = peer.count("get", "one", 2);
            work.refresh_property("outlet:2", binding);
            until(|| peer.count("get", "one", 2) == before + 1).await;
            let outlet = function(&control.context, "one", "outlet:2");
            outlet.execute(Command::On).await.unwrap();
            assert_eq!(outlet.snapshot().power(), Some(Power::On));
            peer.state.borrow_mut().hold_reads = false;
            peer.release_property("one", binding, json!({"value":true}))
                .await;
            until(|| {
                peer.count("get", "one", 2) == before + 2
                    && !work.refresh.borrow().contains("outlet:2", binding)
            })
            .await;
            assert_eq!(peer.count("get", "one", 2), before + 2);
            assert_eq!(outlet.snapshot().power(), Some(Power::Off));
        })
        .await;
    }));
}

#[test]
fn cp7pd_matter_channels_and_master_trigger_preserve_the_outlet_layout() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            block_on(Rig::new(true, &["multi"]).run(|c| async move {
                c.announce();
                let peer = Rc::new(Peer::default());
                let control = &c;
                c.peer(peer, |peer| async move {
                    initialized(control, &peer, &["multi"]).await;
                    let entries = control.publication.matter_configuration().entries;
                    assert_eq!(entries.len(), 1);
                    let entry = &entries[0];
                    assert!(entry.identity.relay.is_none());
                    let ids = ["outlet:3", "outlet:4", "outlet:9"];
                    let endpoints = ids.map(|id| {
                        entry
                            .identity
                            .functions
                            .iter()
                            .find(|item| item.id == id)
                            .unwrap()
                            .endpoint
                    });
                    let model = crate::matter::tests::support::PublishedRelayModel::new(
                        &control.publication,
                        &control.store,
                    );
                    model.check_outlet_topology(entry.identity.endpoint, &endpoints);
                    assert_eq!(model.parts(entry.identity.endpoint).await, endpoints);
                    for (index, endpoint) in endpoints.iter().enumerate() {
                        model.invoke(*endpoint, 1).await.unwrap();
                        assert_eq!(peer.count("rpcReq", "multi", [3, 4, 9][index]), 1);
                        assert!(model.power(*endpoint).await.unwrap());
                    }
                    let before = [3, 4, 9].map(|siid| peer.count("get", "multi", siid));
                    for (siid, value) in [(3, false), (4, true), (9, false)] {
                        peer.value(
                            "multi",
                            crate::xiaomi::capability::PropertyBinding { siid, piid: 1 },
                            json!(value),
                        );
                    }
                    peer.notify("multi", 2, true);
                    until(|| {
                        [3, 4, 9]
                            .iter()
                            .enumerate()
                            .all(|(index, siid)| peer.count("get", "multi", *siid) > before[index])
                    })
                    .await;
                    until(|| {
                        ids.iter().zip([false, true, false]).all(|(id, value)| {
                            function(&control.context, "multi", id).snapshot().power()
                                == Some(if value { Power::On } else { Power::Off })
                        })
                    })
                    .await;
                    for (endpoint, expected) in endpoints.into_iter().zip([false, true, false]) {
                        assert_eq!(model.power(endpoint).await.unwrap(), expected);
                    }
                    assert_eq!(peer.count("rpcReq", "multi", 2), 0);
                })
                .await;
            }));
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn single_power_receipt_preserves_correlated_acceptance_from_an_invalid_batch() {
    use crate::xiaomi::adapter::StepReceipt;
    block_on(Rig::new(true, &["one"]).run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        let control = &c;
        c.peer(peer, |peer| async move {
            initialized(control, &peer, &["one"]).await;
            peer.state.borrow_mut().hold_writes = true;
            let outlet = function(&control.context, "one", "outlet:2");
            let mut action = Box::pin(outlet.execute(Command::On));
            assert!(poll_once(action.as_mut()).await.is_none());
            until(|| peer.count("rpcReq", "one", 2) == 1).await;
            peer.release(
                "one",
                2,
                json!({"result":[
                    {"did":"one","siid":2,"piid":1,"code":0},
                    {"did":"other","siid":2,"piid":1,"code":0}
                ]}),
            )
            .await;
            assert!(matches!(
                action.await,
                Err(crate::device::CommandError::SendFailed(_))
            ));
            assert_eq!(outlet.snapshot().power(), Some(Power::Off));
            let status = control.context.diagnostics();
            let last = status.devices[0].commands[0].last.as_ref().unwrap();
            assert_eq!(last.actual_path, Some(PathKind::Local));
            assert_eq!(last.steps.len(), 1);
            assert_eq!(last.steps[0].receipt, StepReceipt::Accepted);
            assert_eq!(peer.count("rpcReq", "one", 2), 1);
        })
        .await;
    }));
}
