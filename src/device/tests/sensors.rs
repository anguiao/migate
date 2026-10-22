//! Read-only temperature and humidity state at the device boundary.

use crate::device::{
    Availability, Command, CommandError, DeviceKey, FunctionDescription, FunctionKind,
    FunctionSnapshot, MeasurementRange, MeasurementSnapshot, PhysicalDevice,
    PhysicalDeviceDescription,
};
use futures_lite::future::block_on;

fn sensor_description() -> PhysicalDeviceDescription {
    PhysicalDeviceDescription::new(
        DeviceKey::new("xiaomi", "sensor"),
        "Sensor",
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
    .unwrap()
}

fn sensor() -> PhysicalDevice {
    let (physical, receivers) = PhysicalDevice::new(sensor_description());
    assert!(receivers.is_empty(), "sensors have no write queue");
    physical
}

#[test]
fn sensors_start_unknown_without_fabricated_power_or_commands() {
    let physical = sensor();
    for function in physical.functions() {
        let snapshot = function.snapshot();
        assert_eq!(snapshot.power(), None);
        assert_eq!(snapshot.lighting(), None);
        assert_eq!(
            snapshot.measurement(),
            Some(MeasurementSnapshot {
                value: None,
                source_time: None,
                received_at: None,
            })
        );
        assert_eq!(snapshot.availability(), Availability::Initializing);
        assert_eq!(snapshot.revision(), 0);
    }
    assert!(matches!(
        block_on(physical.functions()[0].execute(Command::On)),
        Err(CommandError::Unsupported)
    ));
}

#[test]
fn temperature_and_humidity_update_independently_with_source_and_reception_times() {
    let physical = sensor();
    physical.set_availability(Availability::Available);
    let temperature = &physical.functions()[0];
    let humidity = &physical.functions()[1];

    temperature
        .observe_measurement(21.5, Some(1_700_000_000), 1_700_000_010)
        .unwrap();
    let snapshot = temperature.snapshot();
    assert!(matches!(snapshot, FunctionSnapshot::Temperature { .. }));
    assert_eq!(
        snapshot.measurement(),
        Some(MeasurementSnapshot {
            value: Some(21.5),
            source_time: Some(1_700_000_000),
            received_at: Some(1_700_000_010),
        })
    );
    assert_eq!(humidity.snapshot().measurement().unwrap().value, None);

    humidity
        .observe_measurement(45.0, None, 1_700_000_020)
        .unwrap();
    let snapshot = humidity.snapshot();
    assert!(matches!(snapshot, FunctionSnapshot::Humidity { .. }));
    assert_eq!(
        snapshot.measurement(),
        Some(MeasurementSnapshot {
            value: Some(45.0),
            source_time: None,
            received_at: Some(1_700_000_020),
        })
    );
    assert_eq!(
        temperature.snapshot().measurement().unwrap(),
        MeasurementSnapshot {
            value: Some(21.5),
            source_time: Some(1_700_000_000),
            received_at: Some(1_700_000_010),
        }
    );
}

#[test]
fn repeated_same_measurement_does_not_change_the_value_or_revision_without_timestamps() {
    let physical = sensor();
    let temperature = &physical.functions()[0];
    temperature
        .observe_measurement(21.5, Some(100), 200)
        .unwrap();
    let before = temperature.snapshot();
    temperature
        .observe_measurement(21.5, Some(100), 200)
        .unwrap();
    assert_eq!(temperature.snapshot(), before);
    // A newer reception is diagnostic and changes the snapshot, while the
    // projected value stays the same for Matter reporting.
    temperature
        .observe_measurement(21.5, Some(100), 300)
        .unwrap();
    let after = temperature.snapshot();
    assert_eq!(after.measurement().unwrap().value, Some(21.5));
    assert_eq!(after.measurement().unwrap().received_at, Some(300));
    assert_eq!(after.measurement().unwrap().source_time, Some(100));
    assert!(after.revision() > before.revision());
}

#[test]
fn a_new_value_without_source_time_does_not_inherit_the_previous_source_time() {
    let physical = sensor();
    let temperature = &physical.functions()[0];
    temperature
        .observe_measurement(21.5, Some(100), 200)
        .unwrap();
    temperature.observe_measurement(22.0, None, 300).unwrap();
    assert_eq!(
        temperature.snapshot().measurement(),
        Some(MeasurementSnapshot {
            value: Some(22.0),
            source_time: None,
            received_at: Some(300),
        })
    );
}

#[test]
fn invalid_measurements_keep_the_last_value() {
    let physical = sensor();
    let temperature = &physical.functions()[0];
    temperature
        .observe_measurement(21.5, Some(100), 200)
        .unwrap();
    let before = temperature.snapshot();
    for (value, error) in [
        (f64::NAN, CommandError::Rejected("measurement".into())),
        (f64::INFINITY, CommandError::Rejected("measurement".into())),
        (100.1, CommandError::Rejected("measurement".into())),
        (-30.1, CommandError::Rejected("measurement".into())),
    ] {
        assert_eq!(
            temperature.observe_measurement(value, Some(999), 999),
            Err(error)
        );
        assert_eq!(temperature.snapshot(), before);
    }

    assert_eq!(
        block_on(physical.functions()[0].execute(Command::Off)),
        Err(CommandError::Unsupported)
    );
}

#[test]
fn silence_availability_changes_and_loss_of_all_paths_keep_measurements() {
    let physical = sensor();
    let temperature = &physical.functions()[0];
    temperature
        .observe_measurement(21.5, Some(100), 200)
        .unwrap();
    physical.set_availability(Availability::Available);
    physical.set_availability(Availability::Unavailable("no read path".into()));
    let snapshot = temperature.snapshot();
    assert_eq!(snapshot.measurement().unwrap().value, Some(21.5));
    assert_eq!(
        snapshot.availability(),
        Availability::Unavailable("no read path".into())
    );
    // A long silence changes nothing at all.
    let before = temperature.snapshot();
    assert_eq!(temperature.snapshot(), before);
    physical.set_availability(Availability::Available);
    assert_eq!(
        temperature.snapshot().measurement().unwrap().value,
        Some(21.5)
    );
}

fn battery_sensor() -> PhysicalDevice {
    let (physical, receivers) = PhysicalDevice::new(sensor_description().with_battery());
    assert!(receivers.is_empty());
    physical
}

#[test]
fn battery_capability_is_shared_and_starts_unknown_without_a_third_function() {
    let physical = battery_sensor();
    assert_eq!(physical.functions().len(), 2);
    assert_eq!(
        physical.battery_snapshot(),
        Some(crate::device::BatterySnapshot {
            value: None,
            source_time: None,
            received_at: None,
        })
    );
    assert!(
        physical
            .functions()
            .iter()
            .all(|function| { function.snapshot().measurement().unwrap().value.is_none() })
    );
    let plain = sensor();
    assert_eq!(plain.battery_snapshot(), None);
    assert_eq!(
        plain.observe_battery(50.0, Some(1), 2),
        Err(CommandError::Unsupported)
    );
}

#[test]
fn battery_values_update_independently_with_their_own_times() {
    let physical = battery_sensor();
    let temperature = &physical.functions()[0];
    let battery_before = temperature.snapshot().revision();
    physical
        .observe_battery(45.0, Some(1_700_000_000), 1_700_000_010)
        .unwrap();
    temperature
        .observe_measurement(21.5, Some(1_700_000_001), 1_700_000_011)
        .unwrap();
    assert_eq!(
        physical.battery_snapshot(),
        Some(crate::device::BatterySnapshot {
            value: Some(45.0),
            source_time: Some(1_700_000_000),
            received_at: Some(1_700_000_010),
        })
    );
    assert_eq!(
        temperature.snapshot().measurement().unwrap().value,
        Some(21.5)
    );
    assert!(temperature.snapshot().revision() > battery_before);
    // A battery-only update never invented a temperature or humidity value.
    assert_eq!(
        physical.functions()[1]
            .snapshot()
            .measurement()
            .unwrap()
            .value,
        None
    );
}

#[test]
fn invalid_battery_values_keep_the_last_value_and_survive_availability_loss() {
    let physical = battery_sensor();
    physical.observe_battery(45.0, Some(100), 200).unwrap();
    let before = physical.battery_snapshot();
    for value in [f64::NAN, f64::INFINITY, -0.1, 100.1] {
        assert_eq!(
            physical.observe_battery(value, Some(999), 999),
            Err(CommandError::Rejected("battery".into()))
        );
        assert_eq!(physical.battery_snapshot(), before);
    }
    physical.set_availability(Availability::Unavailable("no read path".into()));
    assert_eq!(physical.battery_snapshot(), before);
    assert_eq!(
        physical.functions()[0]
            .snapshot()
            .measurement()
            .unwrap()
            .value,
        None
    );
}

#[test]
fn timestamp_only_battery_updates_keep_the_value_without_creating_measurements() {
    let physical = battery_sensor();
    physical.observe_battery(50.0, Some(100), 200).unwrap();
    physical.observe_battery(50.0, Some(101), 201).unwrap();
    assert_eq!(
        physical.battery_snapshot(),
        Some(crate::device::BatterySnapshot {
            value: Some(50.0),
            source_time: Some(101),
            received_at: Some(201),
        })
    );
    assert!(
        physical
            .functions()
            .iter()
            .all(|function| { function.snapshot().measurement().unwrap().value.is_none() })
    );
}

#[test]
fn multiple_physical_devices_keep_isolated_battery_state() {
    let first = battery_sensor();
    let second = battery_sensor();
    first.observe_battery(10.0, Some(1), 2).unwrap();
    second.observe_battery(90.0, Some(3), 4).unwrap();
    assert_eq!(first.battery_snapshot().unwrap().value, Some(10.0));
    assert_eq!(second.battery_snapshot().unwrap().value, Some(90.0));
}
