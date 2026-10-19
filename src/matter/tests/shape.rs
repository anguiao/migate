use super::*;
use crate::device::{LightProfile, TemperatureInterval};

fn light(profile: LightProfile) -> FunctionDescription {
    FunctionDescription {
        id: "main".into(),
        label: "主灯".into(),
        kind: FunctionKind::Light(profile),
    }
}

#[test]
fn equivalent_kelvin_encodings_share_one_shape() {
    let first = light(LightProfile::ColorTemperature(
        TemperatureInterval::new(2700, 6500).unwrap(),
    ));
    let second = light(LightProfile::ColorTemperature(
        TemperatureInterval::new(2701, 6501).unwrap(),
    ));
    assert_eq!(fixed_shape(&[first]), fixed_shape(&[second]));
}

#[test]
fn profile_range_and_outlet_order_change_the_shape() {
    let on_off = light(LightProfile::OnOff);
    let dimmable = light(LightProfile::Dimmable);
    assert_ne!(
        fixed_shape(std::slice::from_ref(&on_off)),
        fixed_shape(&[dimmable])
    );

    let narrow = light(LightProfile::ColorTemperature(
        TemperatureInterval::new(3000, 6500).unwrap(),
    ));
    let wide = light(LightProfile::ColorTemperature(
        TemperatureInterval::new(2000, 6500).unwrap(),
    ));
    assert_ne!(fixed_shape(&[narrow]), fixed_shape(&[wide]));

    let outlet = |id: &str| FunctionDescription {
        id: id.into(),
        label: id.into(),
        kind: FunctionKind::Outlet,
    };
    let first = fixed_shape(&[outlet("a"), outlet("b")]);
    let second = fixed_shape(&[outlet("b"), outlet("a")]);
    assert_ne!(first, second);

    // Only multi-function child labels are part of the Descriptor TagList.
    let mut renamed = outlet("a");
    renamed.label = "Renamed".into();
    assert_ne!(
        fixed_shape(&[outlet("a"), outlet("b")]),
        fixed_shape(&[renamed, outlet("b")])
    );
    let mut light_renamed = light(LightProfile::OnOff);
    light_renamed.label = "Renamed".into();
    assert_eq!(
        fixed_shape(&[light(LightProfile::OnOff)]),
        fixed_shape(&[light_renamed])
    );
}
