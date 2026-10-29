//! Native state values and independent observation metadata.
use super::*;

#[test]
fn native_state_initial_reads_and_local_notifications_are_read_only() {
    block_on(contact_rig().run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        peer.value("one", CONTACT, json!(true));
        peer.state
            .borrow_mut()
            .update_times
            .insert(("one".into(), 2, 2), 11);
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| state(control, "contact:2:2").active == Some(false)).await;
            ready(control, "contact:2:2").await;
            assert_eq!(state(control, "contact:2:2").source_time, Some(11));
            peer.notify_value_at("one", CONTACT, json!(false), Some(12), false);
            until(|| state(control, "contact:2:2").active == Some(true)).await;
            assert_eq!(
                state(control, "contact:2:2").input,
                Some(StateInput::Property(json!(false)))
            );
            assert_eq!(state(control, "contact:2:2").source_time, Some(12));
            assert_eq!(
                function(&control.context.registry, "one", "contact:2:2")
                    .execute(Command::Off)
                    .await,
                Err(crate::device::CommandError::Unsupported)
            );
            assert_eq!(peer.count("rpcReq", "one", 0), 0);
            assert_eq!(
                control.context.diagnostics().devices[0].selected_control,
                None
            );
        })
        .await;
    }));
}

#[test]
fn native_motion_events_and_duration_share_state_without_polling_or_event_illuminance() {
    block_on(motion_rig().run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(motion_peer(), |peer| async move {
            until(|| {
                state(control, "motion:2:1024").active == Some(false)
                    && illumination(control) == Some(7.5)
            })
            .await;
            settled(control, "motion:2:1024", DURATION).await;
            event(&peer, true, Some(50));
            peer.notify_value("one", ILLUMINANCE, json!(8.0), false);
            until(|| illumination(control) == Some(8.0)).await;
            assert_eq!(state(control, "motion:2:1024").active, Some(false));
            event(&peer, false, Some(51));
            until(|| state(control, "motion:2:1024").active == Some(true)).await;
            assert_eq!(
                state(control, "motion:2:1024").input,
                Some(StateInput::Event {
                    siid: 2,
                    eiid: 1008
                })
            );
            assert_eq!(state(control, "motion:2:1024").source_time, Some(51));
            assert_eq!(illumination(control), Some(8.0));
            event(&peer, false, None);
            until(|| state(control, "motion:2:1024").source_time.is_none()).await;
            peer.notify_value_at("one", DURATION, json!(0), Some(52), false);
            until(|| state(control, "motion:2:1024").input == Some(StateInput::Property(json!(0))))
                .await;
            assert_eq!(state(control, "motion:2:1024").active, Some(true));
            for value in [120, 300] {
                peer.notify_value_at("one", DURATION, json!(value), Some(value), false);
                until(|| {
                    state(control, "motion:2:1024").input
                        == Some(StateInput::Property(json!(value)))
                })
                .await;
                assert_eq!(state(control, "motion:2:1024").active, Some(false));
            }
            let before = reads(&peer, DURATION);
            control.clock.advance(Duration::from_secs(600));
            for _ in 0..50 {
                future::yield_now().await;
            }
            assert_eq!(reads(&peer, DURATION), before);
            assert_eq!(state(control, "motion:2:1024").active, Some(false));
        })
        .await;
    }));
}

#[test]
fn native_unknown_is_observed_while_read_failures_and_source_loss_keep_the_last_observation() {
    block_on(occupancy_rig().run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        peer.value("one", OCCUPANCY, json!(1));
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| state(control, "occupancy:2:1").active == Some(true)).await;
            settled(control, "occupancy:2:1", OCCUPANCY).await;
            peer.notify_value_at("one", OCCUPANCY, json!(2), Some(71), false);
            until(|| state(control, "occupancy:2:1").active.is_none()).await;
            let unknown = state(control, "occupancy:2:1");
            assert_eq!(unknown.input, Some(StateInput::Property(json!(2))));
            peer.state
                .borrow_mut()
                .read_fail
                .insert(("one".into(), 2, 1));
            work(control).refresh_property("occupancy:2:1", OCCUPANCY);
            until(|| {
                work(control)
                    .refresh
                    .borrow()
                    .problem_for("occupancy:2:1")
                    .is_some()
            })
            .await;
            assert_eq!(state(control, "occupancy:2:1"), unknown);
            peer.notify_value_at("one", OCCUPANCY, json!(0), Some(72), false);
            until(|| state(control, "occupancy:2:1").active == Some(false)).await;
            let known = state(control, "occupancy:2:1");
            peer.state.borrow_mut().pushless.insert("one".into());
            peer.list_changed(&["one"]);
            until(|| work(control).function_observer("occupancy:2:1").is_none()).await;
            assert_eq!(state(control, "occupancy:2:1"), known);
            assert_eq!(
                work(control).physical().availability(),
                Availability::Available
            );
            assert_eq!(
                work(control).observation_source("occupancy:2:1"),
                Some(PathKind::Local)
            );
        })
        .await;
    }));
}

#[test]
fn native_state_first_read_failure_stays_unknown_without_blocking_notification_recovery() {
    block_on(contact_rig().run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        peer.state
            .borrow_mut()
            .read_fail
            .insert(("one".into(), 2, 2));
        let control = &c;
        c.peer(peer, |peer| async move {
            ready(control, "contact:2:2").await;
            until(|| {
                work(control)
                    .refresh
                    .borrow()
                    .problem_for("contact:2:2")
                    .is_some()
            })
            .await;
            assert_eq!(state(control, "contact:2:2").active, None);
            assert_eq!(state(control, "contact:2:2").input, None);
            peer.notify_value("one", CONTACT, json!(false), false);
            until(|| state(control, "contact:2:2").active == Some(true)).await;
            let known = state(control, "contact:2:2");
            work(control).refresh_property("contact:2:2", CONTACT);
            until(|| {
                !work(control)
                    .refresh
                    .borrow()
                    .contains("contact:2:2", CONTACT)
            })
            .await;
            assert_eq!(state(control, "contact:2:2"), known);
        })
        .await;
    }));
}

#[test]
fn native_motion_cloud_events_and_property_snapshots_have_independent_inputs() {
    let (rig, listener, cloud) = cloud_rig(motion_rig());
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            until(|| state(&c, "motion:2:1024").active == Some(false)).await;
            cloud_ready(&c, "motion:2:1024").await;
            cloud_event(&cloud, Some(301));
            until(|| state(&c, "motion:2:1024").source_time == Some(301)).await;
            assert_eq!(state(&c, "motion:2:1024").active, Some(true));
            assert_eq!(
                work(&c).observation_source("motion:2:1024"),
                Some(PathKind::Cloud)
            );
            cloud.property_at("one", 2, 1024, json!(120));
            until(|| state(&c, "motion:2:1024").input == Some(StateInput::Property(json!(120))))
                .await;
            assert_eq!(state(&c, "motion:2:1024").active, Some(false));
            assert_eq!(state(&c, "motion:2:1024").source_time, None);
            assert!(c.cloud.lock().unwrap().property_writes.is_empty());
        })
        .await;
    }));
}
