use super::super::group::parse_group;
use super::*;
use crate::device::{DeviceKey, FunctionKind, LightProfile, TemperatureInterval};
use serde_json::Value;

fn instance() -> Value {
    serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/group3-instance.json"
    ))
    .unwrap()
}

#[test]
fn parses_real_group3_native_light_profile() {
    let parsed = parse_group(
        DeviceKey::new("xiaomi", "group-x"),
        "mijia.light.group3",
        &instance(),
    )
    .unwrap();
    assert_eq!(parsed.functions().len(), 1);
    let function = &parsed.functions()[0];
    assert_eq!(function.id(), "light:2");
    assert_eq!(
        function.kind(),
        FunctionKind::Light(LightProfile::ColorTemperature(
            TemperatureInterval::new(1700, 7000).unwrap()
        ))
    );
    assert_eq!(
        function.power().unwrap(),
        PropertyBinding { siid: 2, piid: 1 }
    );
    let lighting = function.lighting().unwrap();
    assert_eq!(
        lighting.brightness().unwrap().property(),
        PropertyBinding { siid: 2, piid: 2 }
    );
    assert_eq!(
        lighting.temperature().unwrap().property(),
        PropertyBinding { siid: 2, piid: 3 }
    );
    assert!(lighting.color().is_none());
    assert!(
        lighting.mode().is_none(),
        "a group mode value cannot create a color capability"
    );
    assert!(parsed.refresh_trigger().is_none());
    let recognition = parsed.recognition().expect("group recognition metadata");
    assert_eq!(recognition.category(), "light");
    assert_eq!(recognition.core(), ["light:2"]);
}

#[test]
fn rejects_a_group_instance_that_is_not_a_standard_light() {
    let outlet: Value =
        serde_json::from_str(include_str!("../../../../tests/fixtures/miot/outlet.json")).unwrap();
    assert!(matches!(
        parse_group(DeviceKey::new("xiaomi", "group-x"), "group", &outlet),
        Err(CapabilityError::InvalidGroup(_))
    ));
}
