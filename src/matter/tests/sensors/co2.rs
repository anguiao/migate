//! Carbon dioxide cluster metadata, projection and reports.

use super::*;

fn co2_sensor(range: MeasurementRange) -> PhysicalDevice {
    let (physical, receivers) = PhysicalDevice::new(
        PhysicalDeviceDescription::new(
            DeviceKey::new("xiaomi", "co2"),
            "CO₂ sensor",
            vec![FunctionDescription {
                id: "co2".into(),
                label: "CO₂".into(),
                kind: FunctionKind::CarbonDioxide(range),
            }],
        )
        .unwrap()
        .with_battery(),
    );
    assert!(receivers.is_empty());
    physical
}

#[test]
fn co2_publishes_required_clusters_with_nullable_single_precision_ppm_in_air() {
    setup!(@device device, handler, im, co2_sensor(MeasurementRange::new(0.0, 5000.0).unwrap()));
    handler.access(|node| {
        let parent = node
            .endpoints
            .iter()
            .find(|endpoint| endpoint.id == 2)
            .unwrap();
        assert!(parent.clusters.iter().any(|cluster| cluster.id == 0x2f));
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
            [(0x002c, 1)]
        );
        assert_eq!(
            child
                .clusters
                .iter()
                .map(|cluster| cluster.id)
                .collect::<Vec<_>>(),
            [0x1d, 3, 0x5b, 0x40d]
        );
        let concentration = child
            .clusters
            .iter()
            .find(|cluster| cluster.id == 0x40d)
            .unwrap();
        assert_eq!(concentration.feature_map, 1);
        assert_eq!(concentration.revision, 5);
        for attribute in [0, 1, 2, 8, 9, 0xfff8, 0xfff9, 0xfffb, 0xfffc, 0xfffd] {
            assert!(concentration.attribute(attribute).is_some());
        }
        for attribute in [3, 4, 5, 6, 7, 10] {
            assert!(concentration.attribute(attribute).is_none());
        }
        assert!(concentration.commands.is_empty());
        let air_quality = child
            .clusters
            .iter()
            .find(|cluster| cluster.id == 0x5b)
            .unwrap();
        assert_eq!(air_quality.revision, 1);
        assert_eq!(air_quality.feature_map, 0);
    });
    block_on(async {
        for (attribute, expected) in [(1, 0.0_f32), (2, 5000.0_f32)] {
            let ctx = Context::at(&im, 3, 0x40d, attribute);
            assert_eq!(
                report_value(&read_bytes(&handler, &ctx).await.unwrap())
                    .f32()
                    .unwrap(),
                expected
            );
        }
        for (attribute, expected) in [(8, 0), (9, 0)] {
            let ctx = Context::at(&im, 3, 0x40d, attribute);
            assert_eq!(
                report_value(&read_bytes(&handler, &ctx).await.unwrap())
                    .u8()
                    .unwrap(),
                expected
            );
        }
        let ctx = Context::at(&im, 3, 0x40d, 0);
        assert!(
            report_value(&read_bytes(&handler, &ctx).await.unwrap())
                .null()
                .is_ok()
        );
        let co2 = &device.functions()[0];
        for value in [0.0, 650.125, 5000.0] {
            co2.observe_measurement(value, Some(10), 20).unwrap();
            assert_eq!(
                report_value(&read_bytes(&handler, &ctx).await.unwrap())
                    .f32()
                    .unwrap(),
                value as f32
            );
        }
        let air = Context::at(&im, 3, 0x5b, 0);
        assert_eq!(
            report_value(&read_bytes(&handler, &air).await.unwrap())
                .u8()
                .unwrap(),
            0
        );
        assert_eq!(
            handler.write(&ctx).await.unwrap_err().code(),
            ErrorCode::AttributeNotFound
        );
        assert_eq!(
            handler.write(&air).await.unwrap_err().code(),
            ErrorCode::AttributeNotFound
        );
        let endpoints = Context::at(&im, 2, 0x2f, 31);
        assert_eq!(
            report_value(&read_bytes(&handler, &endpoints).await.unwrap())
                .array()
                .unwrap()
                .iter()
                .map(|value| value.unwrap().u16().unwrap())
                .collect::<Vec<_>>(),
            [2, 3]
        );
    });
}

#[test]
fn co2_reports_only_single_precision_value_changes_and_keeps_air_quality_unknown() {
    setup!(@device device, handler, im, co2_sensor(MeasurementRange::new(0.0, 5000.0).unwrap()));
    block_on(async {
        let ctx = Context::at(&im, 3, 0x40d, 0);
        let air = Context::at(&im, 3, 0x5b, 0);
        let air_version = report_dataver(&read_bytes(&handler, &air).await.unwrap());
        let mut run = pin!(handler.run(&ctx));
        assert!(poll_once(run.as_mut()).await.is_none());
        ctx.changes.borrow_mut().clear();
        let co2 = &device.functions()[0];
        co2.observe_measurement(650.0, Some(100), 200).unwrap();
        assert!(poll_once(run.as_mut()).await.is_none());
        assert_eq!(ctx.changes.borrow().as_slice(), [(3, 0x40d, 0)]);
        let version = report_dataver(&read_bytes(&handler, &ctx).await.unwrap());
        ctx.changes.borrow_mut().clear();
        co2.observe_measurement(650.000001, Some(101), 201).unwrap();
        assert!(poll_once(run.as_mut()).await.is_none());
        assert!(ctx.changes.borrow().is_empty());
        assert_eq!(
            report_dataver(&read_bytes(&handler, &ctx).await.unwrap()),
            version
        );
        co2.observe_measurement(651.0, None, 202).unwrap();
        assert!(poll_once(run.as_mut()).await.is_none());
        assert_eq!(ctx.changes.borrow().as_slice(), [(3, 0x40d, 0)]);
        assert_ne!(
            report_dataver(&read_bytes(&handler, &ctx).await.unwrap()),
            version
        );
        assert_eq!(
            report_dataver(&read_bytes(&handler, &air).await.unwrap()),
            air_version
        );
        assert_eq!(
            report_value(&read_bytes(&handler, &air).await.unwrap())
                .u8()
                .unwrap(),
            0
        );
        assert_eq!(co2.snapshot().measurement().unwrap().source_time, None);
    });
}

#[test]
fn co2_projection_uses_the_target_float_bounds_without_changing_native_values() {
    assert_eq!(units::co2_matter_value(f32::MAX as f64), Some(f32::MAX));
    assert_eq!(units::co2_matter_value((f32::MAX as f64) * 2.0), None);
    assert_eq!(
        units::co2_matter_bounds(MeasurementRange::new(400.0, 5000.0).unwrap()),
        Some((400.0, 5000.0))
    );
    setup!(@device device, handler, im, co2_sensor(MeasurementRange::new(0.0, f64::MAX).unwrap()));
    let co2 = &device.functions()[0];
    block_on(async {
        let ctx = Context::at(&im, 3, 0x40d, 0);
        let mut run = pin!(handler.run(&ctx));
        assert!(poll_once(run.as_mut()).await.is_none());
        let version = report_dataver(
            &read_bytes(&handler, &Context::at(&im, 3, 0x40d, 0xfffc))
                .await
                .unwrap(),
        );
        for value in [400.0, f64::MAX] {
            co2.observe_measurement(value, None, 1).unwrap();
            assert!(poll_once(run.as_mut()).await.is_none());
            assert!(ctx.changes.borrow().is_empty());
            assert_eq!(
                read_bytes(&handler, &ctx).await.unwrap_err().code(),
                ErrorCode::ConstraintError
            );
            assert_eq!(
                report_dataver(
                    &read_bytes(&handler, &Context::at(&im, 3, 0x40d, 0xfffc))
                        .await
                        .unwrap()
                ),
                version
            );
            assert_eq!(co2.snapshot().measurement().unwrap().value, Some(value));
            assert!(measurement_mapping_issue(co2).is_some());
        }
    });
}
