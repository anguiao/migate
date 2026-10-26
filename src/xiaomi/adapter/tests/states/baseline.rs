//! Complete notification readiness and post-subscription baselines.
use super::*;

#[test]
fn native_motion_needs_both_subscriptions_but_keeps_illuminance_and_read_snapshots() {
    block_on(motion_rig().run(|c| async move {
        c.announce();
        let peer = motion_peer();
        peer.state
            .borrow_mut()
            .sub_reject
            .insert(("one".into(), 2, 1008));
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| {
                state(control, "motion").active == Some(false) && illumination(control) == Some(7.5)
            })
            .await;
            until(|| {
                work(control)
                    .function_subscription_problem("motion")
                    .is_some()
            })
            .await;
            ready(control, "illuminance").await;
            assert!(work(control).function_observer("motion").is_none());
            let statuses = work(control).subscription_status("motion");
            assert!(statuses.iter().any(|s| s.binding
                == SubscriptionBinding::Event(MOTION)
                && s.problem.is_some()));
            event(&peer, false, Some(80));
            peer.notify_value("one", ILLUMINANCE, json!(10.0), false);
            until(|| illumination(control) == Some(10.0)).await;
            assert_eq!(state(control, "motion").active, Some(false));
            peer.state.borrow_mut().sub_reject.clear();
            peer.list_changed(&["one"]);
            ready(control, "motion").await;
            event(&peer, false, Some(81));
            until(|| state(control, "motion").active == Some(true)).await;
        })
        .await;
    }));
}

#[test]
fn native_motion_readiness_does_not_wait_for_optional_battery_subscription() {
    let mut rig = motion_rig();
    rig.source.request_timeout = Duration::from_secs(2);
    block_on(rig.run(|c| async move {
        c.announce();
        let peer = motion_peer();
        peer.state
            .borrow_mut()
            .sub_timeout
            .insert(("one".into(), 3, 1003));
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| !peer.state.borrow().held_subs.is_empty()).await;
            assert!(
                work(control)
                    .subscription_status("motion")
                    .iter()
                    .any(|s| s.source == PathKind::Local
                        && s.binding == SubscriptionBinding::Event(MOTION)
                        && s.ready),
                "motion event must be subscribed before optional battery work"
            );
            ready(control, "motion").await;
            assert_eq!(
                peer.state.borrow().held_subs.len(),
                1,
                "motion should be ready while the first battery subscription is held"
            );
            event(&peer, false, Some(90));
            until(|| state(control, "motion").active == Some(true)).await;
        })
        .await;
    }));
}

#[test]
fn native_state_ready_after_initial_read_requires_a_new_read_baseline() {
    block_on(held_contact_rig().run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(held_contact_peer(), |peer| async move {
            until(|| {
                state(control, "contact").active == Some(false)
                    && !peer.state.borrow().held_subs.is_empty()
            })
            .await;
            until(|| !work(control).refresh.borrow().contains("contact", CONTACT)).await;
            let before = reads(&peer, CONTACT);
            peer.value("one", CONTACT, json!(false));
            accept_contact(&peer);
            until(|| state(control, "contact").active == Some(true)).await;
            assert!(reads(&peer, CONTACT) > before);
            ready(control, "contact").await;
        })
        .await;
    }));
}

#[test]
fn native_state_notification_in_the_suback_batch_establishes_the_baseline_without_an_extra_read() {
    block_on(held_contact_rig().run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(held_contact_peer(), |peer| async move {
            until(|| {
                state(control, "contact").active == Some(false)
                    && !peer.state.borrow().held_subs.is_empty()
            })
            .await;
            until(|| !work(control).refresh.borrow().contains("contact", CONTACT)).await;
            let before = reads(&peer, CONTACT);
            accept_contact(&peer);
            peer.notify_value_at("one", CONTACT, json!(false), Some(101), false);
            until(|| state(control, "contact").source_time == Some(101)).await;
            ready(control, "contact").await;
            for _ in 0..50 {
                future::yield_now().await;
            }
            assert_eq!(reads(&peer, CONTACT), before);
            assert_eq!(state(control, "contact").active, Some(true));
        })
        .await;
    }));
}

#[test]
fn native_cloud_unknown_notification_in_the_suback_batch_is_a_complete_baseline() {
    let (rig, listener, cloud) = cloud_rig(occupancy_rig());
    rig.cloud
        .lock()
        .unwrap()
        .raw_values
        .insert(("one".into(), 2, 1), json!(1));
    let topic = "device/one/up/properties_changed/2/1";
    cloud.state.borrow_mut().hold.insert(topic.into());
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            until(|| {
                state(&c, "occupancy").active == Some(true)
                    && cloud
                        .state
                        .borrow()
                        .held
                        .iter()
                        .any(|sub| sub.filters[0].path == topic)
            })
            .await;
            until(|| !work(&c).refresh.borrow().contains("occupancy", OCCUPANCY)).await;
            let before = cloud_reads(&c, OCCUPANCY);
            cloud.release(topic);
            cloud.property_at("one", 2, 1, json!(2));
            until(|| state(&c, "occupancy").input == Some(StateInput::Property(json!(2)))).await;
            cloud_ready(&c, "occupancy").await;
            for _ in 0..50 {
                future::yield_now().await;
            }
            assert_eq!(state(&c, "occupancy").active, None);
            assert_eq!(cloud_reads(&c, OCCUPANCY), before);
        })
        .await;
    }));
}
