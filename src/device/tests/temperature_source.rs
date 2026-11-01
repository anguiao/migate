use crate::device::{
    DeviceKey, FunctionDescription, FunctionKind, MeasurementRange, MeasurementSnapshot,
    PhysicalDevice, PhysicalDeviceDescription, TemperatureSourceState,
};
use futures_lite::future::{block_on, poll_once};

fn sensor() -> PhysicalDevice {
    let description = PhysicalDeviceDescription::new(
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
    .unwrap();
    PhysicalDevice::new(description).0
}

#[test]
fn temperature_source_requires_a_new_observation_after_losing_all_paths() {
    block_on(async {
        let physical = sensor();
        let temperature = physical.function("temperature").unwrap();
        let humidity = physical.function("humidity").unwrap();
        let mut changes = temperature.subscribe();
        assert_eq!(
            temperature.snapshot().temperature_source(),
            Some(TemperatureSourceState::Unavailable)
        );
        assert_eq!(humidity.snapshot().temperature_source(), None);

        temperature.set_temperature_source_available(true);
        assert_eq!(
            changes.changed().await.temperature_source(),
            Some(TemperatureSourceState::AwaitingReading)
        );
        temperature.set_temperature_source_available(true);
        assert!(poll_once(changes.changed()).await.is_none());

        let reading = MeasurementSnapshot {
            value: Some(0.0),
            source_time: Some(100),
            received_at: Some(200),
        };
        temperature
            .observe_measurement(0.0, Some(100), 200)
            .unwrap();
        assert_eq!(
            changes.changed().await.temperature_source(),
            Some(TemperatureSourceState::Available(reading))
        );

        temperature.set_temperature_source_available(false);
        let unavailable = changes.changed().await;
        assert_eq!(
            unavailable.temperature_source(),
            Some(TemperatureSourceState::Unavailable)
        );
        assert_eq!(unavailable.measurement(), Some(reading));
        temperature.set_temperature_source_available(true);
        let restored = changes.changed().await;
        assert_eq!(
            restored.temperature_source(),
            Some(TemperatureSourceState::AwaitingReading)
        );
        assert_eq!(restored.measurement(), Some(reading));

        // Even an identical new observation restores current validity and notifies listeners.
        temperature
            .observe_measurement(0.0, Some(100), 200)
            .unwrap();
        assert_eq!(
            changes.changed().await.temperature_source(),
            Some(TemperatureSourceState::Available(reading))
        );
        physical.close();
        assert_eq!(
            changes.changed().await.temperature_source(),
            Some(TemperatureSourceState::Unavailable)
        );
        assert_eq!(temperature.snapshot().measurement(), Some(reading));
    });
}
