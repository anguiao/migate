use migate::device::{
    AccountId, Capability, DeviceChange, DeviceCommand, DeviceService, FeatureCapabilities,
    FeatureIdentity, FeatureRole, HomeId, HvacMode, NumericRange, NumericUnit, Percent,
    PhysicalDeviceId, Property, PropertyState, PropertyValue, StateReport, StateSource, SwingMode,
};

fn feature() -> FeatureIdentity {
    FeatureIdentity {
        physical: PhysicalDeviceId {
            account: AccountId::new("uid-1").unwrap(),
            home: HomeId::new("home-1").unwrap(),
            parent_did: "did-1".parse().unwrap(),
        },
        service_instance: 2,
        role: FeatureRole::Light,
    }
}

#[test]
fn reports_keep_last_known_values_and_reject_older_versions() {
    let service = DeviceService::new();
    let id = feature();
    service.publish(
        id.clone(),
        "Kitchen",
        FeatureCapabilities::light(true, false),
    );
    let mut changes = service.subscribe();

    service.apply_report(StateReport::new(
        id.clone(),
        7,
        StateSource::Gateway,
        100,
        [(Property::Power, PropertyValue::Power(true))],
    ));
    service.apply_report(StateReport::new(
        id.clone(),
        6,
        StateSource::Cloud,
        110,
        [(Property::Power, PropertyValue::Power(false))],
    ));

    assert_eq!(
        service.snapshot(&id).unwrap().property(Property::Power),
        Some(&PropertyState::Current {
            value: PropertyValue::Power(true),
            source: StateSource::Gateway,
            observed_at: 100,
            report_version: 7,
        })
    );
    service.mark_unconfirmed(&id);
    assert_eq!(
        service.snapshot(&id).unwrap().property(Property::Power),
        Some(&PropertyState::LastKnown {
            value: PropertyValue::Power(true),
            source: StateSource::Gateway,
            observed_at: 100,
            report_version: 7,
        })
    );
    let changes = changes.drain();
    assert!(matches!(
        changes.first(),
        Some(DeviceChange::StateChanged { .. })
    ));
}

#[test]
fn query_start_version_prevents_late_result_overwriting_a_push() {
    let service = DeviceService::new();
    let id = feature();
    service.publish(
        id.clone(),
        "Kitchen",
        FeatureCapabilities::light(true, false),
    );
    let query_version = service.begin_query(&id, Property::Power);
    service.apply_report(StateReport::new(
        id.clone(),
        query_version + 1,
        StateSource::Gateway,
        20,
        [(Property::Power, PropertyValue::Power(true))],
    ));
    service.apply_report(StateReport::new(
        id.clone(),
        query_version,
        StateSource::Cloud,
        30,
        [(Property::Power, PropertyValue::Power(false))],
    ));
    assert!(matches!(
        service.snapshot(&id).unwrap().property(Property::Power),
        Some(PropertyState::Current {
            value: PropertyValue::Power(true),
            ..
        })
    ));
}

#[test]
fn topology_removal_and_logout_retain_identity_but_disable_control() {
    let service = DeviceService::new();
    let id = feature();
    service.publish(
        id.clone(),
        "Kitchen",
        FeatureCapabilities::light(true, false),
    );
    assert!(service.can_control(&id));

    service.remove(&id);
    assert!(!service.can_control(&id));
    assert!(service.feature(&id).is_none());

    service.restore(
        id.clone(),
        "Kitchen",
        FeatureCapabilities::light(true, false),
    );
    assert!(service.feature(&id).is_some());
    assert!(!service.can_control(&id));
    service.admit(&id);
    assert!(service.can_control(&id));

    service.logout();
    assert!(service.feature(&id).is_some());
    assert!(!service.can_control(&id));
}

#[test]
fn constrained_core_values_reject_out_of_range_input() {
    assert!(Percent::new(0).is_ok());
    assert!(Percent::new(100).is_ok());
    assert!(Percent::new(101).is_err());
    assert_eq!(Percent::new(55.5).unwrap().get(), 55.5);
}

#[test]
fn command_validation_uses_ranges_steps_and_available_enums() {
    let service = DeviceService::new();
    let id = feature();
    service.publish(
        id.clone(),
        "AC",
        FeatureCapabilities(vec![
            Capability::TargetTemperature(NumericRange {
                minimum: 16.0,
                maximum: 30.0,
                step: 1.0,
                unit: NumericUnit::Celsius,
            }),
            Capability::HvacModes(vec![HvacMode::Off, HvacMode::Cool]),
        ]),
    );
    assert!(
        service
            .validate_command(&id, &DeviceCommand::SetTargetTemperature(25.0))
            .is_ok()
    );
    assert!(
        service
            .validate_command(&id, &DeviceCommand::SetTargetTemperature(25.5))
            .is_err()
    );
    assert!(
        service
            .validate_command(&id, &DeviceCommand::SetHvacMode(HvacMode::Heat))
            .is_err()
    );
}

#[test]
fn oscillation_accepts_any_supported_non_off_swing_direction() {
    let service = DeviceService::new();
    let id = feature();
    service.publish(
        id.clone(),
        "Fan",
        FeatureCapabilities(vec![Capability::SwingModes(vec![
            SwingMode::Off,
            SwingMode::Horizontal,
        ])]),
    );
    assert!(
        service
            .validate_command(&id, &DeviceCommand::SetOscillation(true))
            .is_ok()
    );
    assert!(
        FeatureCapabilities::default()
            .validate(&DeviceCommand::SetOscillation(true))
            .is_err()
    );
}

#[test]
fn unknown_property_retains_its_last_known_value_without_affecting_others() {
    let service = DeviceService::new();
    let id = feature();
    service.publish(id.clone(), "Sensor", FeatureCapabilities::default());
    service.apply_report(StateReport::new(
        id.clone(),
        1,
        StateSource::Gateway,
        10,
        [
            (
                Property::Occupancy,
                PropertyValue::Occupancy(migate::device::PresenceState::Occupied),
            ),
            (
                Property::Battery,
                PropertyValue::Percent(Percent::new(80).unwrap()),
            ),
        ],
    ));
    service.apply_unknown(&id, Property::Occupancy, 2);
    let snapshot = service.snapshot(&id).unwrap();
    assert!(
        matches!(snapshot.property(Property::Occupancy),Some(PropertyState::Unknown{last_known:Some(value),..}) if value.value==PropertyValue::Occupancy(migate::device::PresenceState::Occupied))
    );
    assert!(matches!(
        snapshot.property(Property::Battery),
        Some(PropertyState::Current { .. })
    ));
}

#[test]
fn changed_feature_metadata_notifies_and_lag_requests_resync() {
    let service = DeviceService::new();
    let id = feature();
    service.publish(id.clone(), "Old", FeatureCapabilities::default());
    let mut changes = service.subscribe();
    service.publish(id.clone(), "New", FeatureCapabilities::light(false, false));
    assert!(
        matches!(changes.drain().as_slice(),[DeviceChange::FeatureUpdated(changed)] if changed==&id)
    );
    for version in 1..=300 {
        service.apply_report(StateReport::new(
            id.clone(),
            version,
            StateSource::Lan,
            version as i64,
            [(Property::Power, PropertyValue::Power(version % 2 == 0))],
        ));
    }
    let lagged = DeviceService::new();
    let lagged_id = feature();
    let mut subscription = lagged.subscribe();
    lagged.publish(lagged_id.clone(), "A", FeatureCapabilities::default());
    for version in 1..=300 {
        lagged.apply_report(StateReport::new(
            lagged_id.clone(),
            version,
            StateSource::Lan,
            version as i64,
            [(Property::Power, PropertyValue::Power(true))],
        ));
    }
    assert_eq!(subscription.drain(), vec![DeviceChange::Resync]);
}
