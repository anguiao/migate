use super::*;
use crate::device::{Capability, NumericRange};

fn light(capabilities: Vec<Capability>) -> FunctionDescription {
    FunctionDescription {
        id: "main".into(),
        label: "主灯".into(),
        category: Category::Light,
        capabilities,
    }
}

#[test]
fn equivalent_kelvin_encodings_share_one_shape() {
    let first = light(vec![
        Capability::OnOff,
        Capability::Brightness,
        Capability::ColorTemperature(NumericRange {
            min: 2700,
            max: 6500,
            step: 1,
        }),
    ]);
    let second = light(vec![
        Capability::OnOff,
        Capability::Brightness,
        Capability::ColorTemperature(NumericRange {
            min: 2700,
            max: 6500,
            step: 100,
        }),
    ]);
    assert_eq!(fixed_shape(&[first]), fixed_shape(&[second]));
}

#[test]
fn profile_range_and_outlet_order_change_the_shape() {
    let on_off = light(vec![Capability::OnOff]);
    let dimmable = light(vec![Capability::OnOff, Capability::Brightness]);
    assert_ne!(
        fixed_shape(std::slice::from_ref(&on_off)),
        fixed_shape(&[dimmable])
    );

    let narrow = light(vec![
        Capability::OnOff,
        Capability::Brightness,
        Capability::ColorTemperature(NumericRange {
            min: 3000,
            max: 6500,
            step: 1,
        }),
    ]);
    let wide = light(vec![
        Capability::OnOff,
        Capability::Brightness,
        Capability::ColorTemperature(NumericRange {
            min: 2000,
            max: 6500,
            step: 1,
        }),
    ]);
    assert_ne!(fixed_shape(&[narrow]), fixed_shape(&[wide]));

    let outlet = |id: &str| FunctionDescription {
        id: id.into(),
        label: id.into(),
        category: Category::Outlet,
        capabilities: vec![Capability::OnOff],
    };
    let first = fixed_shape(&[outlet("a"), outlet("b")]);
    let second = fixed_shape(&[outlet("b"), outlet("a")]);
    assert_ne!(first, second);

    // Child labels are part of a multi-function Descriptor TagList, while
    // a single function's label is not part of the fixed surface.
    let mut renamed = outlet("a");
    renamed.label = "Renamed".into();
    assert_ne!(
        fixed_shape(&[outlet("a"), outlet("b")]),
        fixed_shape(&[renamed, outlet("b")])
    );
    let mut light_renamed = light(vec![Capability::OnOff]);
    light_renamed.label = "Renamed".into();
    assert_eq!(
        fixed_shape(&[light(vec![Capability::OnOff])]),
        fixed_shape(&[light_renamed])
    );
}
