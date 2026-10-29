//! Native measurement observations, timestamps and read ordering.

use super::*;

#[test]
fn initial_reads_keep_independent_values_and_upstream_update_times() {
    block_on(Rig::sensors(&["sensor"]).run(|c| async move {
        c.announce();
        let peer = sensor_peer(21.5, 45.0);
        {
            let mut state = peer.state.borrow_mut();
            state
                .update_times
                .insert(("sensor".into(), 3, 1001), 1_700_000_100);
            state
                .update_times
                .insert(("sensor".into(), 3, 1008), 1_700_000_200);
        }
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| {
                measurement(&control.context, "sensor", "temperature:3:1001").value == Some(21.5)
                    && measurement(&control.context, "sensor", "humidity:3:1008").value
                        == Some(45.0)
            })
            .await;
            let temperature = measurement(&control.context, "sensor", "temperature:3:1001");
            assert_eq!(temperature.source_time, Some(1_700_000_100));
            assert!(temperature.received_at.is_some());
            let humidity = measurement(&control.context, "sensor", "humidity:3:1008");
            assert_eq!(humidity.source_time, Some(1_700_000_200));
            assert!(humidity.received_at.is_some());
            assert_eq!(
                control
                    .context
                    .registry
                    .get(&DeviceKey::new("xiaomi", "sensor"))
                    .unwrap()
                    .availability(),
                Availability::Available
            );
            // Sensors never produce a native write.
            assert_eq!(peer.count("rpcReq", "sensor", 3), 0);
            assert_eq!(control.publication.root_version(), 1);
            assert!(control.publication.plan().unwrap().is_none());
        })
        .await;
    }));
}

#[test]
fn native_silence_keeps_readings_without_polling_or_marking_unavailable() {
    block_on(Rig::sensors(&["sensor"]).run(|c| async move {
        c.announce();
        let peer = sensor_peer(21.5, 45.0);
        peer.value("sensor", BATTERY, json!(55));
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| {
                measurement(&control.context, "sensor", "temperature:3:1001").value == Some(21.5)
                    && measurement(&control.context, "sensor", "humidity:3:1008").value
                        == Some(45.0)
                    && battery(&control.context, "sensor").and_then(|b| b.value) == Some(55.0)
                    && battery_status(&control.context, "sensor").notification_source
                        == Some(PathKind::Local)
            })
            .await;
            let physical = work(&control.context, "sensor").physical();
            let before = physical
                .functions()
                .iter()
                .map(|function| function.snapshot())
                .collect::<Vec<_>>();
            let battery_before = physical.battery_snapshot();
            let reads = (
                peer.count("get", "sensor", 3),
                peer.count("get", "sensor", 2),
            );
            control.clock.advance(Duration::from_secs(30 * 60));
            for _ in 0..50 {
                future::yield_now().await;
            }
            assert_eq!(physical.availability(), Availability::Available);
            for (function, expected) in physical.functions().iter().zip(before) {
                assert_eq!(function.snapshot(), expected);
            }
            assert_eq!(physical.battery_snapshot(), battery_before);
            assert_eq!(
                (
                    peer.count("get", "sensor", 3),
                    peer.count("get", "sensor", 2)
                ),
                reads
            );
        })
        .await;
    }));
}

#[test]
fn notifications_update_source_time_and_missing_time_stays_unknown() {
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
            assert_eq!(
                measurement(&control.context, "sensor", "temperature:3:1001").source_time,
                None
            );
            peer.notify_value_at(
                "sensor",
                TEMPERATURE,
                json!(21.0),
                Some(1_700_000_300),
                false,
            );
            until(|| {
                let value = measurement(&control.context, "sensor", "temperature:3:1001");
                value.value == Some(21.0) && value.source_time == Some(1_700_000_300)
            })
            .await;
            peer.notify_value_at("sensor", TEMPERATURE, json!(21.5), None, false);
            until(|| {
                let value = measurement(&control.context, "sensor", "temperature:3:1001");
                value.value == Some(21.5) && value.source_time.is_none()
            })
            .await;
            // Humidity was not touched by the temperature notification.
            assert_eq!(
                measurement(&control.context, "sensor", "humidity:3:1008").value,
                Some(40.0)
            );
        })
        .await;
    }));
}

#[test]
fn a_cached_read_keeps_the_source_time_but_records_a_new_reception() {
    block_on(Rig::sensors(&["sensor"]).run(|c| async move {
        c.announce();
        let peer = sensor_peer(20.0, 40.0);
        peer.state
            .borrow_mut()
            .update_times
            .insert(("sensor".into(), 3, 1001), 1_700_000_400);
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| {
                measurement(&control.context, "sensor", "temperature:3:1001").value == Some(20.0)
            })
            .await;
            let first = measurement(&control.context, "sensor", "temperature:3:1001");
            assert_eq!(first.source_time, Some(1_700_000_400));
            control.clock.advance(Duration::from_secs(5));
            peer.value("sensor", TEMPERATURE, json!(20.5));
            work(&control.context, "sensor").refresh_property("temperature:3:1001", TEMPERATURE);
            until(|| {
                let value = measurement(&control.context, "sensor", "temperature:3:1001");
                value.value == Some(20.5)
                    && value.received_at > first.received_at
                    && value.source_time == first.source_time
            })
            .await;
        })
        .await;
    }));
}

#[test]
fn an_earlier_read_cannot_overwrite_a_later_notification() {
    block_on(Rig::sensors(&["sensor"]).run(|c| async move {
        c.announce();
        let peer = sensor_peer(20.0, 40.0);
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| {
                measurement(&control.context, "sensor", "temperature:3:1001").value == Some(20.0)
            })
            .await;
            let current = work(&control.context, "sensor");
            let earlier = current
                .observations
                .read_stamp("temperature:3:1001", TEMPERATURE);
            peer.notify_value_at("sensor", TEMPERATURE, json!(30.0), Some(300), false);
            until(|| {
                measurement(&control.context, "sensor", "temperature:3:1001").value == Some(30.0)
            })
            .await;
            // The read started before the notification completes afterwards;
            // its older stamp makes the value stale even without timestamp
            // arbitration.
            current
                .observe_read_at(
                    "temperature:3:1001",
                    TEMPERATURE,
                    &earlier,
                    &json!(20.0),
                    Observation {
                        source: PathKind::Local,
                        source_time: Some(100),
                        received_at: control.clock.unix_now(),
                    },
                )
                .unwrap();
            let value = measurement(&control.context, "sensor", "temperature:3:1001");
            assert_eq!(value.value, Some(30.0));
            assert_eq!(value.source_time, Some(300));
        })
        .await;
    }));
}

#[test]
fn invalid_observations_and_read_failures_preserve_each_independent_value() {
    block_on(Rig::sensors(&["sensor"]).run(|c| async move {
        c.announce();
        let peer = sensor_peer(21.0, 45.0);
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| {
                measurement(&control.context, "sensor", "temperature:3:1001").value == Some(21.0)
                    && measurement(&control.context, "sensor", "humidity:3:1008").value
                        == Some(45.0)
            })
            .await;
            for invalid in [json!("broken"), json!(150.0), json!(true), json!(null)] {
                peer.notify_value("sensor", TEMPERATURE, invalid, false);
                for _ in 0..10 {
                    future::yield_now().await;
                }
                assert_eq!(
                    measurement(&control.context, "sensor", "temperature:3:1001").value,
                    Some(21.0)
                );
                assert_eq!(
                    measurement(&control.context, "sensor", "humidity:3:1008").value,
                    Some(45.0)
                );
            }
            peer.state
                .borrow_mut()
                .read_fail
                .insert(("sensor".into(), 3, 1008));
            work(&control.context, "sensor").refresh_property("humidity:3:1008", HUMIDITY);
            for _ in 0..50 {
                future::yield_now().await;
            }
            assert_eq!(
                measurement(&control.context, "sensor", "temperature:3:1001").value,
                Some(21.0)
            );
            assert_eq!(
                measurement(&control.context, "sensor", "humidity:3:1008").value,
                Some(45.0)
            );
            assert!(
                control
                    .context
                    .registry
                    .get(&DeviceKey::new("xiaomi", "sensor"))
                    .unwrap()
                    .availability()
                    == Availability::Available
            );
        })
        .await;
    }));
}

#[test]
fn multiple_sensors_keep_their_own_values() {
    block_on(Rig::sensors(&["one", "two"]).run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        peer.value("one", TEMPERATURE, json!(11.0));
        peer.value("one", HUMIDITY, json!(22.0));
        peer.value("two", TEMPERATURE, json!(33.0));
        peer.value("two", HUMIDITY, json!(44.0));
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| {
                measurement(&control.context, "one", "temperature:3:1001").value == Some(11.0)
                    && measurement(&control.context, "one", "humidity:3:1008").value == Some(22.0)
                    && measurement(&control.context, "two", "temperature:3:1001").value
                        == Some(33.0)
                    && measurement(&control.context, "two", "humidity:3:1008").value == Some(44.0)
            })
            .await;
            peer.notify_value_at("two", TEMPERATURE, json!(35.0), Some(2), false);
            until(|| {
                measurement(&control.context, "two", "temperature:3:1001").value == Some(35.0)
            })
            .await;
            assert_eq!(
                measurement(&control.context, "one", "temperature:3:1001").value,
                Some(11.0)
            );
        })
        .await;
    }));
}

#[test]
fn co2_uses_independent_initial_reads_and_real_local_notifications() {
    let instance: Value = serde_json::from_str(include_str!(
        "../../../../../tests/fixtures/miot/xiaomi-new3pr.json"
    ))
    .unwrap();
    let rig = Rig::custom_lights(&[("co2", "xiaomi.sensor_ht.new3pr", instance)]);
    let co2 = PropertyBinding {
        siid: 8,
        piid: 1029,
    };
    let temperature = PropertyBinding {
        siid: 7,
        piid: 1001,
    };
    let humidity = PropertyBinding {
        siid: 7,
        piid: 1002,
    };
    let battery = PropertyBinding {
        siid: 4,
        piid: 1003,
    };
    block_on(rig.run(|c| async move {
        assert_eq!(measurement(&c.context, "co2", "co2:8:1029").value, None);
        c.announce();
        let peer = Rc::new(Peer::default());
        for (binding, value) in [
            (co2, json!(650)),
            (temperature, json!(21.5)),
            (humidity, json!(45)),
            (battery, json!(55)),
        ] {
            peer.value("co2", binding, value);
        }
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| {
                measurement(&control.context, "co2", "co2:8:1029").value == Some(650.0)
                    && measurement(&control.context, "co2", "temperature:7:1001").value
                        == Some(21.5)
                    && battery_status(&control.context, "co2")
                        .functions
                        .iter()
                        .all(|function| function.notification_source == Some(PathKind::Local))
            })
            .await;
            let before_temperature = measurement(&control.context, "co2", "temperature:7:1001");
            peer.notify_value_at("co2", co2, json!(700), Some(100), false);
            until(|| measurement(&control.context, "co2", "co2:8:1029").value == Some(700.0)).await;
            assert_eq!(
                measurement(&control.context, "co2", "co2:8:1029").source_time,
                Some(100)
            );
            assert_eq!(
                measurement(&control.context, "co2", "temperature:7:1001"),
                before_temperature
            );
            assert_eq!(
                measurement(&control.context, "co2", "humidity:7:1002").value,
                Some(45.0)
            );
            until(|| {
                work(&control.context, "co2")
                    .physical()
                    .battery_snapshot()
                    .unwrap()
                    .value
                    == Some(55.0)
            })
            .await;
            peer.notify_value_at("co2", temperature, json!(22.0), Some(101), false);
            until(|| {
                measurement(&control.context, "co2", "temperature:7:1001").value == Some(22.0)
            })
            .await;
            assert_eq!(
                measurement(&control.context, "co2", "co2:8:1029").value,
                Some(700.0)
            );
            assert!(control.cloud.lock().unwrap().property_writes.is_empty());
        })
        .await;
    }));
}

#[test]
fn illuminance_keeps_sub_lux_readings_and_updates_from_real_local_notifications() {
    let instance: Value = serde_json::from_str(include_str!(
        "../../../../../tests/fixtures/miot/yeelink-nl1.json"
    ))
    .unwrap();
    let rig = Rig::custom_lights(&[("illuminance", "yeelink.light.nl1", instance)]);
    let illuminance = PropertyBinding { siid: 4, piid: 1 };
    let battery = PropertyBinding { siid: 3, piid: 1 };
    block_on(rig.run(|c| async move {
        assert_eq!(
            measurement(&c.context, "illuminance", "illuminance:4:1").value,
            None
        );
        c.announce();
        let peer = Rc::new(Peer::default());
        peer.value("illuminance", illuminance, json!(0.125));
        peer.value("illuminance", battery, json!(55));
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| {
                measurement(&control.context, "illuminance", "illuminance:4:1").value == Some(0.125)
                    && battery_status(&control.context, "illuminance")
                        .functions
                        .iter()
                        .all(|function| function.notification_source == Some(PathKind::Local))
            })
            .await;
            for (value, time) in [(0.5, 100), (10.0, 101)] {
                peer.notify_value_at("illuminance", illuminance, json!(value), Some(time), false);
                until(|| {
                    measurement(&control.context, "illuminance", "illuminance:4:1").value
                        == Some(value)
                })
                .await;
                assert_eq!(
                    measurement(&control.context, "illuminance", "illuminance:4:1").source_time,
                    Some(time)
                );
            }
            until(|| {
                work(&control.context, "illuminance")
                    .physical()
                    .battery_snapshot()
                    .unwrap()
                    .value
                    == Some(55.0)
            })
            .await;
            assert!(control.cloud.lock().unwrap().property_writes.is_empty());
        })
        .await;
    }));
}
