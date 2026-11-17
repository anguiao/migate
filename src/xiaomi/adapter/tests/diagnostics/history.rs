use super::*;

#[test]
fn unavailable_first_admission_has_a_baseline_and_readmission_drops_the_old_history() {
    let rig = Rig::new(true, &["one"]);
    assert_eq!(
        rig.source.context.diagnostics().devices[0].selected_control,
        None
    );
    assert_eq!(
        rig.source.context.diagnostics().devices[0].last_switch_reason,
        None
    );
    rig.cloud.lock().unwrap().online.insert("one".into());
    block_on(rig.run(|c| async move {
        until(|| c.context.cloud_candidate("one", true).is_ok()).await;
        assert_eq!(
            recorded(&c, "one").as_deref(),
            Some("complete cloud directory and device information synchronized")
        );
        c.clock
            .automatic
            .store(false, std::sync::atomic::Ordering::Relaxed);
        c.cloud.lock().unwrap().property_status = 503;
        assert!(
            function(&c.context.registry, "one", "outlet:2")
                .execute(Command::On)
                .await
                .is_err()
        );
        let previous = recorded(&c, "one").unwrap();
        assert!(previous.contains("Mijia HTTP request failed"));
        c.cloud.lock().unwrap().members.clear();
        c.context.request_sync();
        c.clock.advance(Duration::from_secs(1));
        until(|| !c.context.directory.borrow().devices.contains_key("one")).await;
        c.cloud.lock().unwrap().members.push("one".into());
        c.context.request_sync();
        until(|| {
            c.context
                .directory
                .borrow()
                .devices
                .get("one")
                .is_some_and(|slot| slot.work.is_some())
                && c.context.cloud_candidate("one", true).is_ok()
        })
        .await;
        assert_ne!(recorded(&c, "one").as_deref(), Some(previous.as_str()));
        assert_eq!(
            recorded(&c, "one").as_deref(),
            Some("complete cloud directory and device information synchronized")
        );
    }));
}

#[test]
fn one_device_access_event_preserves_other_devices_and_the_global_priority_reason() {
    let rig = Rig::new(true, &["one", "two"]);
    rig.cloud
        .lock()
        .unwrap()
        .online
        .extend(["one".into(), "two".into()]);
    block_on(rig.run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(Rc::new(Peer::default()), |peer| async move {
            initialized(control, &peer, &["one", "two"]).await;
            until(|| {
                control.context.directory.borrow().scope_valid()
                    && control.context.active_notification("one") == Some(PathKind::Local)
                    && control.context.active_notification("two") == Some(PathKind::Local)
            })
            .await;
            control
                .clock
                .automatic
                .store(false, std::sync::atomic::Ordering::Relaxed);
            let other = recorded(control, "two");
            let global = control.context.diagnostics().routing.last_switch_reason;
            let handle = control
                .context
                .hub
                .borrow()
                .session()
                .unwrap()
                .handle
                .clone();
            // Another access fact has not been dispatched yet. A one-DID event
            // must not consume that device's pending transition.
            control.context.hub.borrow_mut().device_offline("two");
            local_device_offline(&control.context, &handle, "one");
            assert_eq!(
                recorded(control, "one").as_deref(),
                Some("hub reported device one offline or missing")
            );
            assert_eq!(recorded(control, "two"), other);
            let status = control.context.diagnostics();
            assert_eq!(status.routing.last_switch_reason, global);
            assert_eq!(recorded(control, "two"), other);
        })
        .await;
    }));
}

#[test]
fn repeated_directory_and_property_feedback_keep_the_last_real_switch_reason() {
    let rig = Rig::new(true, &["one"]);
    rig.cloud.lock().unwrap().online.insert("one".into());
    block_on(rig.run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(Rc::new(Peer::default()), |peer| async move {
            initialized(control, &peer, &["one"]).await;
            until(|| {
                control.context.directory.borrow().scope_valid()
                    && control.context.active_notification("one") == Some(PathKind::Local)
            })
            .await;
            let reason = recorded(control, "one");
            let count = control.cloud.lock().unwrap().infos;
            control.context.request_sync();
            until(|| control.cloud.lock().unwrap().infos > count).await;
            peer.notify("one", 2, true);
            until(|| {
                function(&control.context.registry, "one", "outlet:2")
                    .snapshot()
                    .power()
                    == Some(Power::On)
            })
            .await;
            assert_eq!(recorded(control, "one"), reason);
            assert_eq!(
                control.context.diagnostics().devices[0].last_switch_reason,
                reason
            );
        })
        .await;
    }));
}

#[test]
fn member_online_change_records_the_dependent_virtual_group_only() {
    let (rig, listener, cloud) = with_cloud_rig(repair_rig(0, true));
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            repair_ready(&c).await;
            until(|| c.context.active_notification("group-1") == Some(PathKind::Cloud)).await;
            let other = recorded(&c, "member-b");
            cloud.online("member-a", false);
            until(|| c.context.active_notification("group-1").is_none()).await;
            assert_eq!(
                recorded(&c, "member-a").as_deref(),
                Some("cloud online conditions changed")
            );
            assert_eq!(
                recorded(&c, "group-1").as_deref(),
                Some("cloud online conditions changed")
            );
            assert_eq!(recorded(&c, "member-b"), other);
            let group = c
                .context
                .diagnostics()
                .devices
                .into_iter()
                .find(|device| device.did == "group-1")
                .unwrap();
            assert_eq!(group.selected_control, Some(PathKind::Cloud));
            assert_eq!(group.notification_source, None);
        })
        .await;
    }));
}

#[test]
fn activating_another_function_records_a_handover_even_when_the_device_summary_is_unchanged() {
    let (rig, listener, cloud) = with_cloud_sensors(&["sensor"]);
    let held = "device/sensor/up/properties_changed/3/1008";
    cloud.state.borrow_mut().hold.insert(held.into());
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            until(|| {
                cloud_active(&c, "sensor")
                    && cloud
                        .state
                        .borrow()
                        .held
                        .iter()
                        .any(|subscription| subscription.filters[0].path == held)
            })
            .await;
            let work = c.context.directory.borrow().devices["sensor"]
                .work
                .clone()
                .unwrap();
            assert!(work.function_observer("humidity:3:1008").is_none());
            // A completed HTTP failure provides an independently visible reason
            // while already active MQTT subscriptions remain valid.
            c.cloud.lock().unwrap().read_status = 401;
            work.refresh_property(
                "temperature:3:1001",
                crate::xiaomi::capability::PropertyBinding {
                    siid: 3,
                    piid: 1001,
                },
            );
            until(|| {
                recorded(&c, "sensor")
                    .as_deref()
                    .is_some_and(|reason| reason.contains("Mijia HTTP request failed"))
            })
            .await;
            let before = c.context.diagnostics().devices[0].notification_source;
            cloud.state.borrow_mut().hold.remove(held);
            cloud.release(held);
            until(|| work.function_observer("humidity:3:1008").is_some()).await;
            assert_eq!(
                c.context.diagnostics().devices[0].notification_source,
                before
            );
            assert_eq!(
                recorded(&c, "sensor").as_deref(),
                Some("notification subscriptions ready; source activated")
            );
        })
        .await;
    }));
}

fn mixed_group_rig() -> Rig {
    let mut light = ls2307_instance();
    let mut humidity = sensor_native("sensor").raw.instance["services"]
        .as_array()
        .unwrap()
        .iter()
        .find(|service| service["iid"] == 3)
        .unwrap()
        .clone();
    humidity["iid"] = json!(30);
    humidity["properties"]
        .as_array_mut()
        .unwrap()
        .retain(|property| property["iid"] == 1008);
    light["services"].as_array_mut().unwrap().push(humidity);
    let rig = Rig::custom_lights(&[
        ("member-a", "devcea.light.ls2307", light),
        ("member-b", "devcea.light.ls2307", ls2307_instance()),
    ]);
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.catalog_groups = vec![group_record(
            "group-record",
            "group-1",
            &["member-a", "member-b"],
            false,
        )];
        cloud.catalog_instances.insert(
            group_instance()["type"].as_str().unwrap().into(),
            group_instance(),
        );
        cloud.online.insert("group-1".into());
        for did in ["member-a", "member-b", "group-1"] {
            for (siid, piid, value) in [
                (2, 1, json!(true)),
                (2, 2, json!(50)),
                (2, 3, json!(2700)),
                (4, 1, json!(0x112233)),
                (4, 2, json!(0)),
            ] {
                cloud.raw_values.insert((did.into(), siid, piid), value);
            }
        }
        cloud
            .raw_values
            .insert(("member-a".into(), 30, 1008), json!(40.0));
    }
    rig
}

#[test]
fn non_member_function_handover_preserves_the_light_groups_reason() {
    let rig = mixed_group_rig();
    let (rig, listener, cloud) = with_cloud_rig(rig);
    let held = "device/member-a/up/properties_changed/30/1008";
    cloud.state.borrow_mut().hold.insert(held.into());
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            until(|| {
                c.context.active_notification("group-1") == Some(PathKind::Cloud)
                    && cloud
                        .state
                        .borrow()
                        .held
                        .iter()
                        .any(|subscription| subscription.filters[0].path == held)
            })
            .await;
            let member = c.context.directory.borrow().devices["member-a"]
                .work
                .clone()
                .unwrap();
            assert!(member.function_observer("humidity:30:1008").is_none());
            c.cloud.lock().unwrap().property_status = 503;
            assert!(
                function(&c.context.registry, "member-b", "light:2")
                    .execute(Command::Off)
                    .await
                    .is_err()
            );
            c.cloud.lock().unwrap().property_status = 200;
            until(|| {
                recorded(&c, "group-1").as_deref()
                    == Some("complete cloud directory and device information synchronized")
            })
            .await;
            let previous = recorded(&c, "group-1");
            cloud.state.borrow_mut().hold.remove(held);
            cloud.release(held);
            until(|| member.function_observer("humidity:30:1008").is_some()).await;
            assert_eq!(
                recorded(&c, "member-a").as_deref(),
                Some("notification subscriptions ready; source activated")
            );
            assert_eq!(recorded(&c, "group-1"), previous);
            assert_eq!(
                c.context.active_notification("group-1"),
                Some(PathKind::Cloud)
            );
        })
        .await;
    }));
}

#[test]
fn actual_member_light_handover_records_the_group_even_when_its_summary_stays_cloud() {
    let (rig, listener, cloud) = with_cloud_rig(mixed_group_rig());
    let held = "device/member-a/up/properties_changed/4/1";
    cloud.state.borrow_mut().hold.insert(held.into());
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            c.announce();
            let peer = Rc::new(Peer::default());
            peer.state.borrow_mut().bad.insert("member-b".into());
            let control = &c;
            let cloud = &cloud;
            c.peer(peer, |_peer| async move {
                until(|| {
                    control.context.active_notification("member-a") == Some(PathKind::Local)
                        && control.context.active_notification("group-1") == Some(PathKind::Cloud)
                })
                .await;
                control.cloud.lock().unwrap().property_status = 503;
                assert!(
                    function(&control.context.registry, "member-b", "light:2")
                        .execute(Command::Off)
                        .await
                        .is_err()
                );
                control.cloud.lock().unwrap().property_status = 200;
                until(|| {
                    recorded(control, "group-1").as_deref()
                        == Some("complete cloud directory and device information synchronized")
                })
                .await;
                control
                    .clock
                    .automatic
                    .store(false, std::sync::atomic::Ordering::Relaxed);
                let previous = recorded(control, "group-1");
                control.context.hub.borrow_mut().degrade();
                control
                    .context
                    .update_routing("hub directory priority changed");
                control.context.changed.notify(usize::MAX);
                until(|| {
                    cloud
                        .state
                        .borrow()
                        .held
                        .iter()
                        .any(|subscription| subscription.filters[0].path == held)
                })
                .await;
                assert_eq!(
                    control.context.active_notification("group-1"),
                    Some(PathKind::Cloud)
                );
                assert_eq!(recorded(control, "group-1"), previous);
                cloud.state.borrow_mut().hold.remove(held);
                cloud.release(held);
                until(|| {
                    control.context.directory.borrow().devices["member-a"]
                        .work
                        .as_ref()
                        .unwrap()
                        .function_observer("light:2")
                        .is_some_and(|source| source.kind() == PathKind::Cloud)
                })
                .await;
                assert_eq!(
                    control.context.active_notification("group-1"),
                    Some(PathKind::Cloud)
                );
                assert_eq!(
                    recorded(control, "group-1").as_deref(),
                    Some("notification subscriptions ready; source activated")
                );
                let reason = recorded(control, "group-1");
                control
                    .context
                    .update_device_routing("member-a", "duplicate access conditions");
                assert_eq!(recorded(control, "group-1"), reason);
            })
            .await;
        })
        .await;
    }));
}

#[test]
fn revocation_clears_the_serving_history_before_any_replacement_commit() {
    let rig = Rig::new(true, &["one"]);
    let context = &rig.source.context;
    assert!(
        context.directory.borrow().devices["one"]
            .routing_history
            .as_ref()
            .map(|history| history.selection)
            .is_some()
    );
    context.revoke("one", "confirmed removal").unwrap();
    assert!(
        context.directory.borrow().devices["one"]
            .routing_history
            .as_ref()
            .map(|history| history.selection)
            .is_none()
    );
    context.update_device_routing("one", "late source event");
    assert!(
        context.directory.borrow().devices["one"]
            .routing_history
            .as_ref()
            .map(|history| history.selection)
            .is_none()
    );
}
