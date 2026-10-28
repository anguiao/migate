use super::*;

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
