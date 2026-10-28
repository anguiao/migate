use super::*;

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
                        Some(NotificationSource::Cloud(_))
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
