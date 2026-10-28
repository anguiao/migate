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

#[test]
fn curtain_observations_update_each_value_and_only_value_changes_advance_revision() {
    use crate::device::{CurtainMotion, CurtainValue};

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
    let initial = function.snapshot();
    function
        .observe_curtain(CurtainValue::CurrentPosition(37), Some(11), 12)
        .unwrap();
    let current = function.snapshot();
    assert_eq!(current.revision, initial.revision + 1);
    assert_eq!(current.curtain().unwrap().current_position.value, Some(37));
    assert_eq!(current.curtain().unwrap().target_position.value, None);
    function
        .observe_curtain(CurtainValue::CurrentPosition(37), None, 13)
        .unwrap();
    let repeated = function.snapshot();
    assert_eq!(repeated.revision, current.revision);
    assert_eq!(
        repeated.curtain().unwrap().current_position.source_time,
        None
    );
    assert_eq!(
        repeated.curtain().unwrap().current_position.received_at,
        Some(13)
    );
    function
        .observe_curtain(CurtainValue::TargetPosition(80), Some(14), 15)
        .unwrap();
    function
        .observe_curtain(CurtainValue::Status(CurtainMotion::Opening), Some(16), 17)
        .unwrap();
    let values = function.snapshot().curtain().unwrap();
    assert_eq!(values.current_position.value, Some(37));
    assert_eq!(values.target_position.value, Some(80));
    assert_eq!(values.status.value, Some(CurtainMotion::Opening));
}
