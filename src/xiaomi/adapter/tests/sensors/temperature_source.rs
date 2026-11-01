//! Temperature source validity follows the selected function's actual paths.

use super::*;
use crate::device::TemperatureSourceState;

const ID: &str = "temperature:3:1001";

fn source(context: &super::super::state::Context) -> TemperatureSourceState {
    super::support::function(&context.registry, "sensor", ID)
        .snapshot()
        .temperature_source()
        .unwrap()
}

#[test]
fn read_path_keeps_valid_temperature_through_subscription_and_read_failures_and_silence() {
    block_on(Rig::sensors(&["sensor"]).run(|c| async move {
        c.announce();
        let peer = sensor_peer(21.0, 45.0);
        peer.state
            .borrow_mut()
            .sub_reject
            .insert(("sensor".into(), 3, 1001));
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| {
                matches!(
                    source(&control.context),
                    TemperatureSourceState::Available(_)
                )
            })
            .await;
            until(|| peer.count("subscribe", "sensor", 3) >= 2).await;
            until(|| {
                work(&control.context, "sensor")
                    .function_subscription_problem(ID)
                    .is_some()
            })
            .await;
            let reading = measurement(&control.context, "sensor", ID);
            assert_eq!(reading.value, Some(21.0));
            assert!(reading.received_at.is_some());
            assert_eq!(
                source(&control.context),
                TemperatureSourceState::Available(reading)
            );

            let gets = peer.count("get", "sensor", 3);
            peer.state
                .borrow_mut()
                .read_fail
                .insert(("sensor".into(), 3, 1001));
            work(&control.context, "sensor").refresh_property(ID, TEMPERATURE);
            until(|| peer.count("get", "sensor", 3) > gets).await;
            until(|| {
                work(&control.context, "sensor")
                    .refresh
                    .borrow()
                    .property_problem(ID, TEMPERATURE)
                    .is_some()
            })
            .await;
            let diagnostic = control.context.diagnostics();
            let status = diagnostic.devices[0]
                .functions
                .iter()
                .find(|function| function.function == ID)
                .unwrap();
            assert!(status.sync_limited_reason.is_some());
            assert!(status.read_problem.is_some());
            assert_eq!(
                source(&control.context),
                TemperatureSourceState::Available(reading)
            );
            control.clock.advance(Duration::from_secs(30 * 60));
            for _ in 0..50 {
                future::yield_now().await;
            }
            assert_eq!(
                source(&control.context),
                TemperatureSourceState::Available(reading)
            );
        })
        .await;
    }));
}

#[test]
fn another_measurement_observer_cannot_keep_temperature_valid_without_its_own_path() {
    let (rig, listener, cloud) = with_cloud_sensors(&["sensor"]);
    {
        let mut values = rig.cloud.lock().unwrap();
        values
            .raw_values
            .insert(("sensor".into(), 3, 1001), json!(21.0));
        values
            .raw_values
            .insert(("sensor".into(), 3, 1008), json!(45.0));
    }
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            until(|| matches!(source(&c.context), TemperatureSourceState::Available(_))
                && work(&c.context, "sensor").function_observer("humidity:3:1008").is_some()
                && work(&c.context, "sensor").function_observer(ID).is_some()).await;
            let current = work(&c.context, "sensor");
            let temperature = current.physical().function(ID).unwrap();
            let mut changes = temperature.subscribe();
            let old = temperature.snapshot().measurement().unwrap();
            let observer = current.function_observer(ID).unwrap();

            // Keep humidity's valid cloud observer, but remove the temperature
            // observer and pause the shared HTTP route.
            c.context.http_paused.set(true);
            current.revoke_observer(ID);
            c.context.update_device_availability("sensor");
            assert_eq!(current.physical().availability(), Availability::Available);
            assert_eq!(changes.changed().await.temperature_source(), Some(TemperatureSourceState::Unavailable));
            assert_eq!(temperature.snapshot().measurement(), Some(old));

            // Restoring the observer alone cannot revive its old measurement.
            current.install_observer(ID, observer.clone());
            c.context.update_device_availability("sensor");
            assert_eq!(changes.changed().await.temperature_source(), Some(TemperatureSourceState::AwaitingReading));
            assert_eq!(temperature.snapshot().measurement(), Some(old));
            c.context.property_value_event(&observer, "sensor", TEMPERATURE, &json!("invalid"), Some(100), false);
            assert_eq!(source(&c.context), TemperatureSourceState::AwaitingReading);
            c.context.property_value_event(&observer, "sensor", TEMPERATURE, &json!(22.0), Some(101), false);
            let restored = changes.changed().await.temperature_source().unwrap();
            assert!(matches!(restored, TemperatureSourceState::Available(value)
                if value.value == Some(22.0) && value.source_time == Some(101) && value.received_at.is_some()));
        }).await;
    }));
}

#[test]
fn revoked_temperature_instance_becomes_unavailable_and_cannot_accept_its_old_source() {
    block_on(Rig::sensors(&["sensor"]).run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(sensor_peer(20.0, 40.0), |peer| async move {
            until(|| {
                matches!(
                    source(&control.context),
                    TemperatureSourceState::Available(_)
                )
            })
            .await;
            let current = work(&control.context, "sensor");
            let temperature = current.physical().function(ID).unwrap();
            let mut changes = temperature.subscribe();
            let historical = temperature.snapshot().measurement().unwrap();
            control.context.revoke("sensor", "test removal").unwrap();
            assert_eq!(
                changes.changed().await.temperature_source(),
                Some(TemperatureSourceState::Unavailable)
            );
            peer.notify_value_at("sensor", TEMPERATURE, json!(99.0), Some(999), false);
            for _ in 0..50 {
                future::yield_now().await;
            }
            assert_eq!(
                temperature.snapshot().temperature_source(),
                Some(TemperatureSourceState::Unavailable)
            );
            assert_eq!(temperature.snapshot().measurement(), Some(historical));
        })
        .await;
    }));
}

#[test]
fn recovered_read_path_waits_for_a_new_read_while_another_function_stays_available() {
    let (rig, listener, cloud) = with_cloud_sensors(&["sensor"]);
    let temperature_path = "device/sensor/up/properties_changed/3/1001";
    cloud
        .state
        .borrow_mut()
        .reject
        .insert(temperature_path.into(), SubscribeReasonCode::QuotaExceeded);
    {
        let mut values = rig.cloud.lock().unwrap();
        values
            .raw_values
            .insert(("sensor".into(), 3, 1001), json!(21.0));
        values
            .raw_values
            .insert(("sensor".into(), 3, 1008), json!(45.0));
    }
    let (release, gate) = flume::bounded(1);
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            until(|| {
                matches!(source(&c.context), TemperatureSourceState::Available(_))
                    && work(&c.context, "sensor")
                        .function_observer("humidity:3:1008")
                        .is_some()
                    && cloud.count(temperature_path) > 0
            })
            .await;
            let current = work(&c.context, "sensor");
            until(|| {
                current.function_subscription_problem(ID).is_some()
                    && !current.refresh.borrow().contains(ID, TEMPERATURE)
            })
            .await;
            assert!(current.function_observer(ID).is_none());
            let temperature = current.physical().function(ID).unwrap();
            let old = temperature.snapshot().measurement().unwrap();

            // Lose and restore the route before asynchronous selection runs.
            c.context.http_paused.set(true);
            c.context.update_device_availability("sensor");
            assert_eq!(current.physical().availability(), Availability::Available);
            assert_eq!(source(&c.context), TemperatureSourceState::Unavailable);

            let prior_reads = c
                .cloud
                .lock()
                .unwrap()
                .property_reads
                .iter()
                .filter(|(did, siid, piid)| did == "sensor" && *siid == 3 && *piid == 1001)
                .count();
            {
                let mut values = c.cloud.lock().unwrap();
                values
                    .raw_values
                    .insert(("sensor".into(), 3, 1001), json!(22.0));
                values.read_gates.insert(("sensor".into(), 3, 1001), gate);
            }
            c.context.http_paused.set(false);
            c.context.update_device_availability("sensor");
            assert_eq!(source(&c.context), TemperatureSourceState::AwaitingReading);
            assert_eq!(temperature.snapshot().measurement(), Some(old));
            let scheduled = future::or(
                async {
                    until(|| {
                        c.cloud
                            .lock()
                            .unwrap()
                            .property_reads
                            .iter()
                            .filter(|(did, siid, piid)| {
                                did == "sensor" && *siid == 3 && *piid == 1001
                            })
                            .count()
                            > prior_reads
                    })
                    .await;
                    true
                },
                async {
                    async_io::Timer::after(Duration::from_secs(2)).await;
                    false
                },
            )
            .await;
            assert!(
                scheduled,
                "restoring the read path must request a fresh reading"
            );
            assert_eq!(source(&c.context), TemperatureSourceState::AwaitingReading);
            release.send(()).unwrap();
            until(|| {
                matches!(source(&c.context), TemperatureSourceState::Available(value)
                if value.value == Some(22.0))
            })
            .await;
        })
        .await;
    }));
}

#[test]
fn readmitted_device_gets_a_fresh_temperature_instance() {
    let rig = Rig::sensors(&["sensor"]);
    {
        let mut values = rig.cloud.lock().unwrap();
        values.online.insert("sensor".into());
        values
            .raw_values
            .insert(("sensor".into(), 3, 1001), json!(20.0));
        values
            .raw_values
            .insert(("sensor".into(), 3, 1008), json!(40.0));
    }
    let (release, gate) = flume::bounded(1);
    block_on(rig.run(|c| async move {
        until(|| matches!(source(&c.context), TemperatureSourceState::Available(_))).await;
        let current = work(&c.context, "sensor");
        let old_physical = current.physical();
        let old = old_physical.function(ID).unwrap();
        let historical = old.snapshot().measurement().unwrap();
        let native = current.native.clone();
        let (access, cloud_access) = {
            let live = c.context.live.borrow();
            let slot = &live.devices["sensor"];
            (slot.access.clone(), slot.cloud_access.clone())
        };
        {
            let mut values = c.cloud.lock().unwrap();
            values.raw_values.insert(("sensor".into(), 3, 1001), json!(23.0));
            values.read_gates.insert(("sensor".into(), 3, 1001), gate);
        }
        c.context.revoke("sensor", "test revocation").unwrap();
        assert_eq!(old.snapshot().temperature_source(), Some(TemperatureSourceState::Unavailable));
        {
            let mut live = c.context.live.borrow_mut();
            let slot = live.devices.get_mut("sensor").unwrap();
            slot.candidate.as_mut().unwrap().native = Some(native);
            slot.confirmed = true;
            slot.access = access;
            slot.cloud_access = cloud_access;
        }
        c.context.reconcile("sensor").unwrap();
        until(|| c.context.registry.get(&DeviceKey::new("xiaomi", "sensor"))
            .is_some_and(|physical| !old_physical.same_instance(&physical))).await;
        let new = function(&c.context.registry, "sensor", ID);
        assert_eq!(new.snapshot().temperature_source(), Some(TemperatureSourceState::AwaitingReading));
        assert_eq!(new.snapshot().measurement().unwrap().value, None);
        release.send(()).unwrap();
        until(|| matches!(new.snapshot().temperature_source(), Some(TemperatureSourceState::Available(value))
            if value.value == Some(23.0))).await;
        assert_eq!(old.snapshot().temperature_source(), Some(TemperatureSourceState::Unavailable));
        assert_eq!(old.snapshot().measurement(), Some(historical));
    }));
}
