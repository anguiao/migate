//! Automatic sensor reads and temperature validity during hub degradation.

use super::*;
use crate::device::{
    AmbientTemperature, FunctionDescription, FunctionKind, PhysicalDevice,
    PhysicalDeviceDescription, TemperatureGrid, TemperatureSourceState,
};

const ID: &str = "temperature:3:1001";

#[test]
fn degraded_hub_remains_a_candidate_without_sending_automatic_sensor_reads() {
    block_on(Rig::sensors(&["sensor"]).run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(sensor_peer(21.0, 45.0), |peer| async move {
            let current = work(&control.context, "sensor");
            until(|| {
                measurement(&control.context, "sensor", ID).value == Some(21.0)
                    && current.function_observer(ID).is_some()
                    && !current.refresh.borrow().contains(ID, TEMPERATURE)
                    && !current
                        .refresh
                        .borrow()
                        .contains("humidity:3:1008", HUMIDITY)
            })
            .await;
            control
                .clock
                .automatic
                .store(false, std::sync::atomic::Ordering::Relaxed);
            let terminal = control.terminal();
            control.context.hub.borrow_mut().degrade();
            control.context.update_device_availability("sensor");
            let details = terminal
                .handle_line("status sensor")
                .await
                .unwrap()
                .unwrap();
            let routing = details.split_once("sensor routing:\n").unwrap().1;
            for expected in [
                "  Usable read paths: hub.",
                "  Selected read: none.",
                "  Notification source: hub.",
                "  temperature:3:1001: selected read none; notification source hub.",
            ] {
                assert!(routing.lines().any(|line| line == expected), "{details}");
            }
            let status = battery_status(&control.context, "sensor");
            assert_eq!(status.usable_read_paths, [PathKind::Local]);
            assert_eq!(
                status.selected_read, None,
                "a degraded hub cannot execute automatic reads"
            );
            assert_eq!(
                status
                    .functions
                    .iter()
                    .find(|function| function.function == ID)
                    .unwrap()
                    .selected_read,
                None
            );
            assert_eq!(status.notification_source, Some(PathKind::Local));

            let reads = peer.count("get", "sensor", 3);
            current.refresh_property(ID, TEMPERATURE);
            for _ in 0..50 {
                future::yield_now().await;
            }
            async_io::Timer::after(Duration::from_millis(20)).await;
            assert_eq!(peer.count("get", "sensor", 3), reads);

            peer.notify_value_at("sensor", TEMPERATURE, json!(22.0), Some(100), false);
            until(|| measurement(&control.context, "sensor", ID).value == Some(22.0)).await;
            assert_eq!(current.physical().availability(), Availability::Available);
        })
        .await;
    }));
}

#[test]
fn routing_evaluation_uses_the_supplied_deadline_and_keeps_owned_decisions_read_only() {
    let rig = Rig::sensors(&["sensor"]);
    rig.cloud.lock().unwrap().online.insert("sensor".into());
    block_on(rig.run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(sensor_peer(21.0, 45.0), |_peer| async move {
            let current = work(&control.context, "sensor");
            until(|| {
                current.function_observer(ID).is_some()
                    && control.context.directory.borrow().scope_valid()
            })
            .await;
            control
                .clock
                .automatic
                .store(false, std::sync::atomic::Ordering::Relaxed);
            let deadline = control
                .context
                .credentials
                .borrow()
                .certificate_validity()
                .unwrap()
                .not_after;
            let observer = current.function_observer(ID).unwrap();
            let previous_reason = control.context.directory.borrow().devices["sensor"]
                .routing_history
                .as_ref()
                .and_then(|history| history.reason.clone())
                .clone();
            let pending = current.refresh.borrow().contains(ID, TEMPERATURE);
            let notified = control.context.status_changed.listen();
            let changed = control.context.changed.listen();
            let refreshed = control.context.refresh_changed.listen();

            let before = control.context.routing_evaluation_at(deadline - 1);
            assert!(matches!(
                before.devices["sensor"].read.selected,
                Some(super::super::super::routing::ControlPath::Local(_))
            ));
            for _ in 0..2 {
                let after = control.context.routing_evaluation_at(deadline);
                let routes = &after.devices["sensor"];
                assert_eq!(
                    routes
                        .read
                        .candidates
                        .hub
                        .as_ref()
                        .err()
                        .map(String::as_str),
                    Some("gateway certificate not currently valid")
                );
                assert!(matches!(
                    routes.read.selected,
                    Some(super::super::super::routing::ControlPath::Cloud(_))
                ));
                assert!(matches!(
                    routes.functions[ID].read.selected,
                    Some(super::super::super::routing::ControlPath::Cloud(_))
                ));
                assert_eq!(routes.notification_source, None);
                assert!(routes.functions[ID].notification.active.is_none());
                assert!(before.devices["sensor"].read.candidates.hub.is_ok());
            }
            assert!(current.function_observer(ID).unwrap().same(&observer));
            assert_eq!(current.refresh.borrow().contains(ID, TEMPERATURE), pending);
            assert_eq!(
                control.context.directory.borrow().devices["sensor"]
                    .routing_history
                    .as_ref()
                    .and_then(|history| history.reason.clone()),
                previous_reason
            );
            assert!(poll_once(notified).await.is_none());
            assert!(poll_once(changed).await.is_none());
            assert!(poll_once(refreshed).await.is_none());
        })
        .await;
    }));
}

fn temperature_only_rig(battery: bool) -> Rig {
    let mut instance = sensor_native("sensor").raw.instance;
    let services = instance["services"].as_array_mut().unwrap();
    if !battery {
        services.retain(|service| service["iid"] != 2);
    }
    let environment = services
        .iter_mut()
        .find(|service| service["iid"] == 3)
        .unwrap();
    environment["properties"]
        .as_array_mut()
        .unwrap()
        .retain(|property| property["iid"] == 1001);
    Rig::custom_lights(&[("sensor", "miaomiaoce.sensor_ht.t6", instance)])
}

#[test]
fn temperature_only_devices_lose_reachability_and_wait_for_a_new_recovered_notification() {
    for battery in [false, true] {
        block_on(temperature_only_rig(battery).run(|c| async move {
            c.announce();
            let control = &c;
            c.peer(sensor_peer(21.0, 45.0), |_peer| async move {
                let current = work(&control.context, "sensor");
                let temperature = current.physical().function(ID).unwrap();
                until(|| {
                    matches!(
                        temperature.snapshot().temperature_source(),
                        Some(TemperatureSourceState::Available(_))
                    ) && current.function_observer(ID).is_some()
                })
                .await;
                control
                    .clock
                    .automatic
                    .store(false, std::sync::atomic::Ordering::Relaxed);
                let historical = temperature.snapshot().measurement().unwrap();
                let description = PhysicalDeviceDescription::new(
                    DeviceKey::new("test", "ac"),
                    "AC",
                    vec![FunctionDescription {
                        id: "ac".into(),
                        label: "AC".into(),
                        kind: FunctionKind::AirConditioner {
                            temperature: TemperatureGrid::new(16, 30, 1).unwrap(),
                            vertical_swing: false,
                        },
                    }],
                )
                .unwrap();
                let (ac, _) = PhysicalDevice::new(description);
                let ac = ac.function("ac").unwrap();
                ac.set_ambient_temperature_source(Some((
                    temperature.function_key().clone(),
                    Some(temperature.clone()),
                )));
                assert_eq!(
                    ac.snapshot().ambient_temperature(),
                    Some(AmbientTemperature::Available(historical))
                );
                let observer = current.function_observer(ID).unwrap();
                control.context.hub.borrow_mut().degrade();
                current.revoke_observer(ID);
                control.context.update_device_availability("sensor");
                assert!(
                    matches!(
                        current.physical().availability(),
                        Availability::Unavailable(_)
                    ),
                    "battery={battery}"
                );
                assert_eq!(
                    temperature.snapshot().temperature_source(),
                    Some(TemperatureSourceState::Unavailable)
                );
                assert_eq!(temperature.snapshot().measurement(), Some(historical));
                assert_eq!(
                    ac.snapshot().ambient_temperature(),
                    Some(AmbientTemperature::SourceUnavailable)
                );

                current.install_observer(ID, observer.clone());
                control.context.update_device_availability("sensor");
                assert_eq!(current.physical().availability(), Availability::Available);
                assert_eq!(
                    ac.snapshot().ambient_temperature(),
                    Some(AmbientTemperature::AwaitingReading)
                );
                control.context.property_value_event(
                    &observer,
                    "sensor",
                    TEMPERATURE,
                    &json!("invalid"),
                    Some(100),
                    false,
                );
                assert_eq!(
                    ac.snapshot().ambient_temperature(),
                    Some(AmbientTemperature::AwaitingReading)
                );
                control.context.property_value_event(
                    &observer,
                    "sensor",
                    TEMPERATURE,
                    &json!(22.0),
                    Some(101),
                    false,
                );
                assert!(matches!(
                    ac.snapshot().ambient_temperature(),
                    Some(AmbientTemperature::Available(value)) if value.value == Some(22.0)
                ));
            })
            .await;
        }));
    }
}
