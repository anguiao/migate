use super::*;
use crate::{
    device::{Capability, FunctionDescription, NumericRange},
    matter::publication::{LightDeviceType, UnmappableReason, map_light},
};

const CT_RANGE: NumericRange = NumericRange {
    min: 2700,
    max: 6500,
    step: 1,
};

fn light(capabilities: Vec<Capability>) -> FunctionDescription {
    FunctionDescription {
        id: "main".into(),
        label: "Light".into(),
        category: Category::Light,
        capabilities,
    }
}

#[test]
fn every_real_on_off_combination_selects_its_published_surface() {
    for (brightness, temperature, rgb, expected) in [
        (false, false, false, LightDeviceType::OnOff),
        (false, true, false, LightDeviceType::OnOff),
        (false, false, true, LightDeviceType::OnOff),
        (false, true, true, LightDeviceType::OnOff),
        (true, false, false, LightDeviceType::Dimmable),
        (true, false, true, LightDeviceType::Dimmable),
        (true, true, false, LightDeviceType::ColorTemperature),
        (true, true, true, LightDeviceType::ExtendedColor),
    ] {
        let mut capabilities = vec![Capability::OnOff];
        if brightness {
            capabilities.push(Capability::Brightness);
        }
        if temperature {
            capabilities.push(Capability::ColorTemperature(CT_RANGE));
        }
        if rgb {
            capabilities.push(Capability::ColorRgb);
        }
        let mapped = map_light(&light(capabilities)).unwrap();
        assert_eq!(mapped.device_type, expected);
        assert_eq!(mapped.brightness, brightness);
        assert_eq!(mapped.rgb, rgb);
        assert_eq!(mapped.has_color_control(), temperature || rgb);
        assert_eq!(mapped.temperature.is_some(), temperature);
        if let Some(published) = mapped.temperature {
            assert_eq!(published.range, CT_RANGE);
            assert_eq!(published.bounds, CT_RANGE.mired_bounds().unwrap());
        }
    }
}

#[test]
fn lights_without_on_off_report_unmappable_without_fabricated_capabilities() {
    for (brightness, temperature, rgb) in [
        (true, false, false),
        (false, true, false),
        (false, false, true),
        (true, true, false),
        (true, false, true),
        (false, true, true),
        (true, true, true),
    ] {
        let mut capabilities = Vec::new();
        if brightness {
            capabilities.push(Capability::Brightness);
        }
        if temperature {
            capabilities.push(Capability::ColorTemperature(CT_RANGE));
        }
        if rgb {
            capabilities.push(Capability::ColorRgb);
        }
        assert_eq!(
            map_light(&light(capabilities)),
            Err(UnmappableReason::MissingOnOff)
        );
    }
}

#[test]
fn unrepresentable_temperature_ranges_are_valid_generic_but_unmappable() {
    let range = NumericRange {
        min: 1,
        max: 100,
        step: 1,
    };
    assert!(range.valid());
    assert!(range.mired_bounds().is_err());
    assert_eq!(
        map_light(&light(vec![
            Capability::OnOff,
            Capability::ColorTemperature(range)
        ])),
        Err(UnmappableReason::TemperatureRange)
    );
    assert_eq!(
        map_light(&light(vec![
            Capability::Brightness,
            Capability::ColorTemperature(range)
        ])),
        Err(UnmappableReason::MissingOnOff)
    );
}

#[test]
fn non_light_functions_are_not_mapped() {
    let outlet = FunctionDescription {
        id: "main".into(),
        label: "Outlet".into(),
        category: Category::Outlet,
        capabilities: vec![Capability::OnOff],
    };
    assert_eq!(map_light(&outlet), Err(UnmappableReason::NotLight));
}
