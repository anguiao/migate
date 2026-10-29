use super::*;
use crate::device::FanValue;

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
                },
            }],
        )
        .unwrap(),
    );
    let function = &device.functions()[0];
    let initial = function.snapshot();
    let fan = initial.fan().unwrap();
    assert_eq!(fan.power.value, None);
    assert_eq!(fan.mode.value, None);
    assert_eq!(fan.speed.value, None);
    assert_eq!(fan.swing.value, None);
    function.observe_fan(FanValue::Speed(40), Some(1), 2);
    let observed = function.snapshot();
    let fan = observed.fan().unwrap();
    assert_eq!(fan.speed.value, Some(40));
    assert_eq!(fan.speed.source_time, Some(1));
    assert_eq!(fan.power.value, None);
    assert_eq!(fan.mode.value, None);
    assert_eq!(fan.swing.value, None);
    function.observe_fan(FanValue::Speed(40), Some(3), 4);
    let repeated = function.snapshot();
    assert_eq!(repeated.revision, observed.revision);
    assert_eq!(repeated.fan().unwrap().speed.source_time, Some(3));
    assert_eq!(repeated.fan().unwrap().speed.received_at, Some(4));
    function.observe_fan(FanValue::Power(false), Some(5), 6);
    function.observe_fan(FanValue::Mode(1), Some(7), 8);
    function.observe_fan(FanValue::Swing(true), Some(9), 10);
    let fan = function.snapshot().fan().unwrap();
    assert_eq!(
        (
            fan.power.value,
            fan.mode.value,
            fan.speed.value,
            fan.swing.value
        ),
        (Some(false), Some(1), Some(40), Some(true))
    );
}
