use super::*;

#[test]
fn hub_standard_properties_coexist_with_direct_shared_feedback_and_low_speed() {
    let (rig, direct) = setup();
    let (rig, listener, cloud) = with_cloud_rig(rig);
    rig.cloud.lock().unwrap().home_status = 503;
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            c.announce();
            let control = &c;
            c.peer(standard_peer(), |hub| async move {
                let blow = function(control, BLOW);
                let heater = function(control, HEATER);
                eventually(|| {
                    control.context.direct_candidate(DID).is_ok()
                        && work(control).function_observer(HEATER).is_some()
                        && work(control)
                            .shared_feedback()
                            .source
                            .is_some_and(|source| source == PathKind::Direct)
                        && blow
                            .snapshot()
                            .fan()
                            .is_some_and(|fan| fan.power.value == Some(true))
                })
                .await;
                control.cloud.lock().unwrap().home_status = 200;
                control.context.request_sync();
                eventually(|| {
                    control
                        .context
                        .cloud_observation_candidate(DID, true)
                        .is_ok()
                })
                .await;

                heater
                    .begin_heater(crate::device::HeaterCommand::Power(true))
                    .unwrap()
                    .accepted()
                    .await
                    .unwrap();
                eventually(|| hub_writes(&hub).contains(&(3, 3, json!(true)))).await;
                assert!(
                    direct_methods(&direct, "set_properties")
                        .iter()
                        .all(|request| request["params"][0]["siid"] != json!(3)
                            || request["params"][0]["piid"] != json!(3)),
                    "the heating property stays on the hub path when it is ready"
                );
                assert!(control.cloud.lock().unwrap().numeric_writes.is_empty());

                blow.begin_fan(FanCommand::Speed(1))
                    .unwrap()
                    .accepted()
                    .await
                    .unwrap();
                eventually(|| {
                    direct_methods(&direct, "set_bh_mode")
                        .iter()
                        .any(|request| request["params"] == json!(["coolwind", 1]))
                })
                .await;
                assert!(
                    control.cloud.lock().unwrap().action_requests.is_empty(),
                    "low speed must not also use the cloud action"
                );
                assert!(
                    direct_methods(&direct, "get_prop").iter().any(|request| {
                        request["params"] == json!(["bh_mode", "fan_speed_idx", "temperature"])
                    }),
                    "the shared group keeps its direct path independently of the hub"
                );
                assert!(control.cloud.lock().unwrap().rpc_reads.is_empty());
                let blow_command = control
                    .context
                    .diagnostics()
                    .devices
                    .into_iter()
                    .find(|device| device.did == DID)
                    .unwrap()
                    .commands
                    .into_iter()
                    .find(|command| command.channel == BLOW)
                    .unwrap()
                    .last
                    .unwrap();
                assert_eq!(blow_command.actual_path, Some(PathKind::Direct));
                assert_eq!(
                    heater
                        .snapshot()
                        .heater()
                        .unwrap()
                        .current_temperature
                        .value,
                    Some(26.5)
                );
                let status = control
                    .context
                    .diagnostics()
                    .devices
                    .into_iter()
                    .find(|device| device.did == DID)
                    .unwrap();
                assert_eq!(status.selected_control, Some(PathKind::Local));
                assert_eq!(
                    status
                        .commands
                        .iter()
                        .find(|command| command.channel == HEATER)
                        .and_then(|command| command.last.as_ref())
                        .and_then(|command| command.actual_path),
                    Some(PathKind::Local)
                );
            })
            .await;
        })
        .await;
    }));
}

#[test]
fn an_unsent_direct_low_speed_start_can_use_the_cloud() {
    let (rig, direct) = setup();
    rig.network.lan_timeout.set(Duration::from_millis(200));
    let (rig, listener, cloud) = with_cloud_rig(rig);
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            let blow = function(&c, BLOW);
            eventually(|| {
                c.context.direct_candidate(DID).is_ok()
                    && blow.snapshot().fan().unwrap().speed.value == Some(2)
                    && work(&c).refresh.borrow().snapshot().inflight.is_empty()
            })
            .await;
            c.clock.automatic.store(false, Ordering::Relaxed);
            let client = c.context.direct_candidate(DID).unwrap();
            let requests = direct.requests.clone();
            drop(direct);
            // Losing an unrelated reply expires the client's timestamp. The next
            // operation must handshake before it can send its business datagram.
            assert!(
                Box::pin(client.request_with_timeout(
                    "get_prop",
                    json!(["temperature"]),
                    Duration::from_millis(50)
                ))
                .await
                .is_err()
            );
            assert!(c.context.direct_candidate(DID).is_ok());
            blow.begin_fan(FanCommand::Speed(1))
                .unwrap()
                .accepted()
                .await
                .unwrap();
            assert!(c.context.direct_candidate(DID).is_err());
            assert!(
                requests
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|r| r["method"] != "set_bh_mode")
            );
            assert_eq!(
                c.cloud.lock().unwrap().action_requests.as_slice(),
                &[(DID.into(), 4, 4, json!([]))]
            );
        })
        .await;
    }));
}

#[test]
fn sent_direct_low_speed_timeout_stays_uncertain_and_is_not_replayed_on_cloud() {
    let (rig, direct) = setup();
    rig.network.lan_timeout.set(Duration::from_millis(100));
    let (rig, listener, cloud) = with_cloud_rig(rig);
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            let blow = function(&c, BLOW);
            eventually(|| {
                c.context.direct_candidate(DID).is_ok()
                    && blow
                        .snapshot()
                        .fan()
                        .is_some_and(|fan| fan.speed.value == Some(2))
            })
            .await;
            direct.drop_write.store(true, Ordering::Relaxed);
            assert!(
                blow.begin_fan(FanCommand::Speed(1))
                    .unwrap()
                    .accepted()
                    .await
                    .is_err()
            );
            eventually(|| c.context.direct_candidate(DID).is_err()).await;
            assert_eq!(direct_methods(&direct, "set_bh_mode").len(), 1);
            assert!(
                c.cloud.lock().unwrap().action_requests.is_empty(),
                "a sent direct command with an unknown result must not be replayed on the cloud"
            );
            let status = c
                .context
                .diagnostics()
                .devices
                .into_iter()
                .find(|device| device.did == DID)
                .unwrap();
            let command = status
                .commands
                .iter()
                .find(|command| command.channel == BLOW)
                .unwrap();
            let last = command.last.as_ref().unwrap();
            assert_eq!(last.actual_path, Some(PathKind::Direct));
            assert!(matches!(
                last.steps[0].receipt,
                crate::xiaomi::adapter::StepReceipt::Uncertain(_)
            ));
            async_io::Timer::after(Duration::from_millis(200)).await;
            assert!(c.cloud.lock().unwrap().action_requests.is_empty());
        })
        .await;
    }));
}
