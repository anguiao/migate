//! Independent read and notification source lifetimes.
use super::*;

#[test]
fn native_retired_motion_source_cannot_deliver_late_events_or_property_values() {
    let (rig, listener, cloud) = cloud_rig(motion_rig());
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            cloud_ready(&c, "motion").await;
            cloud_event(&cloud, Some(321));
            until(|| state(&c, "motion").source_time == Some(321)).await;
            let observed = state(&c, "motion");
            work(&c).retire();
            cloud_event(&cloud, Some(999));
            cloud.property_at("one", 2, 1024, json!(120));
            for _ in 0..50 {
                future::yield_now().await;
            }
            assert_eq!(state(&c, "motion"), observed);
        })
        .await;
    }));
}

#[test]
fn native_state_read_path_switches_refresh_without_rebuilding_valid_notifications() {
    let (rig, listener, cloud) = cloud_rig(motion_rig());
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            c.announce();
            let peer = motion_peer();
            peer.state.borrow_mut().pushless.insert("one".into());
            let control = &c;
            let cloud = &cloud;
            c.peer(peer, |peer| async move {
                cloud_ready(control, "motion").await;
                until(|| {
                    state(control, "motion").active == Some(false)
                        && !work(control).refresh.borrow().contains("motion", DURATION)
                })
                .await;
                let event_subs = cloud.count("device/one/up/event_occured/2/1008");
                let property_subs = cloud.count("device/one/up/properties_changed/2/1024");
                let before = cloud_reads(control, DURATION);
                peer.state.borrow_mut().online.insert("one".into(), false);
                peer.list_changed(&["one"]);
                until(|| {
                    cloud_reads(control, DURATION) > before
                        && work(control).observation_source("motion") == Some(PathKind::Cloud)
                })
                .await;
                peer.value("one", DURATION, json!(0));
                peer.state.borrow_mut().online.insert("one".into(), true);
                peer.list_changed(&["one"]);
                until(|| state(control, "motion").active == Some(true)).await;
                assert_eq!(
                    work(control).observation_source("motion"),
                    Some(PathKind::Local)
                );
                assert_eq!(
                    cloud.count("device/one/up/event_occured/2/1008"),
                    event_subs
                );
                assert_eq!(
                    cloud.count("device/one/up/properties_changed/2/1024"),
                    property_subs
                );
                assert!(matches!(
                    work(control).function_observer("motion"),
                    Some(NotificationSource::Cloud(_))
                ));
            })
            .await;
        })
        .await;
    }));
}

#[test]
fn native_state_disconnect_retains_observation_and_reconnect_reads_without_changing_identity() {
    block_on(contact_rig().run(|c| async move {
        c.announce();
        let saved = Rc::new(RefCell::new(None));
        let collected = saved.clone();
        let control = &c;
        let peer = Rc::new(Peer::default());
        peer.value("one", CONTACT, json!(true));
        peer.state
            .borrow_mut()
            .update_times
            .insert(("one".into(), 2, 2), 401);
        c.peer(peer, |_peer| async move {
            until(|| state(control, "contact").source_time == Some(401)).await;
            settled(control, "contact", CONTACT).await;
            *collected.borrow_mut() = Some(state(control, "contact"));
        })
        .await;
        until(|| c.context.live.borrow().session.is_none()).await;
        let observed = saved.borrow().clone().unwrap();
        assert_eq!(state(&c, "contact"), observed);
        assert_eq!(
            work(&c).observation_source("contact"),
            Some(PathKind::Local)
        );
        assert!(work(&c).function_observer("contact").is_none());
        let before = c.publication.current().configuration.clone();
        let peer = Rc::new(Peer::default());
        peer.value("one", CONTACT, json!(false));
        peer.state
            .borrow_mut()
            .update_times
            .insert(("one".into(), 2, 2), 402);
        let control = &c;
        c.peer(peer, |_peer| async move {
            until(|| state(control, "contact").source_time == Some(402)).await;
            assert_eq!(state(control, "contact").active, Some(true));
            ready(control, "contact").await;
        })
        .await;
        assert_eq!(c.publication.current().configuration, before);
    }));
}

#[test]
fn native_state_source_switch_keeps_observation_source_until_a_new_valid_input() {
    let (rig, listener, cloud) = cloud_rig(motion_rig());
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            c.announce();
            let control = &c;
            let cloud = &cloud;
            c.peer(motion_peer(), |peer| async move {
                settled(control, "motion", DURATION).await;
                event(&peer, false, Some(411));
                until(|| state(control, "motion").source_time == Some(411)).await;
                let observed = state(control, "motion");
                peer.state
                    .borrow_mut()
                    .read_fail
                    .insert(("one".into(), 2, 1024));
                peer.state.borrow_mut().pushless.insert("one".into());
                peer.list_changed(&["one"]);
                cloud_ready(control, "motion").await;
                assert_eq!(state(control, "motion"), observed);
                assert_eq!(
                    work(control).observation_source("motion"),
                    Some(PathKind::Local)
                );
                event(&peer, false, Some(999));
                peer.notify_value_at("one", DURATION, json!(0), Some(999), false);
                cloud.property_at("one", 2, 1005, json!(12.0));
                until(|| illumination(control) == Some(12.0)).await;
                assert_eq!(state(control, "motion"), observed);
                cloud.property_at("one", 2, 1024, json!(120));
                until(|| state(control, "motion").input == Some(StateInput::Property(json!(120))))
                    .await;
                assert_eq!(state(control, "motion").active, Some(false));
                assert_eq!(
                    work(control).observation_source("motion"),
                    Some(PathKind::Cloud)
                );
            })
            .await;
        })
        .await;
    }));
}

#[test]
fn native_state_mixed_proxy_does_not_borrow_control_eligibility_or_claim_physical_online() {
    let mut instance: Value = serde_json::from_str(include_str!(
        "../../../../../tests/fixtures/miot/xiaomi-light-ceil04.json"
    ))
    .unwrap();
    let contact: Value = serde_json::from_str(include_str!(
        "../../../../../tests/fixtures/miot/isa-dw2hl.json"
    ))
    .unwrap();
    let mut service = contact["services"]
        .as_array()
        .unwrap()
        .iter()
        .find(|service| {
            service["type"]
                .as_str()
                .unwrap()
                .contains(":magnet-sensor:")
        })
        .unwrap()
        .clone();
    service["iid"] = json!(42);
    instance["services"].as_array_mut().unwrap().push(service);
    let rig = Rig::custom_lights(&[("proxy.state", "xiaomi.light.ceil04", instance)]);
    rig.cloud
        .lock()
        .unwrap()
        .raw_values
        .insert(("proxy.state".into(), 42, 2), json!(false));
    block_on(rig.run(|c| async move {
        until(|| {
            function(&c.context.registry, "proxy.state", "contact")
                .snapshot()
                .state_sensor()
                .unwrap()
                .active
                == Some(true)
        })
        .await;
        let status = c
            .context
            .diagnostics()
            .devices
            .into_iter()
            .find(|d| d.did == "proxy.state")
            .unwrap();
        assert_eq!(status.selected_control, None);
        assert_eq!(status.physical_online, None);
        assert_eq!(status.unavailable_reason, None);
        assert_eq!(
            status
                .functions
                .iter()
                .find(|f| f.function == "contact")
                .unwrap()
                .selected_read,
            Some(PathKind::Cloud)
        );
        assert_eq!(
            status
                .functions
                .iter()
                .find(|f| f.function == "main")
                .unwrap()
                .selected_read,
            None
        );
        assert_eq!(
            c.context
                .registry
                .get(&DeviceKey::new("xiaomi", "proxy.state"))
                .unwrap()
                .availability(),
            Availability::Available
        );
    }));
}
