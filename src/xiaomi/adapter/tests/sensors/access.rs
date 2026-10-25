//! Measurement eligibility across mixed controls, cloud access and online signals.

use super::*;

fn mixed_proxy_instance() -> Value {
    let mut instance: Value = serde_json::from_str(include_str!(
        "../../../../../tests/fixtures/miot/xiaomi-light-ceil04.json"
    ))
    .unwrap();
    let sensor: Value = serde_json::from_str(include_str!(
        "../../../../../tests/fixtures/miot/miaomiaoce-t6.json"
    ))
    .unwrap();
    let mut environment = sensor["services"]
        .as_array()
        .unwrap()
        .iter()
        .find(|service| service["iid"] == 3)
        .unwrap()
        .clone();
    environment["iid"] = json!(42);
    instance["services"]
        .as_array_mut()
        .unwrap()
        .push(environment);
    instance
}

#[test]
fn mixed_proxy_measurements_do_not_require_an_eligible_control_path() {
    let instance = mixed_proxy_instance();
    let rig = Rig::custom_lights(&[("proxy.sensor", "xiaomi.light.ceil04", instance)]);
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.online.clear();
        cloud
            .raw_values
            .insert(("proxy.sensor".into(), 42, 1001), json!(21.0));
        cloud
            .raw_values
            .insert(("proxy.sensor".into(), 42, 1008), json!(45.0));
    }
    block_on(rig.run(|c| async move {
        until(|| {
            measurement(&c.context, "proxy.sensor", "temperature").value == Some(21.0)
                && measurement(&c.context, "proxy.sensor", "humidity").value == Some(45.0)
        })
        .await;
        let status = battery_status(&c.context, "proxy.sensor");
        assert_eq!(status.selected_control, None);
        assert_eq!(status.unavailable_reason, None);
        assert_eq!(status.physical_online, None);
        assert!(
            status
                .functions
                .iter()
                .find(|function| function.function == "main")
                .unwrap()
                .selected_read
                .is_none()
        );
        assert!(
            status
                .functions
                .iter()
                .filter(|function| function.function != "main")
                .all(|function| function.selected_read == Some(PathKind::Cloud))
        );
        assert_eq!(
            work(&c.context, "proxy.sensor").physical().availability(),
            Availability::Available
        );
    }));
}

#[test]
fn mixed_proxy_cloud_notifications_cannot_borrow_another_functions_access() {
    let (rig, listener, cloud) = with_cloud_rig(Rig::custom_lights(&[(
        "proxy.sensor",
        "xiaomi.light.ceil04",
        mixed_proxy_instance(),
    )]));
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            until(|| {
                battery_status(&c.context, "proxy.sensor")
                    .functions
                    .iter()
                    .all(|function| function.notification_source == Some(PathKind::Cloud))
            })
            .await;
            let current = work(&c.context, "proxy.sensor");
            let source = current.function_observer("main").unwrap();
            let power = current
                .native
                .device()
                .functions()
                .iter()
                .find(|function| function.id() == "main")
                .unwrap()
                .power()
                .unwrap();
            let main = current.physical().function("main").unwrap();
            main.observe(false);

            // Revoke the actual control eligibility and deliver queued values
            // synchronously, before asynchronous source selection can run.
            c.context
                .live
                .borrow_mut()
                .devices
                .get_mut("proxy.sensor")
                .unwrap()
                .cloud_access
                .as_mut()
                .unwrap()
                .online = false;
            assert!(current.function_observer("main").is_some());
            c.context.property_value_event(
                &source,
                "proxy.sensor",
                PropertyBinding {
                    siid: 42,
                    piid: 1001,
                },
                &json!(22.0),
                Some(1),
                false,
            );
            c.context.property_value_event(
                &source,
                "proxy.sensor",
                power,
                &json!(true),
                Some(1),
                false,
            );
            assert_eq!(
                measurement(&c.context, "proxy.sensor", "temperature").value,
                Some(22.0)
            );
            assert_eq!(main.snapshot().power(), Some(Power::Off));
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
