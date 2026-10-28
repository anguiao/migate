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
