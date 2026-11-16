use super::*;

#[test]
fn v13_direct_admission_confirms_a_real_boolean_and_reads_standard_baseline() {
    let (rig, peer) = setup();
    block_on(rig.run(|c| async move {
        let blow = function(&c, BLOW);
        let heater = function(&c, HEATER);
        eventually(|| {
            c.context.direct_candidate(DID).is_ok()
                && blow
                    .snapshot()
                    .fan()
                    .is_some_and(|fan| fan.power.value == Some(true))
                && heater
                    .snapshot()
                    .heater()
                    .is_some_and(|heater| heater.power.value == Some(false))
                && c.context.diagnostics().devices.iter().any(|device| {
                    device.did == DID
                        && device.selected_control == Some(PathKind::Direct)
                        && device.selected_read == Some(PathKind::Direct)
                })
        })
        .await;

        let requests = peer.requests.lock().unwrap().clone();
        let info = requests
            .iter()
            .position(|request| request["method"] == "miIO.info")
            .expect("direct admission must confirm the device model");
        let probe = requests
            .get(info + 1)
            .expect("a read-only business confirmation must follow miIO.info");
        assert_eq!(probe["method"], "get_properties", "{requests:?}");
        let item = &probe["params"][0];
        assert_eq!(item["did"], DID);
        let binding = (
            item["siid"].as_u64().unwrap() as u32,
            item["piid"].as_u64().unwrap() as u32,
        );
        assert!(
            matches!(binding, (2, 1) | (3, 2) | (3, 3) | (3, 4)),
            "admission must confirm a real boolean property from the source plan: {requests:?}"
        );

        assert_eq!(
            work(&c).property_source(BLOW, BLOW_POWER),
            Some(PathKind::Direct),
            "standard properties use the authenticated LAN connection"
        );
        assert_eq!(peer.properties.lock().unwrap()[&(3, 2)], json!(true));
        assert_eq!(
            work(&c).property_source(HEATER, HEATING),
            Some(PathKind::Direct),
            "the heater switch baseline is independent of the model probe"
        );
        let status = c
            .context
            .diagnostics()
            .devices
            .into_iter()
            .find(|device| device.did == DID)
            .unwrap();
        assert_eq!(status.lan.unwrap().state, LanState::Ready);
    }));
}

#[test]
fn v13_direct_low_speed_sends_only_set_bh_mode_without_a_high_speed_write() {
    let (rig, peer) = setup();
    block_on(rig.run(|c| async move {
        let blow = function(&c, BLOW);
        eventually(|| {
            c.context.direct_candidate(DID).is_ok()
                && blow
                    .snapshot()
                    .fan()
                    .is_some_and(|fan| fan.power.value == Some(true) && fan.speed.value == Some(2))
        })
        .await;

        for (id, mode) in [(BLOW, "coolwind"), (VENTILATION, "venting")] {
            let before = peer.requests.lock().unwrap().len();
            function(&c, id)
                .begin_fan(FanCommand::Speed(1))
                .unwrap()
                .accepted()
                .await
                .unwrap();
            let requests = peer.requests.lock().unwrap();
            let writes = requests[before..]
                .iter()
                .filter(|request| {
                    matches!(
                        request["method"].as_str(),
                        Some("set_bh_mode" | "set_properties")
                    )
                })
                .map(|request| (&request["method"], &request["params"]))
                .collect::<Vec<_>>();
            assert_eq!(writes, [(&json!("set_bh_mode"), &json!([mode, 1]))], "{id}");
        }
        assert!(
            c.cloud.lock().unwrap().action_requests.is_empty(),
            "a selected direct legacy command must not also invoke the cloud action"
        );
    }));
}

#[test]
fn plain_on_preserves_low_speed_before_and_after_power_feedback() {
    let (rig, direct) = setup();
    rig.network.lan_timeout.set(Duration::from_millis(500));
    let (rig, listener, cloud) = with_cloud_rig(rig);
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            let hub = standard_peer();
            for binding in [BLOW_POWER, VENTILATION_POWER] {
                hub.value(DID, binding, json!(false));
            }
            c.announce();
            let control = &c;
            c.peer(hub, |hub| async move {
                eventually(|| {
                    control.context.direct_candidate(DID).is_ok()
                        && work(control).shared_feedback().source == Some(PathKind::Direct)
                        && [BLOW, VENTILATION].iter().all(|id| {
                            work(control).function_observer(id).is_some()
                                && function(control, id).snapshot().fan().unwrap().power.value
                                    == Some(false)
                        })
                })
                .await;
                hub.state.borrow_mut().hold_reads = true;
                for (id, binding, mode) in [
                    (BLOW, BLOW_POWER, "coolwind"),
                    (VENTILATION, VENTILATION_POWER, "venting"),
                ] {
                    let fan = function(control, id);
                    let before = hub_writes(&hub).len();
                    let low_before = direct_methods(&direct, "set_bh_mode").len();
                    fan.begin_fan(FanCommand::Speed(1))
                        .unwrap()
                        .accepted()
                        .await
                        .unwrap();
                    fan.execute(Command::On).await.unwrap();
                    assert_eq!(fan.snapshot().fan().unwrap().power.value, Some(false));
                    assert_eq!(hub_writes(&hub).len(), before);
                    assert_eq!(
                        direct_methods(&direct, "set_bh_mode")[low_before]["params"],
                        json!([mode, 1])
                    );

                    // Some controllers finish the same gesture with another explicit 50%.
                    fan.begin_fan(FanCommand::Speed(1))
                        .unwrap()
                        .accepted()
                        .await
                        .unwrap();
                    assert_eq!(direct_methods(&direct, "set_bh_mode").len(), low_before + 2);
                    hub.notify_value(DID, binding, json!(true), false);
                    eventually(|| fan.snapshot().fan().unwrap().power.value == Some(true)).await;
                    fan.execute(Command::On).await.unwrap();
                    assert_eq!(hub_writes(&hub).len(), before);
                    fan.begin_fan(FanCommand::Speed(2))
                        .unwrap()
                        .accepted()
                        .await
                        .unwrap();
                    hub.notify_value(DID, binding, json!(false), false);
                    eventually(|| fan.snapshot().fan().unwrap().power.value == Some(false)).await;
                    fan.execute(Command::On).await.unwrap();
                    fan.execute(Command::Off).await.unwrap();
                    fan.execute(Command::On).await.unwrap();
                    assert_eq!(
                        hub_writes(&hub)[before..],
                        [
                            (3, binding.piid, json!(true)),
                            (3, binding.piid, json!(true)),
                            (3, binding.piid, json!(false)),
                            (3, binding.piid, json!(true)),
                        ]
                    );
                }
                let before = hub_writes(&hub).len();
                direct.drop_write.store(true, Ordering::Relaxed);
                let blow = function(control, BLOW);
                let low = blow.begin_fan(FanCommand::Speed(1)).unwrap();
                let on = blow.begin_fan(FanCommand::Power(true)).unwrap();
                assert_eq!(low.accepted().await, Err(CommandError::Timeout));
                assert_eq!(on.accepted().await, Err(CommandError::Timeout));
                assert_eq!(hub_writes(&hub).len(), before);
                blow.execute(Command::Off).await.unwrap();
                blow.execute(Command::On).await.unwrap();
                assert_eq!(
                    hub_writes(&hub)[before..],
                    [(3, 2, json!(false)), (3, 2, json!(true))]
                );
            })
            .await;
        })
        .await;
    }));
}

#[test]
fn v13_direct_shared_get_prop_feeds_both_fans_and_the_built_in_temperature() {
    let (rig, peer) = setup();
    block_on(rig.run(|c| async move {
        let blow = function(&c, BLOW);
        let ventilation = function(&c, VENTILATION);
        let heater = function(&c, HEATER);
        eventually(|| {
            c.context.direct_candidate(DID).is_ok()
                && blow
                    .snapshot()
                    .fan()
                    .is_some_and(|fan| fan.speed.value == Some(2))
                && ventilation
                    .snapshot()
                    .fan()
                    .is_some_and(|fan| fan.speed.value == Some(1))
                && heater
                    .snapshot()
                    .heater()
                    .is_some_and(|heater| heater.current_temperature.value == Some(26.5))
        })
        .await;

        let requests = peer.requests.lock().unwrap().clone();
        let shared = requests
            .iter()
            .filter(|request| request["method"] == "get_prop")
            .collect::<Vec<_>>();
        assert!(
            shared
                .iter()
                .any(|request| request["params"]
                    == json!(["bh_mode", "fan_speed_idx", "temperature"])),
            "the shared legacy group must be read over the LAN connection: {requests:?}"
        );
        assert!(
            requests.iter().any(|request| {
                request["method"] == "get_properties"
                    && request["params"].as_array().unwrap().iter().any(|item| {
                        (item["siid"] == json!(3))
                            && [2, 3, 4, 5, 1, 8].contains(&item["piid"].as_u64().unwrap())
                    })
            }),
            "standard properties must still be read independently: {requests:?}"
        );

        let device = work(&c);
        let shared = device.shared_feedback();
        assert_eq!(shared.source, Some(PathKind::Direct));
        assert!(!shared.pending);
        assert_eq!(
            shared
                .temperature
                .as_ref()
                .and_then(|temperature| temperature.value.as_f64()),
            Some(26.5)
        );
        assert!(
            requests
                .iter()
                .any(|request| request["method"] == "get_properties"),
            "a directly confirmed standard property is still required"
        );
        assert_eq!(
            function(&c, HEATER)
                .snapshot()
                .heater()
                .unwrap()
                .current_temperature
                .value,
            Some(26.5)
        );
        assert!(
            c.cloud.lock().unwrap().rpc_reads.is_empty(),
            "the direct shared group must not also use cloud RPC"
        );
    }));
}

#[test]
fn v13_direct_heater_and_light_write_only_their_own_properties() {
    let (rig, peer) = setup();
    block_on(rig.run(|c| async move {
        let heater = function(&c, HEATER);
        let light = function(&c, "light:2");
        eventually(|| {
            c.context.direct_candidate(DID).is_ok()
                && heater
                    .snapshot()
                    .heater()
                    .is_some_and(|heater| heater.power.value == Some(false))
        })
        .await;

        heater
            .begin_heater(crate::device::HeaterCommand::Power(true))
            .unwrap()
            .accepted()
            .await
            .unwrap();
        heater
            .begin_heater(crate::device::HeaterCommand::Power(false))
            .unwrap()
            .accepted()
            .await
            .unwrap();
        light.execute(crate::device::Command::On).await.unwrap();
        light.execute(crate::device::Command::Off).await.unwrap();
        heater
            .begin_heater(crate::device::HeaterCommand::Temperature(30))
            .unwrap()
            .accepted()
            .await
            .unwrap();

        let writes = peer
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request["method"] == "set_properties")
            .flat_map(|request| request["params"].as_array().unwrap().iter().cloned())
            .map(|item| {
                (
                    item["siid"].as_u64().unwrap() as u32,
                    item["piid"].as_u64().unwrap() as u32,
                    item["value"].clone(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            writes,
            vec![
                (3, 3, json!(true)),
                (3, 3, json!(false)),
                (2, 1, json!(true)),
                (2, 1, json!(false)),
                (3, 8, json!(30)),
            ]
        );
        assert!(
            c.cloud.lock().unwrap().numeric_writes.is_empty(),
            "ready direct standard properties must not also write the cloud"
        );
    }));
}
