use super::*;
use crate::device::{LightProfile, TemperatureInterval};

fn fixed_shape(functions: &[FunctionDescription]) -> FixedShape {
    fixed_shape_with_battery(functions.iter(), false)
}

#[test]
fn curtain_has_a_distinct_stable_single_control_shape() {
    let curtain = FunctionDescription {
        id: "main".into(),
        label: "窗帘".into(),
        kind: FunctionKind::Curtain,
    };
    let mut renamed = curtain.clone();
    renamed.label = "Bedroom".into();
    let outlet = FunctionDescription {
        kind: FunctionKind::Outlet,
        ..curtain.clone()
    };
    assert_eq!(
        fixed_shape(std::slice::from_ref(&curtain)),
        fixed_shape(&[renamed])
    );
    assert_ne!(fixed_shape(&[curtain]), fixed_shape(&[outlet]));
}

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

#[test]
fn battery_capability_is_part_of_the_fixed_physical_shape() {
    let temperature = FunctionDescription {
        id: "temperature".into(),
        label: "Temperature".into(),
        kind: FunctionKind::Temperature(
            crate::device::MeasurementRange::new(-30.0, 100.0).unwrap(),
        ),
    };
    let humidity = FunctionDescription {
        id: "humidity".into(),
        label: "Humidity".into(),
        kind: FunctionKind::Humidity(crate::device::MeasurementRange::new(0.0, 100.0).unwrap()),
    };
    let functions = [temperature, humidity];
    assert_ne!(
        fixed_shape(&functions),
        fixed_shape_with_battery(functions.iter(), true)
    );
}

#[test]
fn co2_fixed_shape_compares_the_published_single_precision_bounds() {
    let measurement = |max| FunctionDescription {
        id: "co2".into(),
        label: "CO₂".into(),
        kind: FunctionKind::CarbonDioxide(
            crate::device::MeasurementRange::new(400.0, max).unwrap(),
        ),
    };
    assert_eq!(
        fixed_shape(&[measurement(5000.0)]),
        fixed_shape(&[measurement(5000.000001)])
    );
    assert_ne!(
        fixed_shape(&[measurement(5000.0)]),
        fixed_shape(&[measurement(6000.0)])
    );
}

#[test]
fn fan_shape_tracks_optional_horizontal_swing() {
    let fan = FunctionDescription {
        id: "main".into(),
        label: "Fan".into(),
        kind: FunctionKind::Fan {
            horizontal_swing: true,
        },
    };
    let without_swing = FunctionDescription {
        kind: FunctionKind::Fan {
            horizontal_swing: false,
        },
        ..fan.clone()
    };
    let outlet = FunctionDescription {
        kind: FunctionKind::Outlet,
        ..fan.clone()
    };
    assert_ne!(
        fixed_shape(std::slice::from_ref(&fan)),
        fixed_shape(&[without_swing])
    );
    assert_ne!(fixed_shape(&[fan]), fixed_shape(&[outlet]));
}
