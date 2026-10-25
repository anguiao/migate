//! Illuminance cluster metadata, projection and reports.

use super::*;

#[test]
fn illuminance_projection_covers_low_light_logarithmic_values_and_target_bounds() {
    for (lux, expected) in [
        (0.0, 0),
        (0.1, 0),
        (0.999, 0),
        (1.0, 1),
        (10.0, 10001),
        (1000.0, 30001),
        (65535.0, 48166),
    ] {
        assert_eq!(
            units::illuminance_matter_value(lux),
            Some(expected),
            "{lux} lux"
        );
    }
    let maximum = 10.0_f64.powf((65534.0 - 1.0) / 10000.0);
    assert_eq!(units::illuminance_matter_value(maximum), Some(65534));
    assert_eq!(units::illuminance_matter_value(maximum * 1.001), None);
    assert_eq!(
        units::illuminance_matter_bounds(MeasurementRange::new(0.0, 1000.0).unwrap()),
        Some((1, 30001))
    );
    assert_eq!(
        units::illuminance_matter_bounds(MeasurementRange::new(10.0, 10000.0).unwrap()),
        Some((10001, 40001))
    );
    assert_eq!(
        units::illuminance_matter_bounds(MeasurementRange::new(0.0, 0.9).unwrap()),
        None
    );
}

fn illuminance_sensor(range: MeasurementRange) -> PhysicalDevice {
    PhysicalDevice::new(
        PhysicalDeviceDescription::new(
            DeviceKey::new("xiaomi", "illuminance"),
            "Illuminance sensor",
            vec![FunctionDescription {
                id: "illuminance".into(),
                label: "Illuminance".into(),
                kind: FunctionKind::Illuminance(range),
            }],
        )
        .unwrap()
        .with_battery(),
    )
    .0
}

#[test]
fn illuminance_publishes_nullable_logarithmic_lux_without_optional_precision_attributes() {
    setup!(@device device, handler, im, illuminance_sensor(MeasurementRange::new(0.0, 10000.0).unwrap()));
    handler.access(|node| {
        let child = node
            .endpoints
            .iter()
            .find(|endpoint| endpoint.id == 3)
            .unwrap();
        assert_eq!(
            child
                .device_types
                .iter()
                .map(|kind| (kind.dtype, kind.drev))
                .collect::<Vec<_>>(),
            [(0x0106, 3)]
        );
        assert_eq!(
            child
                .clusters
                .iter()
                .map(|cluster| cluster.id)
                .collect::<Vec<_>>(),
            [0x1d, 3, 0x400]
        );
        let cluster = child
            .clusters
            .iter()
            .find(|cluster| cluster.id == 0x400)
            .unwrap();
        assert_eq!(cluster.revision, 3);
        assert_eq!(cluster.feature_map, 0);
        assert!(cluster.commands.is_empty());
        for id in [0, 1, 2, 0xfff8, 0xfff9, 0xfffb, 0xfffc, 0xfffd] {
            assert!(cluster.attribute(id).is_some());
        }
        for id in [3, 4] {
            assert!(cluster.attribute(id).is_none());
        }
    });
    block_on(async {
        for (attribute, expected) in [(1, 1), (2, 40001)] {
            let context = Context::at(&im, 3, 0x400, attribute);
            assert_eq!(
                report_value(&read_bytes(&handler, &context).await.unwrap())
                    .u16()
                    .unwrap(),
                expected
            );
        }
        let context = Context::at(&im, 3, 0x400, 0);
        assert!(
            report_value(&read_bytes(&handler, &context).await.unwrap())
                .null()
                .is_ok()
        );
        for (lux, expected) in [
            (0.0, 0),
            (0.125, 0),
            (1.0, 1),
            (10.0, 10001),
            (1000.0, 30001),
        ] {
            device.functions()[0]
                .observe_measurement(lux, Some(100), 200)
                .unwrap();
            assert_eq!(
                report_value(&read_bytes(&handler, &context).await.unwrap())
                    .u16()
                    .unwrap(),
                expected
            );
            assert_eq!(
                device.functions()[0]
                    .snapshot()
                    .measurement()
                    .unwrap()
                    .value,
                Some(lux)
            );
        }
        assert_eq!(
            handler.write(&context).await.unwrap_err().code(),
            ErrorCode::AttributeNotFound
        );
    });
}

#[test]
fn illuminance_reports_projected_changes_and_preserves_sub_lux_native_values() {
    setup!(@device device, handler, im, illuminance_sensor(MeasurementRange::new(0.0, 10000.0).unwrap()));
    block_on(async {
        let context = Context::at(&im, 3, 0x400, 0);
        let mut run = pin!(handler.run(&context));
        assert!(poll_once(run.as_mut()).await.is_none());
        context.changes.borrow_mut().clear();
        let function = &device.functions()[0];
        function.observe_measurement(0.125, Some(1), 2).unwrap();
        assert!(poll_once(run.as_mut()).await.is_none());
        assert_eq!(context.changes.borrow().as_slice(), [(3, 0x400, 0)]);
        let low_version = report_dataver(&read_bytes(&handler, &context).await.unwrap());
        context.changes.borrow_mut().clear();
        for lux in [0.5, 0.999] {
            function.observe_measurement(lux, None, 3).unwrap();
            assert!(poll_once(run.as_mut()).await.is_none());
            assert!(context.changes.borrow().is_empty());
            assert_eq!(
                report_dataver(&read_bytes(&handler, &context).await.unwrap()),
                low_version
            );
            assert_eq!(function.snapshot().measurement().unwrap().value, Some(lux));
        }
        function.observe_measurement(1.0, None, 4).unwrap();
        assert!(poll_once(run.as_mut()).await.is_none());
        assert_eq!(context.changes.borrow().as_slice(), [(3, 0x400, 0)]);
        assert_ne!(
            report_dataver(&read_bytes(&handler, &context).await.unwrap()),
            low_version
        );
        let version = report_dataver(&read_bytes(&handler, &context).await.unwrap());
        context.changes.borrow_mut().clear();
        function.observe_measurement(1.00001, Some(5), 6).unwrap();
        assert!(poll_once(run.as_mut()).await.is_none());
        assert!(context.changes.borrow().is_empty());
        assert_eq!(
            report_dataver(&read_bytes(&handler, &context).await.unwrap()),
            version
        );
    });
}

#[test]
fn an_unrepresentable_illuminance_range_preserves_native_values_without_spurious_reports() {
    setup!(@device device, handler, im, illuminance_sensor(MeasurementRange::new(0.0, 1e10).unwrap()));
    block_on(async {
        let context = Context::at(&im, 3, 0x400, 0);
        let metadata = Context::at(&im, 3, 0x400, 0xfffc);
        let mut run = pin!(handler.run(&context));
        assert!(poll_once(run.as_mut()).await.is_none());
        let version = report_dataver(&read_bytes(&handler, &metadata).await.unwrap());
        for lux in [0.125, 1000.0] {
            device.functions()[0]
                .observe_measurement(lux, None, 1)
                .unwrap();
            assert!(poll_once(run.as_mut()).await.is_none());
            assert_eq!(
                read_bytes(&handler, &context).await.unwrap_err().code(),
                ErrorCode::ConstraintError
            );
            assert_eq!(
                report_dataver(&read_bytes(&handler, &metadata).await.unwrap()),
                version
            );
            assert!(context.changes.borrow().is_empty());
            assert_eq!(
                device.functions()[0]
                    .snapshot()
                    .measurement()
                    .unwrap()
                    .value,
                Some(lux)
            );
            assert!(measurement_mapping_issue(&device.functions()[0]).is_some());
        }
    });
}
