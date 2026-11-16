use super::*;

#[test]
fn modes_remain_observable_when_unpublished_and_survive_failed_reads_and_retirement() {
    block_on(rig().run(|c| async move {
        c.announce();
        let peer = panel_peer(RelayMode::Wireless);
        let control = &c;
        c.peer(peer, |peer| async move {
            let relay = function(&control.context.registry, "one", "relay:2");
            let work = control.context.directory.borrow().devices["one"]
                .admitted
                .clone()
                .unwrap();
            until(|| {
                relay.snapshot().relay_mode() == Some(RelayMode::Wireless)
                    && work.function_observer("relay:2").is_some()
                    && work.source_binding_ready(
                        &work.function_observer("relay:2").unwrap(),
                        binding(2, 2),
                    )
            })
            .await;
            let mut changes = relay.subscribe();
            let source_signal = control.context.configuration_changed.listen();
            peer.notify_value("one", binding(2, 2), json!(0), false);
            source_signal.await;
            assert_eq!(changes.changed().await.relay_mode(), Some(RelayMode::Wired));
            assert_eq!(relay.snapshot().power(), Some(Power::Off));

            peer.state
                .borrow_mut()
                .read_fail
                .insert(("one".into(), 2, 2));
            work.refresh_property("relay:2", binding(2, 2));
            until(|| {
                work.refresh
                    .borrow()
                    .property_problem("relay:2", binding(2, 2))
                    .is_some()
            })
            .await;
            assert_eq!(relay.snapshot().relay_mode(), Some(RelayMode::Wired));

            peer.state
                .borrow_mut()
                .read_fail
                .remove(&("one".into(), 2, 2));
            peer.value("one", binding(2, 2), json!(1));
            control.clock.advance(Duration::from_secs(61));
            until(|| relay.snapshot().relay_mode() == Some(RelayMode::Wireless)).await;

            let old_source = work.function_observer("relay:2").unwrap();
            force_unavailable(&control.context, "one", "access revoked");
            control.context.property_value_event(
                &old_source,
                "one",
                binding(2, 2),
                &json!(0),
                None,
                false,
            );
            control.context.property_value_event(
                &old_source,
                "one",
                binding(2, 1),
                &json!(true),
                None,
                false,
            );
            assert_eq!(relay.snapshot().relay_mode(), Some(RelayMode::Wireless));
            assert_eq!(relay.snapshot().power(), Some(Power::Off));
            assert!(relay.execute(Command::On).await.is_err());
            assert_eq!(peer.count("rpcReq", "one", 2), 0);
        })
        .await;
    }));
}

#[test]
fn relay_power_subscriptions_are_independent_when_another_channel_is_rejected() {
    block_on(rig().run(|c| async move {
        c.announce();
        let peer = panel_peer(RelayMode::Wired);
        peer.state
            .borrow_mut()
            .sub_reject
            .insert(("one".into(), 2, 1));
        let control = &c;
        c.peer(peer, |peer| async move {
            let relay = function(&control.context.registry, "one", "relay:3");
            let other = function(&control.context.registry, "one", "relay:4");
            let work = control.context.directory.borrow().devices["one"]
                .admitted
                .clone()
                .unwrap();
            until(|| {
                work.function_observer("relay:3").is_some()
                    && relay.snapshot().power() == Some(Power::Off)
                    && other.snapshot().power() == Some(Power::Off)
            })
            .await;
            peer.notify_value("one", binding(3, 1), json!(true), false);
            until(|| relay.snapshot().power() == Some(Power::On)).await;
            assert!(work.function_observer("relay:2").is_none());
            assert_eq!(other.snapshot().power(), Some(Power::Off));
        })
        .await;
    }));
}

#[test]
fn mode_notification_stays_live_when_every_power_subscription_is_rejected() {
    block_on(rig().run(|c| async move {
        c.announce();
        let peer = panel_peer(RelayMode::Wireless);
        for siid in [2, 3, 4] {
            peer.state
                .borrow_mut()
                .sub_reject
                .insert(("one".into(), siid, 1));
        }
        let control = &c;
        c.peer(peer, |peer| async move {
            let relay = function(&control.context.registry, "one", "relay:2");
            let work = control.context.directory.borrow().devices["one"]
                .admitted
                .clone()
                .unwrap();
            until(|| {
                work.test_local_exhausted()
                    && work.test_local_subscribed_contains(binding(2, 2))
                    && relay.snapshot().relay_mode() == Some(RelayMode::Wireless)
            })
            .await;
            assert!(work.function_observer("relay:2").is_none());
            peer.notify_value("one", binding(2, 2), json!(0), false);
            until(|| relay.snapshot().relay_mode() == Some(RelayMode::Wired)).await;
            assert_eq!(relay.snapshot().power(), Some(Power::Off));
        })
        .await;
    }));
}

#[test]
fn rejected_mode_subscription_does_not_block_power_feedback() {
    block_on(rig().run(|c| async move {
        c.announce();
        let peer = panel_peer(RelayMode::Wired);
        peer.state
            .borrow_mut()
            .sub_reject
            .insert(("one".into(), 3, 2));
        let control = &c;
        c.peer(peer, |peer| async move {
            let relay = function(&control.context.registry, "one", "relay:3");
            let work = control.context.directory.borrow().devices["one"]
                .admitted
                .clone()
                .unwrap();
            until(|| {
                work.test_local_exhausted()
                    && work.function_observer("relay:3").is_some()
                    && relay.snapshot().power() == Some(Power::Off)
            })
            .await;
            let status = crate::terminal::AuthStatus::new(
                control.context.diagnostics().auth.clone(),
                "/bin/migate".into(),
                "/data".into(),
            );
            let terminal =
                crate::terminal::bridge::Context::committed(control.publication.clone(), &status);
            let details = terminal
                .handle_line("status one", 0)
                .await
                .unwrap()
                .unwrap();
            let mode = details
                .lines()
                .find(|line| line.contains("Mode: property 3.2"))
                .unwrap();
            assert!(mode.contains("hub subscription not ready"), "{mode}");
            let power = details
                .lines()
                .find(|line| line.contains("Power: property 3.1"))
                .unwrap();
            assert!(power.contains("hub subscription ready"), "{power}");
            peer.notify_value("one", binding(3, 1), json!(true), false);
            until(|| relay.snapshot().power() == Some(Power::On)).await;
        })
        .await;
    }));
}

#[test]
fn retained_and_stale_modes_do_not_replace_confirmed_feedback() {
    block_on(rig().run(|c| async move {
        c.announce();
        let peer = panel_peer(RelayMode::Wireless);
        peer.state
            .borrow_mut()
            .read_timeout
            .insert(("one".into(), 2, 2));
        let control = &c;
        c.peer(peer, |peer| async move {
            let relay = function(&control.context.registry, "one", "relay:2");
            let work = control.context.directory.borrow().devices["one"]
                .admitted
                .clone()
                .unwrap();
            until(|| {
                work.function_observer("relay:2").is_some()
                    && work.source_binding_ready(
                        &work.function_observer("relay:2").unwrap(),
                        binding(2, 2),
                    )
                    && peer.state.borrow().held.iter().any(|(request, _)| {
                        let value = decode_mips(&request.payload).unwrap().payload;
                        value["siid"] == 2 && value["piid"] == 2
                    })
            })
            .await;
            let source = work.function_observer("relay:2").unwrap();
            control.context.property_value_event(
                &source,
                "one",
                binding(2, 2),
                &json!(0),
                None,
                true,
            );
            assert_eq!(relay.snapshot().relay_mode(), None);
            control.context.property_value_event(
                &source,
                "one",
                binding(2, 2),
                &json!(0),
                None,
                false,
            );
            assert_eq!(relay.snapshot().relay_mode(), Some(RelayMode::Wired));
            control.context.property_value_event(
                &source,
                "one",
                binding(2, 2),
                &json!(true),
                None,
                false,
            );
            control.context.property_value_event(
                &source,
                "one",
                binding(2, 2),
                &json!(9),
                None,
                false,
            );
            assert_eq!(relay.snapshot().relay_mode(), Some(RelayMode::Wired));
            peer.release_property("one", binding(2, 2), json!({"value":1}))
                .await;
            until(|| !work.refresh.borrow().contains("relay:2", binding(2, 2))).await;
            assert_eq!(relay.snapshot().relay_mode(), Some(RelayMode::Wired));
        })
        .await;
    }));
}

#[test]
fn mode_without_notify_is_polled_but_never_treated_as_a_power_notification() {
    let mut instance: Value = serde_json::from_str(include_str!(
        "../../../../../tests/fixtures/miot/zimi-dhkg05.json"
    ))
    .unwrap();
    let left = instance["services"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|service| service["iid"] == 2)
        .unwrap();
    left["properties"][1]["access"] = json!(["read", "write"]);
    let rig = Rig::profile_light("zimi.switch.dhkg05", instance);
    block_on(rig.run(|c| async move {
        c.announce();
        let peer = panel_peer(RelayMode::Wireless);
        let control = &c;
        c.peer(peer, |peer| async move {
            let relay = function(&control.context.registry, "one", "relay:2");
            let work = control.context.directory.borrow().devices["one"]
                .admitted
                .clone()
                .unwrap();
            until(|| {
                relay.snapshot().relay_mode() == Some(RelayMode::Wireless)
                    && relay.snapshot().power() == Some(Power::Off)
                    && work.function_observer("relay:2").is_some()
            })
            .await;
            assert!(!work.test_local_subscribed_contains(binding(2, 2)));
            let source = work.function_observer("relay:2").unwrap();
            control.context.property_value_event(
                &source,
                "one",
                binding(2, 2),
                &json!(true),
                None,
                false,
            );
            control.context.property_value_event(
                &source,
                "one",
                binding(2, 2),
                &json!(0),
                None,
                false,
            );
            assert_eq!(relay.snapshot().power(), Some(Power::Off));
            assert_eq!(relay.snapshot().relay_mode(), Some(RelayMode::Wireless));
            peer.value("one", binding(2, 2), json!(0));
            control.clock.advance(Duration::from_secs(61));
            until(|| relay.snapshot().relay_mode() == Some(RelayMode::Wired)).await;
        })
        .await;
    }));
}

#[test]
fn relay_mode_is_reread_when_its_usable_path_changes_without_device_unavailability() {
    let (rig, listener, cloud_peer) = with_cloud_rig(rig());
    {
        let mut cloud = rig.cloud.lock().unwrap();
        for siid in [2, 3, 4] {
            cloud
                .raw_values
                .insert(("one".into(), siid, 1), json!(false));
            cloud.raw_values.insert(("one".into(), siid, 2), json!(0));
        }
    }
    block_on(rig.run(|c| async move {
        future::or(cloud_peer.run(&listener), async {
            c.announce();
            let peer = panel_peer(RelayMode::Wireless);
            let control = &c;
            c.peer(peer, |_| async move {
                let relay = function(&control.context.registry, "one", "relay:2");
                let work = control.context.directory.borrow().devices["one"]
                    .admitted
                    .clone()
                    .unwrap();
                until(|| {
                    relay.snapshot().relay_mode() == Some(RelayMode::Wireless)
                        && work.test_observer_is_local()
                        && relay.snapshot().availability == Availability::Available
                })
                .await;
                let handle = control
                    .context
                    .hub
                    .borrow()
                    .session()
                    .unwrap()
                    .handle
                    .clone();
                local_device_offline(&control.context, &handle, "one");
                assert_eq!(relay.snapshot().availability, Availability::Available);
                until(|| relay.snapshot().relay_mode() == Some(RelayMode::Wired)).await;
                assert!(
                    control
                        .cloud
                        .lock()
                        .unwrap()
                        .property_reads
                        .iter()
                        .any(|(did, siid, piid)| did == "one" && *siid == 2 && *piid == 2)
                );
            })
            .await;
        })
        .await;
    }));
}

#[test]
fn relay_power_read_reports_its_real_source_in_terminal_status() {
    let rig = rig();
    let status = Rc::new(crate::terminal::AuthStatus::new(
        rig.source.context.diagnostics().auth.clone(),
        "/bin/migate".into(),
        "/data".into(),
    ));
    let report = status.clone();
    block_on(rig.run_with_status(
        move |next| report.update_adapter(next),
        move |c| async move {
            c.announce();
            let peer = panel_peer(RelayMode::Wired);
            let control = &c;
            c.peer(peer, |_| async move {
                until(|| {
                    let live = control.context.directory.borrow();
                    let work = live.devices["one"].admitted.as_ref().unwrap();
                    let relay = work.physical().function("relay:3").unwrap();
                    relay.snapshot().power() == Some(Power::Off)
                        && work.function_observer("relay:3").is_some()
                        && work.property_source("relay:3", binding(3, 2)) == Some(PathKind::Local)
                })
                .await;
                async_io::Timer::after(Duration::from_millis(40)).await;
                let terminal = crate::terminal::bridge::Context::committed(
                    control.publication.clone(),
                    &status,
                );
                let details = terminal
                    .handle_line("status one", 0)
                    .await
                    .unwrap()
                    .unwrap();
                let relay = details
                    .split("  relay:3 (")
                    .nth(1)
                    .unwrap()
                    .split("  relay:4 (")
                    .next()
                    .unwrap();
                assert!(
                    relay.contains("Power: property 3.1; observation source hub"),
                    "{relay}"
                );
                assert!(
                    relay.contains("notification source hub; observation source hub"),
                    "{relay}"
                );
            })
            .await;
        },
    ));
}

#[test]
fn relay_power_live_notification_records_source_without_accepting_retained_or_invalid_as_fresh() {
    let rig = rig();
    let status = Rc::new(crate::terminal::AuthStatus::new(
        rig.source.context.diagnostics().auth.clone(),
        "/bin/migate".into(),
        "/data".into(),
    ));
    let report = status.clone();
    block_on(rig.run_with_status(
        move |next| report.update_adapter(next),
        move |c| async move {
            c.announce();
            let peer = panel_peer(RelayMode::Wired);
            peer.state
                .borrow_mut()
                .read_timeout
                .insert(("one".into(), 2, 1));
            let control = &c;
            c.peer(peer, |peer| async move {
                let work = control.context.directory.borrow().devices["one"]
                    .admitted
                    .clone()
                    .unwrap();
                let relay = work.physical().function("relay:2").unwrap();
                until(|| {
                    relay.snapshot().relay_mode() == Some(RelayMode::Wired)
                        && work.function_observer("relay:2").is_some()
                })
                .await;
                assert_eq!(relay.snapshot().power(), Some(Power::Unknown));
                assert_eq!(work.observation_source("relay:2"), None);
                assert_eq!(work.property_source("relay:2", binding(2, 1)), None);
                let source = work.function_observer("relay:2").unwrap();
                control.context.property_value_event(
                    &source,
                    "one",
                    binding(2, 1),
                    &json!(true),
                    None,
                    true,
                );
                assert_eq!(relay.snapshot().power(), Some(Power::On));
                assert_eq!(work.property_source("relay:2", binding(2, 1)), None);
                control.context.property_value_event(
                    &source,
                    "one",
                    binding(2, 1),
                    &json!("bad"),
                    None,
                    false,
                );
                assert_eq!(work.observation_source("relay:2"), None);
                let stale = work.observations.read_stamp_input(
                    "relay:2",
                    crate::xiaomi::capability::ReadInput::Property(binding(2, 1)),
                );
                peer.notify_value("one", binding(2, 1), json!(false), false);
                until(|| relay.snapshot().power() == Some(Power::Off)).await;
                work.observe_read_at(
                    "relay:2",
                    binding(2, 1),
                    &stale,
                    &json!(true),
                    Observation {
                        source: PathKind::Cloud,
                        source_time: None,
                        received_at: 0,
                    },
                )
                .unwrap();
                assert_eq!(relay.snapshot().power(), Some(Power::Off));
                assert_eq!(
                    work.property_source("relay:2", binding(2, 1)),
                    Some(PathKind::Local)
                );
                async_io::Timer::after(Duration::from_millis(40)).await;
                let terminal = crate::terminal::bridge::Context::committed(
                    control.publication.clone(),
                    &status,
                );
                let details = terminal
                    .handle_line("status one", 0)
                    .await
                    .unwrap()
                    .unwrap();
                let section = details
                    .split("  relay:2 (")
                    .nth(1)
                    .unwrap()
                    .split("  relay:3 (")
                    .next()
                    .unwrap();
                assert!(
                    section.contains("Power: property 2.1; observation source hub"),
                    "{section}"
                );
                assert!(
                    section.contains("notification source hub; observation source hub"),
                    "{section}"
                );
                control.context.property_value_event(
                    &source,
                    "one",
                    binding(2, 1),
                    &json!(true),
                    None,
                    true,
                );
                assert_eq!(relay.snapshot().power(), Some(Power::Off));
                assert_eq!(work.observation_source("relay:2"), Some(PathKind::Local));
            })
            .await;
        },
    ));
}
