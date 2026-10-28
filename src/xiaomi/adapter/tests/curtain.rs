//! Curtain reports through the existing local and cloud observation paths.
use super::*;
use crate::device::{CurtainMotion, CurtainSnapshot};
use crate::xiaomi::capability::PropertyBinding;

const STATUS: PropertyBinding = PropertyBinding { siid: 2, piid: 2 };
const CURRENT: PropertyBinding = PropertyBinding { siid: 2, piid: 3 };
const TARGET: PropertyBinding = PropertyBinding { siid: 2, piid: 4 };

fn rig() -> Rig {
    Rig::custom_lights(&[(
        "one",
        "xiaomi.curtain.acn010",
        serde_json::from_str(include_str!(
            "../../../../tests/fixtures/miot/xiaomi-acn010.json"
        ))
        .unwrap(),
    )])
}
fn work(c: &Control) -> Rc<super::work::DeviceWork> {
    c.context.live.borrow().devices["one"]
        .admitted
        .as_ref()
        .unwrap()
        .clone()
}
fn curtain(c: &Control) -> CurtainSnapshot {
    function(&c.context.registry, "one", "main")
        .snapshot()
        .curtain()
        .unwrap()
}
fn peer() -> Rc<Peer> {
    let peer = Rc::new(Peer::default());
    peer.value("one", STATUS, json!(0));
    peer.value("one", CURRENT, json!(37));
    peer.value("one", TARGET, json!(80));
    peer
}

#[test]
fn local_curtain_reads_and_subscribes_only_the_three_reported_properties() {
    block_on(rig().run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(peer(), |peer| async move {
            until(|| {
                control
                    .context
                    .live
                    .borrow()
                    .devices
                    .get("one")
                    .is_some_and(|slot| slot.admitted.is_some())
            })
            .await;
            assert_eq!(work(control).channels(), vec!["main"]);
            until(|| {
                let value = curtain(control);
                value.status.value == Some(CurtainMotion::Opening)
                    && value.current_position.value == Some(37)
                    && value.target_position.value == Some(80)
                    && work(control).function_observer("main").is_some()
            })
            .await;
            let subscriptions = work(control).subscription_status("main");
            for binding in [STATUS, CURRENT, TARGET] {
                assert!(
                    subscriptions
                        .iter()
                        .any(|item| item.source == PathKind::Local
                            && item.binding
                                == super::model::SubscriptionBinding::Property(binding)
                            && item.ready)
                );
            }
            assert!(!subscriptions.iter().any(|item| item.binding
                == super::model::SubscriptionBinding::Property(PropertyBinding {
                    siid: 2,
                    piid: 1
                })));
            let reads = peer
                .state
                .borrow()
                .seen
                .iter()
                .filter(|(op, _)| op == "get")
                .map(|(_, value)| value["piid"].as_u64().unwrap())
                .collect::<Vec<_>>();
            assert!(reads.iter().all(|piid| [2, 3, 4].contains(piid)));
            peer.notify_value_at("one", CURRENT, json!(38), Some(90), false);
            until(|| curtain(control).current_position.value == Some(38)).await;
            let snapshot = curtain(control);
            assert_eq!(snapshot.current_position.source_time, Some(90));
            assert_eq!(snapshot.target_position.value, Some(80));
            assert_eq!(snapshot.status.value, Some(CurtainMotion::Opening));
        })
        .await;
    }));
}

#[test]
fn terminal_lists_each_curtain_value_source_and_subscription() {
    block_on(rig().run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(peer(), |peer| async move {
            until(|| {
                curtain(control).current_position.value == Some(37)
                    && curtain(control).target_position.value == Some(80)
                    && curtain(control).status.value == Some(CurtainMotion::Opening)
                    && work(control).function_observer("main").is_some()
            })
            .await;
            peer.notify_value_at("one", CURRENT, json!(38), Some(201), false);
            until(|| curtain(control).current_position.source_time == Some(201)).await;
            let status = crate::terminal::AuthStatus::new(
                control.context.status.borrow().authentication.clone(),
                "/bin/migate".into(),
                "/data".into(),
            );
            status.update_adapter(control.context.diagnostics());
            let terminal =
                crate::terminal::bridge::Context::committed(control.publication.clone(), &status);
            for command in ["list", "status one"] {
                let text = terminal.handle_line(command, 0).await.unwrap().unwrap();
                for expected in [
                    "Current position: 38%",
                    "Target position: 80%",
                    "Status: opening",
                    "upstream time 201",
                    "received at",
                    "observation source local",
                    "property 2.3",
                    "local subscription ready",
                ] {
                    assert!(text.contains(expected), "missing {expected}: {text}");
                }
            }
            for command in ["on one", "off one"] {
                let text = terminal.handle_line(command, 0).await.unwrap().unwrap();
                assert!(
                    text.contains("device does not support this command"),
                    "{text}"
                );
            }
            assert_eq!(peer.count("rpcReq", "one", 0), 0);
        })
        .await;
    }));
}

async fn settled(c: &Control) {
    until(|| {
        let value = curtain(c);
        value.current_position.value.is_some()
            && value.target_position.value.is_some()
            && value.status.value.is_some()
            && work(c).function_observer("main").is_some()
            && [STATUS, CURRENT, TARGET]
                .iter()
                .all(|binding| !work(c).refresh.borrow().contains("main", *binding))
    })
    .await;
}

fn cloud_rig() -> (Rig, Async<TcpListener>, Rc<CloudPeer>) {
    let rig = rig();
    let listener = Async::<TcpListener>::bind(([127, 0, 0, 1], 0)).unwrap();
    rig.source.context.notification_connector.set(
        super::super::notifications::NotificationConnector::Plaintext(
            listener.get_ref().local_addr().unwrap(),
        ),
    );
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.online.insert("one".into());
        for (binding, value) in [(STATUS, 2), (CURRENT, 25), (TARGET, 75)] {
            cloud
                .raw_values
                .insert(("one".into(), binding.siid, binding.piid), json!(value));
        }
    }
    (rig, listener, Rc::new(CloudPeer::default()))
}

#[test]
fn curtain_local_notification_wins_over_old_read_and_failures_keep_observations() {
    block_on(rig().run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(peer(), |peer| async move {
            settled(control).await;
            peer.state
                .borrow_mut()
                .read_timeout
                .insert(("one".into(), 2, 3));
            work(control).refresh_property("main", CURRENT);
            until(|| {
                peer.state.borrow().held.iter().any(|(publish, _)| {
                    let payload = decode_mips(&publish.payload).unwrap().payload;
                    payload["siid"] == 2 && payload["piid"] == 3
                })
            })
            .await;
            peer.notify_value_at("one", CURRENT, json!(64), Some(401), false);
            until(|| curtain(control).current_position.source_time == Some(401)).await;
            peer.release_property("one", CURRENT, json!({"value": 20, "updateTime": 1}))
                .await;
            peer.notify_value_at("one", STATUS, json!(1), Some(402), false);
            until(|| curtain(control).status.value == Some(CurtainMotion::Closing)).await;
            assert_eq!(curtain(control).current_position.value, Some(64));
            assert_eq!(curtain(control).target_position.value, Some(80));

            let function = function(&control.context.registry, "one", "main");
            let revision = function.snapshot().revision;
            peer.notify_value_at("one", CURRENT, json!(64), Some(403), false);
            until(|| curtain(control).current_position.source_time == Some(403)).await;
            assert_eq!(function.snapshot().revision, revision);
            peer.notify_value_at("one", CURRENT, json!(101), Some(404), false);
            peer.notify_value_at("one", TARGET, json!(81), Some(405), false);
            until(|| curtain(control).target_position.value == Some(81)).await;
            assert_eq!(curtain(control).current_position.source_time, Some(403));

            let observed = curtain(control);
            peer.state
                .borrow_mut()
                .read_fail
                .insert(("one".into(), 2, 4));
            work(control).refresh_property("main", TARGET);
            work(control)
                .refresh
                .borrow_mut()
                .exhaust_pending("main", TARGET);
            until(|| {
                work(control)
                    .refresh
                    .borrow()
                    .property_problem("main", TARGET)
                    .is_some()
            })
            .await;
            assert_eq!(curtain(control), observed);
            assert!(
                work(control)
                    .refresh
                    .borrow()
                    .property_problem("main", CURRENT)
                    .is_none()
            );
            let before = peer
                .state
                .borrow()
                .seen
                .iter()
                .filter(|(op, _)| op == "get")
                .count();
            control.clock.advance(Duration::from_secs(600));
            for _ in 0..50 {
                future::yield_now().await;
            }
            assert_eq!(
                peer.state
                    .borrow()
                    .seen
                    .iter()
                    .filter(|(op, _)| op == "get")
                    .count(),
                before
            );
            assert_eq!(curtain(control), observed);
        })
        .await;
    }));
}

#[test]
fn curtain_baselines_are_independent_when_notification_arrives_with_last_suback() {
    let mut rig = rig();
    rig.source.request_timeout = Duration::from_secs(2);
    block_on(rig.run(|c| async move {
        c.announce();
        let peer = peer();
        peer.state
            .borrow_mut()
            .sub_timeout
            .insert(("one".into(), 2, 4));
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| {
                let value = curtain(control);
                value.current_position.value == Some(37)
                    && value.target_position.value == Some(80)
                    && value.status.value == Some(CurtainMotion::Opening)
                    && !peer.state.borrow().held_subs.is_empty()
                    && [STATUS, CURRENT, TARGET]
                        .iter()
                        .all(|binding| !work(control).refresh.borrow().contains("main", *binding))
            })
            .await;
            let reads = |binding: PropertyBinding| {
                peer.state
                    .borrow()
                    .seen
                    .iter()
                    .filter(|(op, value)| {
                        op == "get"
                            && value["siid"] == binding.siid
                            && value["piid"] == binding.piid
                    })
                    .count()
            };
            let before = [reads(STATUS), reads(CURRENT), reads(TARGET)];
            peer.value("one", CURRENT, json!(45));
            peer.value("one", TARGET, json!(95));
            let sub = peer.state.borrow_mut().held_subs.remove(0);
            peer.send(Packet::SubAck(SubAck::new(
                sub.pkid,
                vec![SubscribeReasonCode::QoS2],
            )));
            peer.notify_value_at("one", STATUS, json!(1), Some(500), false);
            until(|| {
                curtain(control).status.source_time == Some(500)
                    && curtain(control).current_position.value == Some(45)
                    && curtain(control).target_position.value == Some(95)
            })
            .await;
            settled(control).await;
            assert_eq!(reads(STATUS), before[0]);
            assert!(reads(CURRENT) > before[1]);
            assert!(reads(TARGET) > before[2]);
        })
        .await;
    }));
}

#[test]
fn curtain_subscription_failure_recovers_without_losing_read_values() {
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
                curtain(control).target_position.value == Some(80)
                    && work(control)
                        .function_subscription_problem("main")
                        .is_some()
            })
            .await;
            assert!(work(control).function_observer("main").is_none());
            let statuses = work(control).subscription_status("main");
            assert!(statuses.iter().any(|item| item.binding
                == super::model::SubscriptionBinding::Property(CURRENT)
                && item.source == PathKind::Local
                && item.problem.is_some()));
            peer.state.borrow_mut().sub_reject.clear();
            peer.list_changed(&["one"]);
            until(|| work(control).function_observer("main").is_some()).await;
            peer.notify_value_at("one", CURRENT, json!(48), Some(600), false);
            until(|| curtain(control).current_position.source_time == Some(600)).await;
            assert_eq!(curtain(control).target_position.value, Some(80));
            assert!(
                work(control)
                    .subscription_status("main")
                    .iter()
                    .any(|item| item.binding
                        == super::model::SubscriptionBinding::Property(CURRENT)
                        && item.source == PathKind::Local
                        && item.ready
                        && item.problem.is_none())
            );
        })
        .await;
    }));
}

#[test]
fn curtain_cloud_reads_and_reports_are_independent_and_old_http_read_cannot_overwrite() {
    let (rig, listener, cloud) = cloud_rig();
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            settled(&c).await;
            let initial = curtain(&c);
            assert_eq!(initial.current_position.value, Some(25));
            assert_eq!(initial.target_position.value, Some(75));
            assert_eq!(initial.status.value, Some(CurtainMotion::Stopped));
            assert_eq!(
                work(&c).property_source("main", CURRENT),
                Some(PathKind::Cloud)
            );
            let reads = c.cloud.lock().unwrap().property_reads.clone();
            assert!(
                reads
                    .iter()
                    .all(|(_, siid, piid)| *siid == 2 && [2, 3, 4].contains(piid))
            );
            let (release, gate) = flume::bounded(1);
            c.cloud
                .lock()
                .unwrap()
                .read_gates
                .insert(("one".into(), 2, 3), gate);
            let before = c.cloud.lock().unwrap().property_reads.len();
            work(&c).refresh_property("main", CURRENT);
            until(|| c.cloud.lock().unwrap().property_reads.len() > before).await;
            cloud.property_at("one", 2, 3, json!(40));
            until(|| curtain(&c).current_position.value == Some(40)).await;
            cloud.property_at("one", 2, 4, json!(90));
            cloud.property_at("one", 2, 2, json!(0));
            until(|| {
                curtain(&c).target_position.value == Some(90)
                    && curtain(&c).status.value == Some(CurtainMotion::Opening)
            })
            .await;
            c.cloud
                .lock()
                .unwrap()
                .read_gates
                .remove(&("one".into(), 2, 3));
            release.send(()).unwrap();
            until(|| !work(&c).refresh.borrow().contains("main", CURRENT)).await;
            assert_eq!(curtain(&c).current_position.value, Some(40));
            for binding in [STATUS, CURRENT, TARGET] {
                assert_eq!(
                    work(&c).property_source("main", binding),
                    Some(PathKind::Cloud)
                );
            }
            assert_eq!(cloud.count("device/one/up/properties_changed/2/1"), 0);
        })
        .await;
    }));
}

#[test]
fn curtain_disconnect_and_retirement_keep_observations_and_recovery_reads_all_properties() {
    block_on(rig().run(|c| async move {
        c.announce();
        let saved = Rc::new(RefCell::new(None));
        let result = saved.clone();
        let old_source = Rc::new(RefCell::new(None));
        let capture_source = old_source.clone();
        let control = &c;
        c.peer(peer(), |_peer| async move {
            settled(control).await;
            *result.borrow_mut() = Some(curtain(control));
            *capture_source.borrow_mut() = work(control).function_observer("main");
        })
        .await;
        until(|| c.context.live.borrow().session.is_none()).await;
        assert_eq!(curtain(&c), saved.borrow().unwrap());
        assert!(work(&c).function_observer("main").is_none());
        let replacement = peer();
        replacement.value("one", STATUS, json!(2));
        replacement.value("one", CURRENT, json!(55));
        replacement.value("one", TARGET, json!(60));
        let control = &c;
        c.peer(replacement, |peer| async move {
            until(|| {
                curtain(control).current_position.value == Some(55)
                    && curtain(control).target_position.value == Some(60)
                    && curtain(control).status.value == Some(CurtainMotion::Stopped)
            })
            .await;
            settled(control).await;
            let observed = curtain(control);
            control.context.property_value_event(
                old_source.borrow().as_ref().unwrap(),
                "one",
                CURRENT,
                &json!(98),
                Some(998),
                false,
            );
            assert_eq!(curtain(control), observed);
            let old = work(control);
            old.retire();
            peer.notify_value_at("one", CURRENT, json!(99), Some(999), false);
            for _ in 0..50 {
                future::yield_now().await;
            }
            assert_eq!(curtain(control), observed);
        })
        .await;
    }));
}

#[test]
fn curtain_notification_and_read_routes_switch_independently() {
    let (rig, listener, cloud) = cloud_rig();
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            c.announce();
            let peer = peer();
            peer.state
                .borrow_mut()
                .sub_reject
                .insert(("one".into(), 2, 3));
            let control = &c;
            let cloud = &cloud;
            c.peer(peer, |peer| async move {
                until(|| {
                    matches!(
                        work(control).function_observer("main"),
                        Some(super::super::notifications::NotificationSource::Cloud(_))
                    ) && [STATUS, CURRENT, TARGET]
                        .iter()
                        .all(|binding| !work(control).refresh.borrow().contains("main", *binding))
                        && curtain(control).current_position.value == Some(37)
                })
                .await;
                let diagnostics = control.context.diagnostics();
                let status = &diagnostics.devices[0].functions[0];
                assert_eq!(status.selected_read, Some(PathKind::Local));
                assert_eq!(status.notification_source, Some(PathKind::Cloud));
                peer.state
                    .borrow_mut()
                    .read_timeout
                    .insert(("one".into(), 2, 3));
                work(control).refresh_property("main", CURRENT);
                until(|| {
                    peer.state.borrow().held.iter().any(|(publish, _)| {
                        let payload = decode_mips(&publish.payload).unwrap().payload;
                        payload["siid"] == 2 && payload["piid"] == 3
                    })
                })
                .await;
                cloud.property_at("one", 2, 3, json!(41));
                until(|| curtain(control).current_position.value == Some(41)).await;
                peer.release_property("one", CURRENT, json!({"value": 10}))
                    .await;
                peer.notify_value_at("one", CURRENT, json!(99), Some(999), false);
                cloud.property_at("one", 2, 2, json!(1));
                until(|| curtain(control).status.value == Some(CurtainMotion::Closing)).await;
                assert_eq!(curtain(control).current_position.value, Some(41));
                assert_eq!(
                    work(control).property_source("main", CURRENT),
                    Some(PathKind::Cloud)
                );

                let subscriptions = [STATUS, CURRENT, TARGET].map(|binding| {
                    cloud.count(&format!(
                        "device/one/up/properties_changed/{}/{}",
                        binding.siid, binding.piid
                    ))
                });
                let before = control.cloud.lock().unwrap().property_reads.len();
                peer.state.borrow_mut().online.insert("one".into(), false);
                peer.list_changed(&["one"]);
                until(|| {
                    control.cloud.lock().unwrap().property_reads.len() >= before + 3
                        && [STATUS, CURRENT, TARGET].iter().all(|binding| {
                            work(control).property_source("main", *binding) == Some(PathKind::Cloud)
                        })
                })
                .await;
                assert_eq!(curtain(control).current_position.value, Some(25));
                assert_eq!(curtain(control).target_position.value, Some(75));
                assert_eq!(curtain(control).status.value, Some(CurtainMotion::Stopped));
                assert_eq!(
                    [STATUS, CURRENT, TARGET].map(|binding| cloud.count(&format!(
                        "device/one/up/properties_changed/{}/{}",
                        binding.siid, binding.piid
                    ))),
                    subscriptions
                );
            })
            .await;
        })
        .await;
    }));
}

#[test]
fn curtain_unknown_and_individual_read_failure_are_visible_and_clear_on_recovery() {
    block_on(rig().run(|c| async move {
        c.announce();
        let peer = peer();
        peer.state
            .borrow_mut()
            .read_fail
            .insert(("one".into(), 2, 4));
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| {
                curtain(control).current_position.value == Some(37)
                    && curtain(control).status.value == Some(CurtainMotion::Opening)
                    && work(control)
                        .refresh
                        .borrow()
                        .property_problem("main", TARGET)
                        .is_some()
            })
            .await;
            assert_eq!(curtain(control).target_position.value, None);
            let status = crate::terminal::AuthStatus::new(
                control.context.status.borrow().authentication.clone(),
                "/bin/migate".into(),
                "/data".into(),
            );
            status.update_adapter(control.context.diagnostics());
            let terminal =
                crate::terminal::bridge::Context::committed(control.publication.clone(), &status);
            for command in ["list", "status one"] {
                let text = terminal.handle_line(command, 0).await.unwrap().unwrap();
                let target = text
                    .lines()
                    .find(|line| line.contains("Target position: unknown"))
                    .unwrap();
                for expected in [
                    "upstream time unknown",
                    "received at unknown",
                    "observation source unknown",
                    "property 2.4",
                    "read: one read 2.4",
                ] {
                    assert!(target.contains(expected), "missing {expected}: {text}");
                }
            }
            peer.state.borrow_mut().read_fail.clear();
            work(control).refresh_property("main", TARGET);
            until(|| {
                curtain(control).target_position.value == Some(80)
                    && work(control)
                        .refresh
                        .borrow()
                        .property_problem("main", TARGET)
                        .is_none()
            })
            .await;
        })
        .await;
    }));
}

#[test]
fn curtain_local_commands_write_one_quantized_property_without_refresh() {
    use crate::device::CurtainCommand;
    let mut description: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/xiaomi-acn010.json"
    ))
    .unwrap();
    description["services"][1]["properties"][3]["value-range"] = json!([0, 100, 5]);
    let rig = Rig::custom_lights(&[("one", "xiaomi.curtain.acn010", description)]);
    block_on(rig.run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(peer(), |peer| async move {
            settled(control).await;
            let function = function(&control.context.registry, "one", "main");
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
            let status = crate::terminal::AuthStatus::new(
                control.context.status.borrow().authentication.clone(),
                "/bin/migate".into(),
                "/data".into(),
            );
            status.update_adapter(control.context.diagnostics());
            let terminal =
                crate::terminal::bridge::Context::committed(control.publication.clone(), &status);
            for line in ["list", "status one"] {
                let output = terminal.handle_line(line, 0).await.unwrap().unwrap();
                assert!(
                    output
                        .contains("target 27.4%; properties 2.4=25; actual path local; accepted."),
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
                vec![(PropertyBinding { siid: 2, piid: 1 }, json!(2))]
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
            let function = function(&c.context.registry, "one", "main");
            let before = function.snapshot();
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
            assert_eq!(function.snapshot(), before);
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
            assert_eq!(last.properties, vec![(TARGET, json!(33))]);
            assert_eq!(last.actual_path, Some(PathKind::Cloud));
            assert_eq!(last.result, Some(Ok(())));
        })
        .await;
    }));
}

#[test]
fn curtain_local_stop_waits_for_old_receipt_and_other_device_is_independent() {
    use crate::device::{CommandError, CurtainCommand};
    let description: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/xiaomi-acn010.json"
    ))
    .unwrap();
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
                function(&control.context.registry, "two", "main")
                    .snapshot()
                    .availability
                    == Availability::Available
            })
            .await;
            peer.state.borrow_mut().hold_writes = true;
            let one = function(&control.context.registry, "one", "main");
            let two = function(&control.context.registry, "two", "main");
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
            let status = crate::terminal::AuthStatus::new(
                control.context.status.borrow().authentication.clone(),
                "/bin/migate".into(),
                "/data".into(),
            );
            status.update_adapter(control.context.diagnostics());
            let terminal =
                crate::terminal::bridge::Context::committed(control.publication.clone(), &status);
            for line in ["list", "status one"] {
                let output = terminal.handle_line(line, 0).await.unwrap().unwrap();
                assert!(
                    output.contains("queued curtain commands: stop, 90%."),
                    "{output}"
                );
                assert!(
                    output.contains(
                        "target 10%; properties 2.4=10; actual path local; awaiting result."
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
fn curtain_rejection_and_timeout_are_reported_without_replay() {
    use crate::device::{CommandError, CurtainCommand};
    block_on(rig().run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(peer(), |peer| async move {
            settled(control).await;
            let function = function(&control.context.registry, "one", "main");
            let before = function.snapshot().curtain();
            peer.state.borrow_mut().hold_writes = true;
            let rejected = function
                .begin_curtain(CurtainCommand::Target(11.0))
                .unwrap();
            peer.release_property(
                "one",
                TARGET,
                json!({"result":[{"did":"one","siid":2,"piid":4,"code":-704}]}),
            )
            .await;
            assert!(matches!(
                rejected.accepted().await,
                Err(CommandError::Rejected(_))
            ));
            let timed = function.begin_curtain(CurtainCommand::Stop).unwrap();
            until(|| peer.count("rpcReq", "one", 0) == 2).await;
            assert_eq!(timed.accepted().await, Err(CommandError::Timeout));
            for _ in 0..30 {
                future::yield_now().await;
            }
            assert_eq!(peer.count("rpcReq", "one", 0), 2);
            assert_eq!(control.cloud.lock().unwrap().numeric_writes.len(), 0);
            assert_eq!(function.snapshot().curtain(), before);
            let last = control.context.diagnostics().devices[0].commands[0]
                .last
                .clone()
                .unwrap();
            assert_eq!(last.result, Some(Err(CommandError::Timeout)));
            assert_eq!(last.actual_path, Some(PathKind::Local));
        })
        .await;
    }));
}

#[test]
fn retired_curtain_cancels_queued_work_and_old_receipt_cannot_overtake_replacement() {
    use crate::device::{CommandError, CurtainCommand};
    let mut rig = rig();
    rig.source.request_timeout = Duration::from_secs(2);
    block_on(rig.run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(peer(), |peer| async move {
            settled(control).await;
            peer.state.borrow_mut().hold_writes = true;
            let old_device = control
                .context
                .registry
                .get(&DeviceKey::new("xiaomi", "one"))
                .unwrap();
            let old = old_device.function("main").unwrap();
            let sent = old.begin_curtain(CurtainCommand::Target(15.0)).unwrap();
            until(|| peer.count("rpcReq", "one", 0) == 1).await;
            let stale = old.begin_curtain(CurtainCommand::Stop).unwrap();
            let mut changed: Value = serde_json::from_str(include_str!(
                "../../../../tests/fixtures/miot/xiaomi-acn010.json"
            ))
            .unwrap();
            changed["services"][1]["properties"][3]["value-range"] = json!([0, 100, 5]);
            let parsed = crate::xiaomi::capability::parse_device_for_model(
                DeviceKey::new("xiaomi", "one"),
                "xiaomi.curtain.acn010",
                &changed,
            )
            .unwrap();
            let native = crate::xiaomi::capability::NativeDescription::new(
                "xiaomi.curtain.acn010".into(),
                changed["type"].as_str().unwrap().into(),
                parsed,
            )
            .unwrap();
            control
                .context
                .live
                .borrow_mut()
                .devices
                .get_mut("one")
                .unwrap()
                .latest = Some(native);
            control.context.reconcile("one").unwrap();
            let replacement = loop {
                let current = control
                    .context
                    .registry
                    .get(&DeviceKey::new("xiaomi", "one"))
                    .unwrap();
                if !old_device.same_instance(&current) {
                    break current;
                }
                future::yield_now().await;
            };
            let current = replacement.function("main").unwrap();
            until(|| current.snapshot().availability == Availability::Available).await;
            assert!(matches!(
                stale.accepted().await,
                Err(CommandError::Cancelled) | Err(CommandError::Unavailable(_))
            ));
            let next = current.begin_curtain(CurtainCommand::Target(77.0)).unwrap();
            for _ in 0..30 {
                future::yield_now().await;
            }
            assert_eq!(peer.count("rpcReq", "one", 0), 1);
            peer.release_property(
                "one",
                TARGET,
                json!({"result":[{"did":"one","siid":2,"piid":4,"code":0}]}),
            )
            .await;
            sent.accepted().await.unwrap();
            until(|| peer.count("rpcReq", "one", 0) == 2).await;
            let actual = peer
                .state
                .borrow()
                .seen
                .iter()
                .rfind(|(op, value)| op == "rpcReq" && value["did"] == "one")
                .unwrap()
                .1["rpc"]["params"][0]
                .clone();
            assert_eq!(actual["piid"], 4);
            assert_eq!(actual["value"], 75);
            let observed = current.snapshot().curtain();
            peer.release_property(
                "one",
                TARGET,
                json!({"result":[{"did":"one","siid":2,"piid":4,"code":0}]}),
            )
            .await;
            next.accepted().await.unwrap();
            assert_eq!(current.snapshot().curtain(), observed);
            assert_eq!(peer.count("rpcReq", "one", 0), 2);
        })
        .await;
    }));
}

#[test]
fn curtain_cloud_failure_and_timeout_do_not_retry_or_infer_motion() {
    use crate::device::{CommandError, CurtainCommand};
    let (rig, listener, cloud) = cloud_rig();
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            settled(&c).await;
            let function = function(&c.context.registry, "one", "main");
            let before = function.snapshot().curtain();
            c.cloud.lock().unwrap().property_code = -704;
            let rejected = function
                .begin_curtain(CurtainCommand::Target(44.0))
                .unwrap();
            assert!(matches!(
                rejected.accepted().await,
                Err(CommandError::Rejected(_))
            ));
            let (_release, gate) = flume::bounded(1);
            {
                let mut cloud = c.cloud.lock().unwrap();
                cloud.property_code = 0;
                cloud.write_gates.insert("one".into(), gate);
            }
            let timed = function.begin_curtain(CurtainCommand::Stop).unwrap();
            assert_eq!(timed.accepted().await, Err(CommandError::Timeout));
            for _ in 0..30 {
                future::yield_now().await;
            }
            assert_eq!(c.cloud.lock().unwrap().numeric_writes.len(), 2);
            assert_eq!(function.snapshot().curtain(), before);
            let last = c.context.diagnostics().devices[0].commands[0]
                .last
                .clone()
                .unwrap();
            assert_eq!(last.actual_path, Some(PathKind::Cloud));
            assert_eq!(last.result, Some(Err(CommandError::Timeout)));
        })
        .await;
    }));
}
