use crate::device::{
    AmbientTemperature as Ambient, DeviceKey, FunctionDescription, FunctionKey, FunctionKind,
    MeasurementRange, MeasurementSnapshot, PhysicalDevice, PhysicalDeviceDescription,
    TemperatureGrid,
};
use futures_lite::future::{block_on, poll_once};

fn devices() -> (PhysicalDevice, PhysicalDevice) {
    let combined = PhysicalDeviceDescription::new(
        DeviceKey::new("xiaomi", "combined"),
        "Combined",
        vec![
            FunctionDescription {
                id: "air-conditioner:3".into(),
                label: "AC".into(),
                kind: FunctionKind::AirConditioner {
                    temperature: TemperatureGrid::new(16, 30, 1).unwrap(),
                    vertical_swing: true,
                },
            },
            FunctionDescription {
                id: "temperature:3:7".into(),
                label: "Temperature".into(),
                kind: FunctionKind::Temperature(MeasurementRange::new(-30.0, 100.0).unwrap()),
            },
        ],
    )
    .unwrap();
    let other = PhysicalDeviceDescription::new(
        DeviceKey::new("xiaomi", "other"),
        "Other",
        vec![FunctionDescription {
            id: "temperature:3:7".into(),
            label: "Temperature".into(),
            kind: FunctionKind::Temperature(MeasurementRange::new(-30.0, 100.0).unwrap()),
        }],
    )
    .unwrap();
    (
        PhysicalDevice::new(combined).0,
        PhysicalDevice::new(other).0,
    )
}

#[test]
fn subscriptions_follow_validity_readings_and_normal_source_switches() {
    block_on(async {
        let (combined, other) = devices();
        let ac = combined.function("air-conditioner:3").unwrap();
        let sensor = combined.function("temperature:3:7").unwrap();
        let second = other.function("temperature:3:7").unwrap();
        let mut changes = ac.subscribe();
        assert_eq!(ac.snapshot().ambient_temperature(), Some(Ambient::Unbound));
        let key = sensor.function_key().clone();
        ac.set_ambient_temperature_source(Some((key.clone(), Some(sensor.clone()))));
        assert_eq!(
            changes.changed().await.ambient_temperature(),
            Some(Ambient::SourceUnavailable)
        );
        sensor.set_temperature_source_available(true);
        assert_eq!(
            changes.changed().await.ambient_temperature(),
            Some(Ambient::AwaitingReading)
        );
        sensor.observe_measurement(0.0, Some(5), 10).unwrap();
        let first = MeasurementSnapshot {
            value: Some(0.0),
            source_time: Some(5),
            received_at: Some(10),
        };
        assert_eq!(
            changes.changed().await.ambient_temperature(),
            Some(Ambient::Available(first))
        );
        sensor.set_temperature_source_available(false);
        assert_eq!(
            changes.changed().await.ambient_temperature(),
            Some(Ambient::SourceUnavailable)
        );
        sensor.set_temperature_source_available(true);
        assert_eq!(
            changes.changed().await.ambient_temperature(),
            Some(Ambient::AwaitingReading)
        );
        sensor.observe_measurement(0.0, Some(5), 10).unwrap();
        assert_eq!(
            changes.changed().await.ambient_temperature(),
            Some(Ambient::Available(first))
        );

        second.set_temperature_source_available(true);
        second.observe_measurement(23.0, Some(7), 11).unwrap();
        ac.set_ambient_temperature_source(Some((
            second.function_key().clone(),
            Some(second.clone()),
        )));
        assert!(
            matches!(changes.changed().await.ambient_temperature(), Some(Ambient::Available(value)) if value.value == Some(23.0))
        );
        ac.set_ambient_temperature_source(Some((second.function_key().clone(), Some(second))));
        assert!(poll_once(changes.changed()).await.is_none());
        ac.set_ambient_temperature_source(None);
        assert_eq!(
            changes.changed().await.ambient_temperature(),
            Some(Ambient::Unbound)
        );
        ac.set_ambient_temperature_source(Some((
            FunctionKey::new(DeviceKey::new("xiaomi", "missing"), "temperature:3:7"),
            None,
        )));
        assert_eq!(
            changes.changed().await.ambient_temperature(),
            Some(Ambient::SourceMissing)
        );
        assert_eq!(
            ac.snapshot().air_conditioner().unwrap().temperature.value,
            None
        );
        assert!(ac.pending_air_conditioner_commands().is_empty());
    });
}
