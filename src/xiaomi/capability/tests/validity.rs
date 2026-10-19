//! Native binding and parsed-device boundary tests. The four supported light
//! profiles come from real bindings; capability lists are diagnostics only.

use super::key;
use crate::device::{LightProfile, NumericRange, TemperatureInterval};
use crate::xiaomi::capability::{
    BrightnessBinding, ColorBinding, LightBindings, ModeBinding, NumericFormat, ParsedDevice,
    ParsedFunction, PropertyBinding, TemperatureBinding, validate_model_topology,
};

fn property(siid: u32, piid: u32) -> PropertyBinding {
    PropertyBinding { siid, piid }
}

fn brightness() -> BrightnessBinding {
    BrightnessBinding::new(
        property(2, 2),
        NumericFormat::Uint32,
        NumericRange::new(1, 100, 1).unwrap(),
    )
    .unwrap()
}

fn temperature(min: u32, max: u32, step: u32) -> TemperatureBinding {
    TemperatureBinding::new(
        property(2, 3),
        NumericFormat::Uint16,
        NumericRange::new(min, max, step).unwrap(),
    )
    .unwrap()
}

fn color() -> ColorBinding {
    ColorBinding::new(property(4, 1), NumericFormat::Uint32).unwrap()
}

#[test]
fn light_bindings_only_accept_the_four_supported_profiles() {
    let brightness = brightness();
    let temperature = temperature(2700, 6500, 1);
    let color = color();

    assert_eq!(
        LightBindings::new(None, None, None, None)
            .unwrap()
            .profile(),
        LightProfile::OnOff
    );
    assert_eq!(
        LightBindings::new(Some(brightness), None, None, None)
            .unwrap()
            .profile(),
        LightProfile::Dimmable
    );
    assert_eq!(
        LightBindings::new(Some(brightness), Some(temperature), None, None)
            .unwrap()
            .profile(),
        LightProfile::ColorTemperature(TemperatureInterval::new(2700, 6500).unwrap())
    );
    assert_eq!(
        LightBindings::new(Some(brightness), Some(temperature), Some(color), None)
            .unwrap()
            .profile(),
        LightProfile::ExtendedColor(TemperatureInterval::new(2700, 6500).unwrap())
    );

    // Every other real-binding combination is rejected.
    assert!(LightBindings::new(None, Some(temperature), None, None).is_err());
    assert!(LightBindings::new(None, None, Some(color), None).is_err());
    assert!(LightBindings::new(Some(brightness), None, Some(color), None).is_err());
    assert!(LightBindings::new(None, Some(temperature), Some(color), None).is_err());
}

#[test]
fn mode_bindings_require_both_color_dimensions_and_distinct_values() {
    let mode = ModeBinding::new(property(4, 2), NumericFormat::Uint8, 0, 1).unwrap();
    let brightness = brightness();
    let temperature = temperature(2700, 6500, 1);
    assert!(LightBindings::new(Some(brightness), Some(temperature), None, Some(mode)).is_err());
    assert!(
        LightBindings::new(
            Some(brightness),
            Some(temperature),
            Some(color()),
            Some(mode)
        )
        .is_ok()
    );
    assert_eq!(
        ModeBinding::new(property(4, 2), NumericFormat::Uint8, 1, 1),
        Err(crate::xiaomi::capability::CapabilityError::InvalidLight(
            "mode"
        ))
    );
    assert!(ModeBinding::new(property(0, 2), NumericFormat::Uint8, 0, 1).is_err());
}

#[test]
fn binding_constructors_validate_grids_formats_and_intervals() {
    assert!(
        BrightnessBinding::new(
            property(0, 2),
            NumericFormat::Uint8,
            NumericRange::new(1, 100, 1).unwrap()
        )
        .is_err()
    );
    assert!(
        BrightnessBinding::new(
            property(2, 2),
            NumericFormat::Uint8,
            NumericRange::new(1, 100, 3).unwrap()
        )
        .is_ok()
    );
    // A grid whose nonzero span collapses cannot be a brightness binding.
    assert!(
        BrightnessBinding::new(
            property(2, 2),
            NumericFormat::Uint8,
            NumericRange::new(0, 1, 1).unwrap()
        )
        .is_err()
    );
    // The declared format must hold every reachable value.
    assert!(
        BrightnessBinding::new(
            property(2, 2),
            NumericFormat::Uint8,
            NumericRange::new(1, 300, 1).unwrap()
        )
        .is_err()
    );
    // RGB has a fixed full range.
    assert!(ColorBinding::new(property(4, 1), NumericFormat::Uint16).is_err());
    // Unrepresentable native temperature grids are rejected.
    for (min, max) in [(1, 6500), (10, 6500), (4500, 4501)] {
        assert!(
            TemperatureBinding::new(
                property(2, 3),
                NumericFormat::Uint16,
                NumericRange::new(min, max, 1).unwrap()
            )
            .is_err(),
            "{min}..{max}"
        );
    }
    assert!(
        TemperatureBinding::new(
            property(0, 3),
            NumericFormat::Uint16,
            NumericRange::new(2700, 6500, 1).unwrap()
        )
        .is_err()
    );
}

#[test]
fn native_quantization_grid_stays_separate_from_the_logical_interval() {
    let binding = temperature(2000, 3000, 100);
    assert_eq!(binding.range(), NumericRange::new(2000, 3000, 100).unwrap());
    assert_eq!(
        binding.interval(),
        TemperatureInterval::new(2000, 3000).unwrap()
    );
    // Xiaomi quantization happens at the source binding boundary.
    assert_eq!(binding.temperature_to_raw(2050.0), 2100);
    assert_eq!(binding.temperature_to_raw(2199.0), 2200);
    let profile = LightBindings::new(Some(brightness()), Some(binding), None, None)
        .unwrap()
        .profile();
    assert_eq!(
        profile,
        LightProfile::ColorTemperature(TemperatureInterval::new(2000, 3000).unwrap())
    );
}

#[test]
fn profile_interval_uses_the_effective_grid_maximum_for_the_mired_span() {
    let binding = temperature(2700, 6550, 100);
    assert_eq!(binding.range().max(), 6550);
    assert_eq!(
        binding.interval(),
        TemperatureInterval::new(2700, 6500).unwrap()
    );
    assert_eq!(
        crate::lighting::temperature::mired_bounds(binding.interval()),
        (154, 370)
    );
}

#[test]
fn mired_conversion_keeps_precision_until_one_native_quantization() {
    let binding = temperature(2000, 4000, 2);
    let kelvin = crate::lighting::temperature::kelvin_from_mireds(253);
    assert!((kelvin - 1_000_000.0 / 253.0).abs() < f64::EPSILON);
    // Baseline: quantize(3952.569) = 3952. Rounding to 3953 first would
    // double-round to 3954.
    assert_eq!(binding.temperature_to_raw(kelvin), 3952);
}

#[test]
fn parsed_devices_require_non_empty_unique_functions_and_supported_topologies() {
    let outlet = ParsedFunction::outlet("main", "Main", property(2, 1));
    assert!(ParsedDevice::new(key(), vec![], None).is_err());
    assert!(ParsedDevice::new(key(), vec![outlet.clone(), outlet.clone()], None).is_err());
    assert!(ParsedDevice::new(key(), vec![outlet.clone()], None).is_ok());

    // Arbitrary multi-outlet layouts are outside the supported range.
    let second = ParsedFunction::outlet("outlet_2", "Second", property(3, 1));
    assert!(ParsedDevice::new(key(), vec![outlet.clone(), second.clone()], None).is_err());

    // The concrete cp7pd three-channel layout is admitted with its trigger.
    let cp7pd = |id, siid| ParsedFunction::outlet(id, id, property(siid, 1));
    assert!(
        ParsedDevice::new(
            key(),
            vec![cp7pd("outlet_1", 3), cp7pd("outlet_2", 4), cp7pd("usb", 9),],
            Some(property(2, 1)),
        )
        .is_ok()
    );
    assert!(
        ParsedDevice::new(key(), vec![cp7pd("outlet_1", 3)], Some(property(2, 1))).is_err(),
        "a trigger alone does not make a cp7pd layout"
    );

    // Unsupported combinations stay rejected even when bindings differ.
    let overlapping = ParsedFunction::outlet("outlet_2", "Second", property(2, 1));
    assert!(ParsedDevice::new(key(), vec![outlet.clone(), overlapping], None).is_err());

    let light = ParsedFunction::light(
        "main",
        "Light",
        property(2, 1),
        LightBindings::new(None, None, None, None).unwrap(),
    );
    assert!(ParsedDevice::new(key(), vec![light.clone(), outlet.clone()], None).is_err());
    assert!(ParsedDevice::new(key(), vec![light.clone()], Some(property(9, 1))).is_err());
    assert!(ParsedDevice::new(key(), vec![light], None).is_ok());
}

#[test]
fn model_topology_rejects_kind_and_cp7pd_recovery_mismatches() {
    let outlet_type = "urn:miot-spec-v2:device:outlet:0000A002:test:1";
    let cp7pd_type = "urn:miot-spec-v2:device:outlet:0000A002:cuco-cp7pd:1";

    let light = ParsedFunction::light(
        "main",
        "Light",
        property(2, 1),
        LightBindings::new(None, None, None, None).unwrap(),
    );
    let light_device = ParsedDevice::new(key(), vec![light], None).unwrap();
    assert!(
        validate_model_topology(&light_device, "vendor.plug", outlet_type).is_err(),
        "a light kind cannot hide behind an outlet instance type"
    );

    let channel = |id, siid| ParsedFunction::outlet(id, id, property(siid, 1));
    let valid = ParsedDevice::new(
        key(),
        vec![
            channel("outlet_1", 3),
            channel("outlet_2", 4),
            channel("usb", 9),
        ],
        Some(property(2, 1)),
    )
    .unwrap();
    assert!(validate_model_topology(&valid, "cuco.plug.cp7pd", cp7pd_type).is_ok());
    assert!(validate_model_topology(&valid, "vendor.plug", outlet_type).is_err());
    assert!(
        validate_model_topology(&valid, "cuco.plug.cp7pd", outlet_type).is_err(),
        "the cp7pd model needs its cp7pd instance type"
    );
    assert!(validate_model_topology(&valid, "vendor.plug", cp7pd_type).is_err());

    let wrong_trigger = ParsedDevice::new(
        key(),
        vec![
            channel("outlet_1", 3),
            channel("outlet_2", 4),
            channel("usb", 9),
        ],
        Some(property(2, 2)),
    )
    .unwrap();
    assert!(
        validate_model_topology(&wrong_trigger, "cuco.plug.cp7pd", cp7pd_type).is_err(),
        "the master trigger must match the real binding"
    );

    let wrong_channel = ParsedDevice::new(
        key(),
        vec![
            channel("outlet_1", 5),
            channel("outlet_2", 4),
            channel("usb", 9),
        ],
        Some(property(2, 1)),
    )
    .unwrap();
    assert!(
        validate_model_topology(&wrong_channel, "cuco.plug.cp7pd", cp7pd_type).is_err(),
        "cp7pd channels own exact bindings"
    );
}

#[test]
fn declared_temperature_maximum_still_bounds_the_native_grid() {
    // min=4000, max=1_000_100, step=1000: the effective grid ends exactly at
    // 1_000_000 K and would pass the interval constructor, but the declared
    // Kelvins bound is above the documented 1_000_000 K limit and must stay
    // rejected. Removing the duplicate grid check must not relax this.
    let range = NumericRange::new(4000, 1_000_100, 1_000).unwrap();
    assert!(range.effective_max() == 1_000_000);
    assert!(
        TemperatureBinding::new(property(2, 3), NumericFormat::Uint32, range).is_err(),
        "the declared maximum is part of the accepted domain"
    );
}
