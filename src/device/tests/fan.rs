use super::*;
use crate::device::{FanCommand, FanValue};

#[test]
fn fan_observations_start_unknown_and_update_independently() {
    let (device, _) = PhysicalDevice::new(
        PhysicalDeviceDescription::new(
            DeviceKey::new("xiaomi", "fan"),
            "Fan",
            vec![FunctionDescription {
                id: "main".into(),
                label: "Fan".into(),
                kind: FunctionKind::Fan {
                    horizontal_swing: true,
                    speed_count: 100,
                },
            }],
        )
        .unwrap(),
    );
    let function = &device.functions()[0];
    let initial = function.snapshot();
    let fan = initial.fan().unwrap();
    assert_eq!(fan.power.value, None);
    assert_eq!(fan.speed.value, None);
    assert_eq!(fan.swing.value, None);
    function.observe_fan(FanValue::Speed(40), Some(1), 2);
    let observed = function.snapshot();
    let fan = observed.fan().unwrap();
    assert_eq!(fan.speed.value, Some(40));
    assert_eq!(fan.speed.source_time, Some(1));
    assert_eq!(fan.power.value, None);
    assert_eq!(fan.swing.value, None);
    function.observe_fan(FanValue::Speed(40), Some(3), 4);
    let repeated = function.snapshot();
    assert_eq!(repeated.revision, observed.revision);
    assert_eq!(repeated.fan().unwrap().speed.source_time, Some(3));
    assert_eq!(repeated.fan().unwrap().speed.received_at, Some(4));
    function.observe_fan(FanValue::Power(false), Some(5), 6);
    function.observe_fan(FanValue::Swing(true), Some(9), 10);
    let fan = function.snapshot().fan().unwrap();
    assert_eq!(
        (fan.power.value, fan.speed.value, fan.swing.value),
        (Some(false), Some(40), Some(true))
    );
}

#[test]
fn fan_requires_valid_level_count_and_rejects_out_of_range_commands() {
    for count in [0, 101] {
        let result = PhysicalDeviceDescription::new(
            DeviceKey::new("test", "fan"),
            "Fan",
            vec![FunctionDescription {
                id: "main".into(),
                label: "Fan".into(),
                kind: FunctionKind::Fan {
                    horizontal_swing: false,
                    speed_count: count,
                },
            }],
        );
        assert!(result.is_err());
    }
    let description = PhysicalDeviceDescription::new(
        DeviceKey::new("test", "fan"),
        "Fan",
        vec![FunctionDescription {
            id: "main".into(),
            label: "Fan".into(),
            kind: FunctionKind::Fan {
                horizontal_swing: false,
                speed_count: 3,
            },
        }],
    )
    .unwrap();
    let (device, _receivers) = PhysicalDevice::new(description);
    device.set_availability(Availability::Available);
    let function = &device.functions()[0];
    assert!(function.begin_fan(FanCommand::Speed(4)).is_err());
    assert!(function.begin_fan(FanCommand::Speed(3)).is_ok());
}
