use super::*;
use crate::device::{
    DeviceKey, FunctionDescription, FunctionKind, LightProfile, PhysicalDevice,
    PhysicalDeviceDescription, Power, TemperatureInterval,
};

mod support;

#[test]
fn projected_temperature_is_applied_atomically_without_changing_other_observations() {
    let interval = TemperatureInterval::new(2700, 6500).unwrap();
    let description = PhysicalDeviceDescription::new(
        DeviceKey::new("test", "light"),
        "Light",
        vec![FunctionDescription {
            id: "main".into(),
            label: "Main".into(),
            kind: FunctionKind::Light(LightProfile::ColorTemperature(interval)),
        }],
    )
    .unwrap();
    let (device, _receivers) = PhysicalDevice::new(description);
    let function = &device.functions()[0];
    function
        .observe_light(LightPatch {
            power: Some(true),
            brightness_percent: Some(55.0),
            temperature_kelvin: Some(3000),
            ..Default::default()
        })
        .unwrap();
    let before = function.snapshot();
    let observation = function
        .project_light_observation(LightPatch {
            temperature_kelvin: Some(7000),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        function.snapshot(),
        before,
        "preparation must not publish state"
    );
    let after = observation.apply();
    let lighting = after.lighting().unwrap();
    assert_eq!(after.power(), Some(Power::On));
    assert_eq!(lighting.brightness_percent, Some(55.0));
    assert_eq!(lighting.temperature_kelvin, None);
    assert_eq!(lighting.mode, Some(LightMode::Temperature));
    assert_eq!(after.revision, before.revision + 1);
}
