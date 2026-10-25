//! Common measurement projection, access and reporting behavior.

use super::*;

#[test]
fn unrepresentable_temperature_keeps_native_state_and_reports_unsupported_mapping() {
    let (device, _) = PhysicalDevice::new(
        PhysicalDeviceDescription::new(
            DeviceKey::new("xiaomi", "wide"),
            "Wide temperature",
            vec![FunctionDescription {
                id: "temperature".into(),
                label: "Temperature".into(),
                kind: FunctionKind::Temperature(MeasurementRange::new(-300.0, 500.0).unwrap()),
            }],
        )
        .unwrap(),
    );
    let function = &device.functions()[0];
    function.observe_measurement(400.0, None, 1).unwrap();
    let handler = model_handler(vec![entry(&device, 2, &[("temperature", 3)])]);
    handler.access(|node| assert!(node.endpoints.iter().any(|endpoint| endpoint.id == 3)));
    assert_eq!(
        function.snapshot().measurement().unwrap().value,
        Some(400.0)
    );
    assert!(measurement_mapping_issue(function).is_some());
}

#[test]
fn an_unsupported_measurement_range_has_a_stable_version_and_no_value_reports() {
    use crate::matter::sensors::TemperatureHandler;
    use rs_matter::dm::{Dataver, clusters::decl::temperature_measurement::ClusterHandler};

    setup!(_baseline, _model, im);
    let range = MeasurementRange::new(-300.0, 500.0).unwrap();
    let (physical, _) = PhysicalDevice::new(
        PhysicalDeviceDescription::new(
            DeviceKey::new("xiaomi", "wide"),
            "Wide temperature",
            vec![FunctionDescription {
                id: "temperature".into(),
                label: "Temperature".into(),
                kind: FunctionKind::Temperature(range),
            }],
        )
        .unwrap(),
    );
    let function = &physical.functions()[0];
    let handler = TemperatureHandler::new(function.clone(), range, 3, Dataver::new(100));
    block_on(async {
        let ctx = Context::at(&im, 3, 0x402, 0);
        let mut run = pin!(ClusterHandler::run(&handler, &ctx));
        assert!(poll_once(run.as_mut()).await.is_none());
        let version = ClusterHandler::dataver(&handler);
        for (value, received) in [(10.0, 1), (20.0, 2)] {
            function.observe_measurement(value, None, received).unwrap();
            assert!(poll_once(run.as_mut()).await.is_none());
            assert_eq!(ClusterHandler::dataver(&handler), version);
            assert!(ctx.changes.borrow().is_empty());
            assert_eq!(
                ClusterHandler::measured_value(&handler, &ctx)
                    .unwrap_err()
                    .code(),
                ErrorCode::ConstraintError
            );
            assert_eq!(
                function.snapshot().measurement().unwrap().value,
                Some(value)
            );
            assert!(measurement_mapping_issue(function).is_some());
        }
    });
}

#[test]
fn measured_values_are_nullable_and_projected_with_nearest_half_away_from_zero() {
    setup!(device, handler, im);
    block_on(async {
        let temperature = &device.functions()[0];
        let humidity = &device.functions()[1];
        let mut ctx = Context::at(&im, 3, 0x402, 0);
        let bytes = read_bytes(&handler, &ctx).await.unwrap();
        assert!(report_value(&bytes).null().is_ok());
        ctx.attribute = attr(0x402, 1);
        ctx.attribute.endpoint_id = 3;
        assert_eq!(
            report_value(&read_bytes(&handler, &ctx).await.unwrap())
                .i16()
                .unwrap(),
            -3000
        );
        ctx.attribute = attr(0x402, 2);
        ctx.attribute.endpoint_id = 3;
        assert_eq!(
            report_value(&read_bytes(&handler, &ctx).await.unwrap())
                .i16()
                .unwrap(),
            10000
        );
        ctx.attribute = attr(0x405, 0);
        ctx.attribute.endpoint_id = 4;
        let bytes = read_bytes(&handler, &ctx).await.unwrap();
        assert!(report_value(&bytes).null().is_ok());
        ctx.attribute = attr(0x405, 1);
        ctx.attribute.endpoint_id = 4;
        assert_eq!(
            report_value(&read_bytes(&handler, &ctx).await.unwrap())
                .u16()
                .unwrap(),
            0
        );
        ctx.attribute = attr(0x405, 2);
        ctx.attribute.endpoint_id = 4;
        assert_eq!(
            report_value(&read_bytes(&handler, &ctx).await.unwrap())
                .u16()
                .unwrap(),
            10000
        );

        temperature
            .observe_measurement(0.125, Some(10), 20)
            .unwrap();
        ctx.attribute = attr(0x402, 0);
        ctx.attribute.endpoint_id = 3;
        assert_eq!(
            report_value(&read_bytes(&handler, &ctx).await.unwrap())
                .i16()
                .unwrap(),
            13
        );
        temperature
            .observe_measurement(-0.125, Some(10), 21)
            .unwrap();
        assert_eq!(
            report_value(&read_bytes(&handler, &ctx).await.unwrap())
                .i16()
                .unwrap(),
            -13
        );
        humidity.observe_measurement(45.5, None, 22).unwrap();
        ctx.attribute = attr(0x405, 0);
        ctx.attribute.endpoint_id = 4;
        assert_eq!(
            report_value(&read_bytes(&handler, &ctx).await.unwrap())
                .u16()
                .unwrap(),
            4550
        );
    });
}

#[test]
fn measurement_attributes_are_read_only() {
    setup!(device, handler, im);
    block_on(async {
        device.functions()[0]
            .observe_measurement(21.0, Some(1), 2)
            .unwrap();
        for (endpoint, cluster) in [(3, 0x402), (4, 0x405)] {
            let mut ctx = Context::at(&im, endpoint, cluster, 0);
            ctx.data = TLVElement::new(&[0x15, 0x18]);
            let error = handler.write(&ctx).await.unwrap_err();
            assert_eq!(error.code(), ErrorCode::AttributeNotFound);
        }
    });
}

#[test]
fn measurement_changes_bump_versions_and_notify_only_projected_value_edges() {
    setup!(device, handler, im);
    block_on(async {
        let temperature = &device.functions()[0];
        let humidity = &device.functions()[1];
        let ctx = Context::at(&im, 3, 0x402, 0);
        let mut run = pin!(handler.run(&ctx));
        assert!(poll_once(run.as_mut()).await.is_none());
        ctx.changes.borrow_mut().clear();

        temperature.observe_measurement(0.124, Some(1), 2).unwrap();
        assert!(poll_once(run.as_mut()).await.is_none());
        assert!(ctx.changes.borrow().contains(&(3, 0x402, 0)));
        let version = {
            let bytes = read_bytes(&handler, &ctx).await.unwrap();
            report_dataver(&bytes)
        };
        let count = ctx.changes.borrow().len();
        // Same projected value and a timestamp-only change must not fabricate a report.
        temperature.observe_measurement(0.1249, Some(2), 3).unwrap();
        assert!(poll_once(run.as_mut()).await.is_none());
        assert_eq!(ctx.changes.borrow().len(), count);
        let bytes = read_bytes(&handler, &ctx).await.unwrap();
        assert_eq!(report_dataver(&bytes), version);

        temperature.observe_measurement(1.0, Some(3), 4).unwrap();
        assert!(poll_once(run.as_mut()).await.is_none());
        assert!(ctx.changes.borrow().contains(&(3, 0x402, 0)));
        assert_ne!(
            report_dataver(&read_bytes(&handler, &ctx).await.unwrap()),
            version
        );

        ctx.changes.borrow_mut().clear();
        humidity.observe_measurement(50.0, Some(4), 5).unwrap();
        assert!(poll_once(run.as_mut()).await.is_none());
        assert!(ctx.changes.borrow().contains(&(4, 0x405, 0)));
        assert!(!ctx.changes.borrow().contains(&(3, 0x402, 0)));
    });
}

#[test]
fn matter_projection_rejects_unrepresentable_and_out_of_range_values() {
    assert_eq!(units::temperature_matter_value(-273.15), Some(-27315));
    assert_eq!(units::temperature_matter_value(327.66), Some(32766));
    assert_eq!(units::temperature_matter_value(327.67), Some(32767));
    assert_eq!(units::temperature_matter_value(327.671), None);
    assert_eq!(units::temperature_matter_value(-273.151), None);
    assert_eq!(units::temperature_matter_value(f64::NAN), None);
    assert_eq!(units::humidity_matter_value(0.0), Some(0));
    assert_eq!(units::humidity_matter_value(100.0), Some(10000));
    assert_eq!(units::humidity_matter_value(100.001), None);
    assert_eq!(units::humidity_matter_value(-0.001), None);
    assert_eq!(units::battery_percent_matter_value(0.0), Some(0));
    assert_eq!(units::battery_percent_matter_value(0.25), Some(1));
    assert_eq!(units::battery_percent_matter_value(5.0), Some(10));
    assert_eq!(units::battery_percent_matter_value(100.0), Some(200));
    assert_eq!(units::battery_percent_matter_value(100.001), None);
    assert_eq!(units::battery_percent_matter_value(-0.001), None);
    assert_eq!(units::battery_percent_matter_value(f64::NAN), None);
    assert_eq!(
        units::temperature_matter_bounds(MeasurementRange::new(-30.0, 100.0).unwrap()),
        Some((-3000, 10000))
    );
    assert_eq!(
        units::humidity_matter_bounds(MeasurementRange::new(0.0, 100.0).unwrap()),
        Some((0, 10000))
    );
    assert_eq!(
        units::temperature_matter_bounds(MeasurementRange::new(-273.15, 327.67).unwrap()),
        Some((-27315, 32767))
    );
}

#[test]
fn availability_changes_report_reachable_on_the_parent_not_measurement_edges() {
    setup!(device, handler, im);
    block_on(async {
        let ctx = Context::at(&im, 2, 0x39, 17);
        let mut run = pin!(handler.run(&ctx));
        assert!(poll_once(run.as_mut()).await.is_none());
        ctx.changes.borrow_mut().clear();
        device.set_availability(Availability::Available);
        assert!(poll_once(run.as_mut()).await.is_none());
        assert!(ctx.changes.borrow().contains(&(2, 0x39, 17)));
        assert!(ctx.events.borrow().contains(&(2, 0x39, 3)));
        assert!(
            !ctx.changes
                .borrow()
                .iter()
                .any(|(_, cluster, _)| *cluster == 0x402)
        );
    });
}
