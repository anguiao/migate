//! Read-only numeric sensor observations through the real source runtime.

use super::*;
use crate::device::{Availability, MeasurementSnapshot};
use crate::xiaomi::capability::PropertyBinding;

const TEMPERATURE: PropertyBinding = PropertyBinding {
    siid: 3,
    piid: 1001,
};
const HUMIDITY: PropertyBinding = PropertyBinding {
    siid: 3,
    piid: 1008,
};

fn measurement(context: &super::super::state::Context, did: &str, id: &str) -> MeasurementSnapshot {
    super::support::function(&context.registry, did, id)
        .snapshot()
        .measurement()
        .expect("sensor function")
}

fn work(context: &super::super::state::Context, did: &str) -> Rc<super::super::work::DeviceWork> {
    context.live.borrow().devices[did]
        .admitted
        .as_ref()
        .unwrap()
        .clone()
}

#[test]
fn initial_reads_keep_independent_values_and_upstream_update_times() {
    block_on(Rig::sensors(&["sensor"]).run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        peer.value("sensor", TEMPERATURE, json!(21.5));
        peer.value("sensor", HUMIDITY, json!(45.0));
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
                measurement(&control.context, "sensor", "temperature").value == Some(21.5)
                    && measurement(&control.context, "sensor", "humidity").value == Some(45.0)
            })
            .await;
            let temperature = measurement(&control.context, "sensor", "temperature");
            assert_eq!(temperature.source_time, Some(1_700_000_100));
            assert!(temperature.received_at.is_some());
            let humidity = measurement(&control.context, "sensor", "humidity");
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
fn notifications_update_source_time_and_missing_time_stays_unknown() {
    block_on(Rig::sensors(&["sensor"]).run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        peer.value("sensor", TEMPERATURE, json!(20.0));
        peer.value("sensor", HUMIDITY, json!(40.0));
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| {
                measurement(&control.context, "sensor", "temperature").value == Some(20.0)
                    && measurement(&control.context, "sensor", "humidity").value == Some(40.0)
            })
            .await;
            assert_eq!(
                measurement(&control.context, "sensor", "temperature").source_time,
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
                let value = measurement(&control.context, "sensor", "temperature");
                value.value == Some(21.0) && value.source_time == Some(1_700_000_300)
            })
            .await;
            peer.notify_value_at("sensor", TEMPERATURE, json!(21.5), None, false);
            until(|| {
                let value = measurement(&control.context, "sensor", "temperature");
                value.value == Some(21.5) && value.source_time.is_none()
            })
            .await;
            // Humidity was not touched by the temperature notification.
            assert_eq!(
                measurement(&control.context, "sensor", "humidity").value,
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
        let peer = Rc::new(Peer::default());
        peer.value("sensor", TEMPERATURE, json!(20.0));
        peer.value("sensor", HUMIDITY, json!(40.0));
        peer.state
            .borrow_mut()
            .update_times
            .insert(("sensor".into(), 3, 1001), 1_700_000_400);
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| measurement(&control.context, "sensor", "temperature").value == Some(20.0))
                .await;
            let first = measurement(&control.context, "sensor", "temperature");
            assert_eq!(first.source_time, Some(1_700_000_400));
            control.clock.advance(Duration::from_secs(5));
            peer.value("sensor", TEMPERATURE, json!(20.5));
            work(&control.context, "sensor").refresh_property("temperature", TEMPERATURE);
            until(|| {
                let value = measurement(&control.context, "sensor", "temperature");
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
        let peer = Rc::new(Peer::default());
        peer.value("sensor", TEMPERATURE, json!(20.0));
        peer.value("sensor", HUMIDITY, json!(40.0));
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| measurement(&control.context, "sensor", "temperature").value == Some(20.0))
                .await;
            let current = work(&control.context, "sensor");
            let earlier = current.observations.read_stamp("temperature", TEMPERATURE);
            peer.notify_value_at("sensor", TEMPERATURE, json!(30.0), Some(300), false);
            until(|| measurement(&control.context, "sensor", "temperature").value == Some(30.0))
                .await;
            // The read started before the notification completes afterwards;
            // its older stamp makes the value stale even without timestamp
            // arbitration.
            current
                .observe_read_at(
                    "temperature",
                    TEMPERATURE,
                    &earlier,
                    &json!(20.0),
                    Some(100),
                    control.clock.unix_now(),
                )
                .unwrap();
            let value = measurement(&control.context, "sensor", "temperature");
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
        let peer = Rc::new(Peer::default());
        peer.value("sensor", TEMPERATURE, json!(21.0));
        peer.value("sensor", HUMIDITY, json!(45.0));
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| {
                measurement(&control.context, "sensor", "temperature").value == Some(21.0)
                    && measurement(&control.context, "sensor", "humidity").value == Some(45.0)
            })
            .await;
            for invalid in [json!("broken"), json!(150.0), json!(true), json!(null)] {
                peer.notify_value("sensor", TEMPERATURE, invalid, false);
                for _ in 0..10 {
                    future::yield_now().await;
                }
                assert_eq!(
                    measurement(&control.context, "sensor", "temperature").value,
                    Some(21.0)
                );
                assert_eq!(
                    measurement(&control.context, "sensor", "humidity").value,
                    Some(45.0)
                );
            }
            peer.state
                .borrow_mut()
                .read_fail
                .insert(("sensor".into(), 3, 1008));
            work(&control.context, "sensor").refresh_property("humidity", HUMIDITY);
            for _ in 0..50 {
                future::yield_now().await;
            }
            assert_eq!(
                measurement(&control.context, "sensor", "temperature").value,
                Some(21.0)
            );
            assert_eq!(
                measurement(&control.context, "sensor", "humidity").value,
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
fn a_read_only_device_is_available_without_notifications_and_recovers_them() {
    block_on(Rig::sensors(&["sensor"]).run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        peer.value("sensor", TEMPERATURE, json!(19.0));
        peer.value("sensor", HUMIDITY, json!(39.0));
        peer.state.borrow_mut().pushless.insert("sensor".into());
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| measurement(&control.context, "sensor", "temperature").value == Some(19.0))
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
            until(|| measurement(&control.context, "sensor", "temperature").value == Some(19.5))
                .await;
        })
        .await;
    }));
}

#[test]
fn unreliable_ble_and_proxy_cloud_offline_does_not_block_reads() {
    let dids = ["blt.sensor", "proxy.sensor"];
    let rig = Rig::sensors(&dids);
    {
        let mut cloud = rig.cloud.lock().unwrap();
        for did in dids {
            cloud.raw_values.insert((did.into(), 3, 1001), json!(18.0));
            cloud.raw_values.insert((did.into(), 3, 1008), json!(38.0));
        }
        cloud
            .update_times
            .insert(("blt.sensor".into(), 3, 1001), 1_700_000_500);
        cloud.online.clear();
    }
    block_on(rig.run(|c| async move {
        for did in dids {
            wait_cloud_values(&c, did).await;
            let physical = c
                .context
                .registry
                .get(&DeviceKey::new("xiaomi", did))
                .unwrap();
            assert_eq!(physical.availability(), Availability::Available);
            let status = c
                .context
                .status
                .borrow()
                .devices
                .iter()
                .find(|device| device.did == did)
                .cloned()
                .unwrap();
            assert_eq!(status.selected_control, None);
            assert_eq!(status.selected_read, Some(PathKind::Cloud));
            assert_eq!(status.physical_online, None);
            assert_eq!(
                measurement(&c.context, did, "temperature").value,
                Some(18.0)
            );
            assert_eq!(measurement(&c.context, did, "humidity").value, Some(38.0));
        }
        assert_eq!(
            measurement(&c.context, "blt.sensor", "temperature").source_time,
            Some(1_700_000_500)
        );
        // The skipped online flag never bypasses an explicit authorization
        // failure. Readings survive the path loss.
        c.context
            .live
            .borrow_mut()
            .devices
            .get_mut("blt.sensor")
            .unwrap()
            .cloud_forbidden = true;
        c.context.update_all_availability();
        assert_eq!(
            c.context
                .registry
                .get(&DeviceKey::new("xiaomi", "blt.sensor"))
                .unwrap()
                .availability(),
            Availability::Unavailable("no usable read or notification path".into())
        );
        assert_eq!(
            measurement(&c.context, "blt.sensor", "temperature").value,
            Some(18.0)
        );
        assert_eq!(
            measurement(&c.context, "blt.sensor", "humidity").value,
            Some(38.0)
        );
    }));
}

#[test]
fn a_reliable_offline_sensor_does_not_read_or_become_available() {
    let rig = Rig::sensors(&["sensor"]);
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud
            .raw_values
            .insert(("sensor".into(), 3, 1001), json!(18.0));
        cloud
            .raw_values
            .insert(("sensor".into(), 3, 1008), json!(38.0));
        cloud.online.clear();
    }
    block_on(rig.run(|c| async move {
        c.announce();
        for _ in 0..500 {
            let access_known = c.context.live.borrow().devices["sensor"]
                .cloud_access
                .is_some();
            if access_known {
                break;
            }
            async_io::Timer::after(Duration::from_millis(10)).await;
        }
        let physical = c
            .context
            .registry
            .get(&DeviceKey::new("xiaomi", "sensor"))
            .unwrap();
        assert_eq!(
            physical.availability(),
            Availability::Unavailable("no usable read or notification path".into())
        );
        let status = c
            .context
            .status
            .borrow()
            .devices
            .iter()
            .find(|device| device.did == "sensor")
            .cloned()
            .unwrap();
        assert_eq!(status.selected_control, None);
        assert_eq!(status.selected_read, None);
        assert_eq!(status.physical_online, Some(false));
        assert_eq!(measurement(&c.context, "sensor", "temperature").value, None);
        assert_eq!(measurement(&c.context, "sensor", "humidity").value, None);
    }));
}

async fn wait_cloud_values(c: &Control, did: &str) {
    for _ in 0..500 {
        if measurement(&c.context, did, "temperature").value == Some(18.0)
            && measurement(&c.context, did, "humidity").value == Some(38.0)
        {
            return;
        }
        async_io::Timer::after(Duration::from_millis(10)).await;
    }
    panic!("cloud sensor values were not read for {did}");
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
                measurement(&control.context, "one", "temperature").value == Some(11.0)
                    && measurement(&control.context, "one", "humidity").value == Some(22.0)
                    && measurement(&control.context, "two", "temperature").value == Some(33.0)
                    && measurement(&control.context, "two", "humidity").value == Some(44.0)
            })
            .await;
            peer.notify_value_at("two", TEMPERATURE, json!(35.0), Some(2), false);
            until(|| measurement(&control.context, "two", "temperature").value == Some(35.0)).await;
            assert_eq!(
                measurement(&control.context, "one", "temperature").value,
                Some(11.0)
            );
        })
        .await;
    }));
}

#[test]
fn losing_the_notification_path_retains_readings_and_recovers() {
    block_on(Rig::sensors(&["sensor"]).run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        peer.value("sensor", TEMPERATURE, json!(20.0));
        peer.value("sensor", HUMIDITY, json!(40.0));
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| {
                measurement(&control.context, "sensor", "temperature").value == Some(20.0)
                    && measurement(&control.context, "sensor", "humidity").value == Some(40.0)
                    && control
                        .context
                        .status
                        .borrow()
                        .devices
                        .iter()
                        .find(|device| device.did == "sensor")
                        .is_some_and(|device| device.notification_source == Some(PathKind::Local))
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
                measurement(&control.context, "sensor", "temperature").value,
                Some(20.0)
            );
            assert_eq!(
                measurement(&control.context, "sensor", "humidity").value,
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
                    .is_some_and(|device| device.notification_source == Some(PathKind::Local))
            })
            .await;
            peer.notify_value_at("sensor", HUMIDITY, json!(41.0), Some(5), false);
            until(|| measurement(&control.context, "sensor", "humidity").value == Some(41.0)).await;
            assert_eq!(
                measurement(&control.context, "sensor", "temperature").value,
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
        let peer = Rc::new(Peer::default());
        peer.value("sensor", TEMPERATURE, json!(20.0));
        peer.value("sensor", HUMIDITY, json!(40.0));
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| {
                measurement(&control.context, "sensor", "temperature").value == Some(20.0)
                    && measurement(&control.context, "sensor", "humidity").value == Some(40.0)
            })
            .await;
            work(&control.context, "sensor").retire();
            peer.notify_value_at("sensor", TEMPERATURE, json!(99.0), Some(999), false);
            peer.notify_value_at("sensor", HUMIDITY, json!(99.0), Some(999), false);
            for _ in 0..50 {
                future::yield_now().await;
            }
            assert_eq!(
                measurement(&control.context, "sensor", "temperature").value,
                Some(20.0)
            );
            assert_eq!(
                measurement(&control.context, "sensor", "humidity").value,
                Some(40.0)
            );
        })
        .await;
    }));
}

#[test]
fn prefixed_control_devices_keep_offline_blocking() {
    let dids = ["blt.outlet", "proxy.light"];
    let rig = Rig::new(true, &dids);
    block_on(rig.run(|c| async move {
        for _ in 0..500 {
            let known = dids.iter().all(|did| {
                c.context
                    .live
                    .borrow()
                    .devices
                    .get(*did)
                    .is_some_and(|slot| slot.cloud_access.is_some())
                    && c.context
                        .status
                        .borrow()
                        .devices
                        .iter()
                        .any(|d| d.did == *did)
            });
            if known {
                break;
            }
            async_io::Timer::after(Duration::from_millis(10)).await;
        }
        c.context.update_routing("prefixed control offline probe");
        for did in dids {
            assert_eq!(
                c.context.cloud_candidate(did, true),
                Err("device offline".into()),
                "{did}"
            );
            assert_eq!(
                c.context.cloud_candidate(did, false),
                Err("device offline".into()),
                "{did}"
            );
            let physical = c
                .context
                .registry
                .get(&DeviceKey::new("xiaomi", did))
                .unwrap();
            assert_eq!(
                physical.availability(),
                Availability::Unavailable("no usable read or notification path".into()),
                "{did}"
            );
            let status = c
                .context
                .status
                .borrow()
                .devices
                .iter()
                .find(|device| device.did == did)
                .cloned()
                .unwrap();
            assert_eq!(status.selected_control, None, "{did}");
            assert_eq!(status.selected_read, None, "{did}");
            assert_eq!(status.physical_online, Some(false), "{did}");
        }
    }));
}
