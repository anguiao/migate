//! Independent notification source readiness, retirement and recovery.

use super::*;

#[test]
fn one_failed_measurement_subscription_keeps_other_measurements_live() {
    block_on(Rig::sensors(&["sensor"]).run(|c| async move {
        c.announce();
        let peer = sensor_peer(21.0, 45.0);
        peer.state
            .borrow_mut()
            .sub_reject
            .insert(("sensor".into(), HUMIDITY.siid, HUMIDITY.piid));
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| peer.count("subscribe", "sensor", 3) >= 2).await;
            for _ in 0..50 {
                future::yield_now().await;
            }
            assert_eq!(
                battery_status(&control.context, "sensor").notification_source,
                Some(PathKind::Local)
            );
            until(|| {
                measurement(&control.context, "sensor", "humidity:3:1008").value == Some(45.0)
            })
            .await;
            peer.notify_value_at("sensor", TEMPERATURE, json!(22.5), Some(22), false);
            until(|| {
                measurement(&control.context, "sensor", "temperature:3:1001").value == Some(22.5)
            })
            .await;
            assert_eq!(
                measurement(&control.context, "sensor", "humidity:3:1008").value,
                Some(45.0)
            );
        })
        .await;
    }));
}

#[test]
fn measurement_notification_sources_switch_and_recover_independently() {
    let (rig, listener, cloud) = with_cloud_sensors(&["sensor"]);
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            c.announce();
            let peer = sensor_peer(21.0, 45.0);
            peer.value("sensor", BATTERY, json!(55));
            peer.state.borrow_mut().sub_reject.insert((
                "sensor".into(),
                HUMIDITY.siid,
                HUMIDITY.piid,
            ));
            let control = &c;
            let cloud = &cloud;
            c.peer(peer, |peer| async move {
                let sources = || {
                    let status = battery_status(&control.context, "sensor");
                    status
                        .functions
                        .into_iter()
                        .map(|function| (function.function, function.notification_source))
                        .collect::<BTreeMap<_, _>>()
                };
                until(|| {
                    sources().get("temperature:3:1001") == Some(&Some(PathKind::Local))
                        && sources().get("humidity:3:1008") == Some(&Some(PathKind::Cloud))
                })
                .await;
                peer.value("sensor", TEMPERATURE, json!(22.0));
                peer.notify_value_at("sensor", TEMPERATURE, json!(22.0), Some(100), false);
                cloud.property_at("sensor", 3, 1008, json!(46.0));
                until(|| {
                    measurement(&control.context, "sensor", "temperature:3:1001").value
                        == Some(22.0)
                        && measurement(&control.context, "sensor", "humidity:3:1008").value
                            == Some(46.0)
                })
                .await;
                let temperature_subscriptions = peer
                    .state
                    .borrow()
                    .seen
                    .iter()
                    .filter(|(operation, value)| {
                        operation == "subscribe"
                            && value["did"] == "sensor"
                            && value["siid"] == TEMPERATURE.siid
                            && value["piid"] == TEMPERATURE.piid
                    })
                    .count();
                peer.state.borrow_mut().sub_reject.clear();
                peer.list_changed(&["sensor"]);
                until(|| {
                    sources().get("humidity:3:1008") == Some(&Some(PathKind::Local))
                        && !work(&control.context, "sensor")
                            .refresh
                            .borrow()
                            .contains("humidity:3:1008", HUMIDITY)
                })
                .await;
                peer.value("sensor", HUMIDITY, json!(47.0));
                cloud.property_at("sensor", 3, 1008, json!(90.0));
                peer.notify_value_at("sensor", HUMIDITY, json!(47.0), Some(101), false);
                until(|| {
                    measurement(&control.context, "sensor", "humidity:3:1008").value == Some(47.0)
                })
                .await;
                for _ in 0..30 {
                    future::yield_now().await;
                }
                assert_eq!(
                    measurement(&control.context, "sensor", "humidity:3:1008").value,
                    Some(47.0)
                );
                assert_eq!(
                    measurement(&control.context, "sensor", "temperature:3:1001").value,
                    Some(22.0)
                );
                assert_eq!(
                    peer.state
                        .borrow()
                        .seen
                        .iter()
                        .filter(|(operation, value)| operation == "subscribe"
                            && value["did"] == "sensor"
                            && value["siid"] == TEMPERATURE.siid
                            && value["piid"] == TEMPERATURE.piid)
                        .count(),
                    temperature_subscriptions
                );
            })
            .await;
        })
        .await;
    }));
}

#[test]
fn a_read_only_device_is_available_without_notifications_and_recovers_them() {
    block_on(Rig::sensors(&["sensor"]).run(|c| async move {
        c.announce();
        let peer = sensor_peer(19.0, 39.0);
        peer.state.borrow_mut().pushless.insert("sensor".into());
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| {
                measurement(&control.context, "sensor", "temperature:3:1001").value == Some(19.0)
            })
            .await;
            let status = control
                .context
                .status
                .borrow()
                .devices
                .iter()
                .find(|device| device.did == "sensor")
                .cloned()
                .unwrap();
            assert_eq!(status.selected_control, None);
            assert_eq!(status.selected_read, Some(PathKind::Local));
            assert!(status.sync_limited_reason.is_some());
            assert_eq!(status.physical_online, Some(false));
            assert_eq!(
                control
                    .context
                    .registry
                    .get(&DeviceKey::new("xiaomi", "sensor"))
                    .unwrap()
                    .availability(),
                Availability::Available
            );
            assert!(
                !status
                    .commands
                    .iter()
                    .any(|channel| channel.inflight.is_some() || channel.last.is_some())
            );

            peer.state.borrow_mut().pushless.clear();
            peer.list_changed(&["sensor"]);
            until(|| {
                control
                    .context
                    .status
                    .borrow()
                    .devices
                    .iter()
                    .find(|device| device.did == "sensor")
                    .is_some_and(|device| device.notification_source == Some(PathKind::Local))
            })
            .await;
            peer.notify_value_at("sensor", TEMPERATURE, json!(19.5), Some(9), false);
            until(|| {
                measurement(&control.context, "sensor", "temperature:3:1001").value == Some(19.5)
            })
            .await;
        })
        .await;
    }));
}

#[test]
fn losing_the_notification_path_retains_readings_and_recovers() {
    block_on(Rig::sensors(&["sensor"]).run(|c| async move {
        c.announce();
        let peer = sensor_peer(20.0, 40.0);
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| {
                measurement(&control.context, "sensor", "temperature:3:1001").value == Some(20.0)
                    && measurement(&control.context, "sensor", "humidity:3:1008").value
                        == Some(40.0)
                    && control
                        .context
                        .status
                        .borrow()
                        .devices
                        .iter()
                        .find(|device| device.did == "sensor")
                        .is_some_and(|device| {
                            device.functions.iter().all(|function| {
                                function.notification_source == Some(PathKind::Local)
                            })
                        })
            })
            .await;
            peer.state.borrow_mut().pushless.insert("sensor".into());
            peer.list_changed(&["sensor"]);
            until(|| {
                control
                    .context
                    .status
                    .borrow()
                    .devices
                    .iter()
                    .find(|device| device.did == "sensor")
                    .is_some_and(|device| device.notification_source.is_none())
            })
            .await;
            assert_eq!(
                measurement(&control.context, "sensor", "temperature:3:1001").value,
                Some(20.0)
            );
            assert_eq!(
                measurement(&control.context, "sensor", "humidity:3:1008").value,
                Some(40.0)
            );
            assert_eq!(
                control
                    .context
                    .registry
                    .get(&DeviceKey::new("xiaomi", "sensor"))
                    .unwrap()
                    .availability(),
                Availability::Available
            );

            peer.state.borrow_mut().pushless.clear();
            peer.list_changed(&["sensor"]);
            until(|| {
                control
                    .context
                    .status
                    .borrow()
                    .devices
                    .iter()
                    .find(|device| device.did == "sensor")
                    .is_some_and(|device| {
                        device
                            .functions
                            .iter()
                            .all(|function| function.notification_source == Some(PathKind::Local))
                    })
            })
            .await;
            peer.notify_value_at("sensor", HUMIDITY, json!(41.0), Some(5), false);
            until(|| {
                measurement(&control.context, "sensor", "humidity:3:1008").value == Some(41.0)
            })
            .await;
            assert_eq!(
                measurement(&control.context, "sensor", "temperature:3:1001").value,
                Some(20.0)
            );
        })
        .await;
    }));
}

#[test]
fn a_retired_or_old_source_cannot_update_sensor_state() {
    block_on(Rig::sensors(&["sensor"]).run(|c| async move {
        c.announce();
        let peer = sensor_peer(20.0, 40.0);
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| {
                measurement(&control.context, "sensor", "temperature:3:1001").value == Some(20.0)
                    && measurement(&control.context, "sensor", "humidity:3:1008").value
                        == Some(40.0)
            })
            .await;
            work(&control.context, "sensor").retire();
            peer.notify_value_at("sensor", TEMPERATURE, json!(99.0), Some(999), false);
            peer.notify_value_at("sensor", HUMIDITY, json!(99.0), Some(999), false);
            for _ in 0..50 {
                future::yield_now().await;
            }
            assert_eq!(
                measurement(&control.context, "sensor", "temperature:3:1001").value,
                Some(20.0)
            );
            assert_eq!(
                measurement(&control.context, "sensor", "humidity:3:1008").value,
                Some(40.0)
            );
        })
        .await;
    }));
}
