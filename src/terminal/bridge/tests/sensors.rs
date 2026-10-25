//! Terminal presentation and command boundary for read-only sensors.

use super::*;
use crate::device::{
    Availability, DeviceKey, FunctionDescription, FunctionKind, MeasurementRange, PhysicalDevice,
    PhysicalDeviceDescription,
};
use crate::terminal::bridge::measurement_text;
use crate::terminal::bridge::tests::support::{add_device, handle_line, terminal_command};
use crate::xiaomi::auth::AuthService;
use crate::xiaomi::cloud::CloudClient;
use futures_lite::future::block_on;

fn sensor() -> PhysicalDevice {
    sensor_with_battery(false)
}

fn sensor_with_battery(battery: bool) -> PhysicalDevice {
    let description = PhysicalDeviceDescription::new(
        DeviceKey::new("xiaomi", "sensor"),
        "Bedroom sensor".to_string(),
        vec![
            FunctionDescription {
                id: "temperature".into(),
                label: "Temperature".into(),
                kind: FunctionKind::Temperature(MeasurementRange::new(-30.0, 100.0).unwrap()),
            },
            FunctionDescription {
                id: "humidity".into(),
                label: "Humidity".into(),
                kind: FunctionKind::Humidity(MeasurementRange::new(0.0, 100.0).unwrap()),
            },
        ],
    )
    .unwrap();
    let description = if battery {
        description.with_battery()
    } else {
        description
    };
    let (physical, receivers) = PhysicalDevice::new(description);
    assert!(receivers.is_empty());
    physical
}

#[test]
fn status_shows_independent_values_and_cannot_control_a_sensor() {
    block_on(async {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let report = AuthService::new(store.xiaomi(), CloudClient::new().unwrap())
            .local_status()
            .unwrap();
        let status = AuthStatus::new(report, "/bin/migate".into(), directory.path().into());
        let registry = DeviceRegistry::new();
        let physical = sensor();
        physical.set_availability(Availability::Available);
        add_device(&store, &physical.description());
        registry.insert(physical.clone());
        physical.functions()[0]
            .observe_measurement(21.5, Some(1_700_000_100), 1_700_000_110)
            .unwrap();
        physical.functions()[1]
            .observe_measurement(45.0, None, 1_700_000_120)
            .unwrap();

        let list = handle_line(&registry, &store.devices(), &status, "status sensor", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(
            list.contains("temperature (Temperature): 21.50 °C"),
            "{list}"
        );
        assert!(list.contains("humidity (Humidity): 45.00 %"), "{list}");
        assert!(list.contains("upstream time 1700000100"), "{list}");
        assert!(list.contains("upstream time unknown"), "{list}");
        assert!(list.contains("received at"), "{list}");
        assert!(!list.contains("power"), "{list}");
        assert!(!list.contains("unknown\n  humidity"), "{list}");

        let result = terminal_command(&physical, "on").await.unwrap();
        assert!(result.contains("does not support"), "{result}");
        for line in ["off sensor temperature", "off sensor humidity"] {
            let result = handle_line(&registry, &store.devices(), &status, line, 0)
                .await
                .unwrap()
                .unwrap();
            assert!(result.contains("does not support"), "{line}: {result}");
        }
    });
}

#[test]
fn a_sensor_never_creates_a_native_write_request() {
    block_on(async {
        let physical = sensor();
        physical.set_availability(Availability::Available);
        let result = terminal_command(&physical, "on").await.unwrap();
        assert!(result.contains("does not support"));
        // There is no consumer for a read-only function at all.
        let (_, receivers) = PhysicalDevice::new(physical.description());
        assert!(receivers.is_empty());
    });
}

#[test]
fn a_supplied_source_time_is_displayed_without_rfc3339_assumptions() {
    let text = measurement_text(
        crate::device::MeasurementSnapshot {
            value: Some(21.5),
            source_time: Some(i64::MAX),
            received_at: Some(1_700_000_000),
        },
        "°C",
    );
    assert!(text.contains("upstream time 9223372036854775807"), "{text}");
    assert!(text.contains("received at"), "{text}");
}

#[test]
fn status_shows_shared_battery_value_and_separate_times() {
    block_on(async {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let report = AuthService::new(store.xiaomi(), CloudClient::new().unwrap())
            .local_status()
            .unwrap();
        let status = AuthStatus::new(report, "/bin/migate".into(), directory.path().into());
        let registry = DeviceRegistry::new();
        let physical = sensor_with_battery(true);
        physical.set_availability(Availability::Available);
        add_device(&store, &physical.description());
        registry.insert(physical.clone());
        physical
            .observe_battery(55.0, Some(1_700_000_600), 1_700_000_610)
            .unwrap();
        physical.functions()[0]
            .observe_measurement(21.5, Some(1_700_000_100), 1_700_000_110)
            .unwrap();

        let list = handle_line(&registry, &store.devices(), &status, "status sensor", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(list.contains("battery (shared): 55.00 %"), "{list}");
        assert!(list.contains("upstream time 1700000600"), "{list}");
        assert!(list.contains("received at"), "{list}");
        // The battery's times are independent of the temperature observation.
        assert!(
            list.contains("temperature (Temperature): 21.50 °C"),
            "{list}"
        );
        assert!(!list.contains("battery (Battery)"), "{list}");
    });
}

#[test]
fn co2_terminal_status_shows_native_ppm_and_independent_times() {
    block_on(async {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let report = AuthService::new(store.xiaomi(), CloudClient::new().unwrap())
            .local_status()
            .unwrap();
        let status = AuthStatus::new(report, "/bin/migate".into(), directory.path().into());
        let registry = DeviceRegistry::new();
        let (physical, receivers) = PhysicalDevice::new(
            PhysicalDeviceDescription::new(
                DeviceKey::new("xiaomi", "co2"),
                "Air",
                vec![FunctionDescription {
                    id: "co2".into(),
                    label: "CO₂".into(),
                    kind: FunctionKind::CarbonDioxide(
                        MeasurementRange::new(400.0, 5000.0).unwrap(),
                    ),
                }],
            )
            .unwrap(),
        );
        assert!(receivers.is_empty());
        add_device(&store, &physical.description());
        registry.insert(physical.clone());
        physical.functions()[0]
            .observe_measurement(650.125, Some(100), 200)
            .unwrap();
        let output = handle_line(&registry, &store.devices(), &status, "status co2", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(output.contains("co2 (CO₂): 650.12 ppm"), "{output}");
        assert!(output.contains("upstream time 100"), "{output}");
        assert!(output.contains("received at"), "{output}");
        assert!(
            terminal_command(&physical, "on")
                .await
                .unwrap()
                .contains("does not support")
        );
    });
}
