use super::{
    AirConditionerConstraints, AirConditionerFanSpeed, AirConditionerMode, AirConditionerValue,
    ConstraintViolation, TemperatureGrid,
};
use crate::device::{
    Availability, DeviceKey, FunctionDescription, FunctionKind, PhysicalDevice,
    PhysicalDeviceDescription,
};

#[test]
fn native_integer_grid_distinguishes_reachable_and_unreachable_temperatures() {
    let grid = TemperatureGrid::new(16, 31, 2).unwrap();
    assert_eq!(
        (grid.min(), grid.max(), grid.step(), grid.effective_max()),
        (16, 31, 2, 30)
    );
    assert!(grid.contains(16));
    assert!(grid.contains(30));
    assert!(!grid.contains(17));
    assert!(!grid.contains(31));
    assert!(!grid.contains(32));
    assert!(TemperatureGrid::new(30, 16, 1).is_err());
    assert!(TemperatureGrid::new(16, 30, 0).is_err());
}

#[test]
fn standard_constraints_allow_all_confirmed_modes_temperatures_and_speeds() {
    let grid = TemperatureGrid::new(16, 30, 1).unwrap();
    let constraints = AirConditionerConstraints::standard(grid);
    for mode in AirConditionerMode::ALL {
        assert!(constraints.validate_mode(mode).is_ok());
        for temperature in [16, 26, 30] {
            assert!(constraints.validate_temperature(mode, temperature).is_ok());
        }
        for speed in AirConditionerFanSpeed::ALL {
            assert!(constraints.validate_speed(mode, speed).is_ok());
        }
        assert_eq!(
            constraints.validate_temperature(mode, 31),
            Err(ConstraintViolation::Constraint)
        );
    }
}

#[test]
fn runtime_constraints_replace_mode_and_parameter_permissions_without_changing_grid() {
    use AirConditionerFanSpeed::{Auto, High, Low};
    use AirConditionerMode::{Cool, Dry, Fan, Heat};
    let grid = TemperatureGrid::new(16, 30, 2).unwrap();
    let constraints = AirConditionerConstraints::new(
        grid,
        [
            (Cool, vec![16, 26, 30], vec![Auto, Low, High]),
            (AirConditionerMode::Auto, vec![], vec![Auto, Low, High]),
            (Dry, vec![16, 26], vec![Low]),
        ],
    )
    .unwrap();
    assert_eq!(constraints.grid(), grid);
    assert!(constraints.validate_mode(Cool).is_ok());
    assert_eq!(
        constraints.validate_mode(Heat),
        Err(ConstraintViolation::InvalidInState)
    );
    assert_eq!(
        constraints.validate_temperature(Cool, 24),
        Err(ConstraintViolation::InvalidInState)
    );
    assert_eq!(
        constraints.validate_temperature(AirConditionerMode::Auto, 26),
        Err(ConstraintViolation::InvalidInState)
    );
    assert_eq!(
        constraints.validate_temperature(Cool, 25),
        Err(ConstraintViolation::Constraint)
    );
    assert_eq!(
        constraints.validate_temperature(Fan, 31),
        Err(ConstraintViolation::Constraint)
    );
    assert!(constraints.validate_speed(Dry, Low).is_ok());
    assert_eq!(
        constraints.validate_speed(Dry, Auto),
        Err(ConstraintViolation::InvalidInState)
    );
    assert_eq!(constraints.temperatures(Dry), Some([16, 26].as_slice()));
    assert_eq!(constraints.speeds(Dry), Some([Low].as_slice()));
    assert_eq!(constraints.temperatures(Fan), None);

    let replacement = AirConditionerConstraints::standard(grid);
    assert!(
        replacement
            .validate_temperature(AirConditionerMode::Auto, 26)
            .is_ok()
    );
    assert!(replacement.validate_speed(Dry, High).is_ok());
    assert_eq!(replacement.grid(), constraints.grid());
}

#[test]
fn runtime_constraint_models_reject_ambiguous_or_off_grid_values() {
    use AirConditionerFanSpeed::{Auto, Low};
    use AirConditionerMode::Cool;
    let grid = TemperatureGrid::new(16, 30, 2).unwrap();
    assert!(AirConditionerConstraints::new(grid, [(Cool, vec![17], vec![Auto])]).is_err());
    assert!(AirConditionerConstraints::new(grid, [(Cool, vec![16, 16], vec![Auto])]).is_err());
    assert!(AirConditionerConstraints::new(grid, [(Cool, vec![16], vec![Low, Low])]).is_err());
    assert!(
        AirConditionerConstraints::new(
            grid,
            [(Cool, vec![16], vec![Auto]), (Cool, vec![], vec![])]
        )
        .is_err()
    );
}

#[test]
fn source_observations_keep_each_field_unknown_until_reported_and_preserve_metadata() {
    let description = PhysicalDeviceDescription::new(
        DeviceKey::new("xiaomi", "thermostat-observation"),
        "Air conditioner",
        vec![FunctionDescription {
            id: "air-conditioner:3".into(),
            label: "空调".into(),
            kind: FunctionKind::AirConditioner {
                temperature: TemperatureGrid::new(16, 30, 1).unwrap(),
                vertical_swing: true,
            },
        }],
    )
    .unwrap();
    let (device, _receiver) = PhysicalDevice::new(description);
    device.set_availability(Availability::Available);
    let function = &device.functions()[0];
    let observed = || function.snapshot().air_conditioner().unwrap();

    let state = observed();
    assert_eq!(state.power.value, None);
    assert_eq!(state.mode.value, None);
    assert_eq!(state.temperature.value, None);
    assert_eq!(state.speed.value, None);
    assert_eq!(state.swing.value, None);

    function.observe_air_conditioner(AirConditionerValue::Power(true), Some(11), 12);
    function.observe_air_conditioner(
        AirConditionerValue::Mode(AirConditionerMode::Cool),
        None,
        13,
    );
    function.observe_air_conditioner(AirConditionerValue::Temperature(26), Some(14), 15);
    function.observe_air_conditioner(
        AirConditionerValue::Speed(AirConditionerFanSpeed::Auto),
        Some(16),
        17,
    );
    function.observe_air_conditioner(AirConditionerValue::Swing(false), Some(18), 19);
    let state = observed();
    assert_eq!(state.power.value, Some(true));
    assert_eq!(state.mode.value, Some(AirConditionerMode::Cool));
    assert_eq!(state.temperature.value, Some(26));
    assert_eq!(state.speed.value, Some(AirConditionerFanSpeed::Auto));
    assert_eq!(state.swing.value, Some(false));
    assert_eq!(
        (state.power.source_time, state.power.received_at),
        (Some(11), Some(12))
    );
    assert_eq!(
        (state.mode.source_time, state.mode.received_at),
        (None, Some(13))
    );
    assert_eq!(
        (state.temperature.source_time, state.temperature.received_at),
        (Some(14), Some(15))
    );
    assert_eq!(
        (state.speed.source_time, state.speed.received_at),
        (Some(16), Some(17))
    );
    assert_eq!(
        (state.swing.source_time, state.swing.received_at),
        (Some(18), Some(19))
    );

    let revision = function.snapshot().revision;
    function.observe_air_conditioner(AirConditionerValue::Temperature(26), Some(20), 21);
    let state = observed();
    assert_eq!(state.temperature.value, Some(26));
    assert_eq!(
        (state.temperature.source_time, state.temperature.received_at),
        (Some(20), Some(21))
    );
    assert_eq!(function.snapshot().revision, revision);
}
