use crate::device::{
    CurtainObservation, DeviceKey, FunctionDescription, FunctionKind, PhysicalDevice,
    PhysicalDeviceDescription,
};

#[test]
fn curtain_ignores_power_observations_without_publishing_a_change() {
    let description = PhysicalDeviceDescription::new(
        DeviceKey::new("xiaomi", "curtain"),
        "Curtain",
        vec![FunctionDescription {
            id: "main".into(),
            label: "Curtain".into(),
            kind: FunctionKind::Curtain,
        }],
    )
    .unwrap();
    let (device, _) = PhysicalDevice::new(description);
    let function = &device.functions()[0];
    let original = function.snapshot();
    let curtain = original.curtain().unwrap();
    let unknown = CurtainObservation {
        value: None,
        source_time: None,
        received_at: None,
    };
    assert_eq!(curtain.current_position, unknown);
    assert_eq!(curtain.target_position, unknown);
    assert_eq!(curtain.status.value, None);
    assert_eq!(curtain.status.source_time, None);
    assert_eq!(curtain.status.received_at, None);
    function.observe(true);
    assert_eq!(function.snapshot(), original);
}
