//! Matter temperature and humidity sensor publication.

use super::super::{model::basic_info, storage::StoreAdapter, units};
use super::support::{
    Context, attr, entry, model_handler, read_bytes, report_dataver, report_value,
};
use crate::device::{
    Availability, DeviceKey, FunctionDescription, FunctionKind, MeasurementRange, PhysicalDevice,
    PhysicalDeviceDescription,
};
use futures_lite::future::{block_on, poll_once};
use rs_matter::{
    MATTER_PORT, Matter,
    crypto::default_crypto,
    dm::{
        AsyncHandler, Metadata,
        devices::test::{DAC_PRIVKEY, TEST_DEV_ATT, TEST_DEV_COMM},
        networks::eth::EthNetwork,
    },
    error::ErrorCode,
    im::{EthInteractionModelState, InteractionModel},
    tlv::TLVElement,
    transport::exchange::MatterBuffers,
};
use std::pin::pin;

fn battery_sensor(battery: bool) -> PhysicalDevice {
    battery_sensor_named("sensor", "Sensor", battery)
}

fn battery_sensor_named(id: &str, label: &str, battery: bool) -> PhysicalDevice {
    let description = PhysicalDeviceDescription::new(
        DeviceKey::new("xiaomi", id),
        label,
        vec![
            FunctionDescription {
                id: "temperature".into(),
                label: "Temperature".into(),
                kind: FunctionKind::Temperature(MeasurementRange::new(-30.0, 100.0).unwrap()),
            },
            FunctionDescription {
                id: "humidity".into(),
                label: "Humidity".into(),
                kind: FunctionKind::Humidity(MeasurementRange::new(0.0, 100.0).unwrap()),
            },
        ],
    )
    .unwrap();
    let description = if battery {
        description.with_battery()
    } else {
        description
    };
    let (device, receivers) = PhysicalDevice::new(description);
    assert!(receivers.is_empty());
    device
}

macro_rules! setup {
    ($device:ident, $handler:ident, $im:ident) => {
        setup!($device, $handler, $im, false);
    };
    ($device:ident, $handler:ident, $im:ident, $battery:expr) => {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::storage::Store::open(dir.path()).unwrap();
        let identity = store.load_identity().unwrap();
        let $device = battery_sensor($battery);
        let info = basic_info(&identity);
        let matter = Matter::new(&info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
        let buffers: MatterBuffers = MatterBuffers::new();
        let state: EthInteractionModelState =
            EthInteractionModelState::new(EthNetwork::new_default());
        let crypto = default_crypto(rand::rng(), DAC_PRIVKEY);
        let kv = matter.kv(StoreAdapter::new(store.matter()));
        let $handler = model_handler(vec![entry(
            &$device,
            2,
            &[("temperature", 3), ("humidity", 4)],
        )]);
        let $im = InteractionModel::new(&matter, &crypto, &buffers, &$handler, &kv, &state);
    };
}

#[test]
fn a_single_measurement_keeps_a_physical_parent_and_battery() {
    for kind in [
        FunctionKind::Temperature(MeasurementRange::new(-30.0, 100.0).unwrap()),
        FunctionKind::Humidity(MeasurementRange::new(0.0, 100.0).unwrap()),
    ] {
        let (device, _) = PhysicalDevice::new(
            PhysicalDeviceDescription::new(
                DeviceKey::new("xiaomi", "single"),
                "Single",
                vec![FunctionDescription {
                    id: "measurement".into(),
                    label: "Measurement".into(),
                    kind,
                }],
            )
            .unwrap()
            .with_battery(),
        );
        let handler = model_handler(vec![entry(&device, 2, &[("measurement", 3)])]);
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
            assert_eq!(child.device_types.len(), 1);
            assert!(
                child
                    .clusters
                    .iter()
                    .any(|cluster| cluster.id == 0x402 || cluster.id == 0x405)
            );
        });
    }
}

#[test]
fn mixed_light_children_publish_their_real_light_profiles() {
    use crate::device::{LightProfile, TemperatureInterval};
    let interval = TemperatureInterval::new(2700, 6500).unwrap();
    for (profile, device_type, expected_clusters) in [
        (LightProfile::OnOff, 0x0100, vec![6]),
        (LightProfile::Dimmable, 0x0101, vec![6, 8]),
        (
            LightProfile::ColorTemperature(interval),
            0x010c,
            vec![6, 8, 0x300],
        ),
        (
            LightProfile::ExtendedColor(interval),
            0x010d,
            vec![6, 8, 0x300],
        ),
    ] {
        let (device, _) = PhysicalDevice::new(
            PhysicalDeviceDescription::new(
                DeviceKey::new("xiaomi", "mixed"),
                "Mixed",
                vec![
                    FunctionDescription {
                        id: "main".into(),
                        label: "Light".into(),
                        kind: FunctionKind::Light(profile),
                    },
                    FunctionDescription {
                        id: "temperature".into(),
                        label: "Temperature".into(),
                        kind: FunctionKind::Temperature(
                            MeasurementRange::new(-30.0, 100.0).unwrap(),
                        ),
                    },
                ],
            )
            .unwrap(),
        );
        let handler = model_handler(vec![entry(&device, 2, &[("main", 3), ("temperature", 4)])]);
        handler.access(|node| {
            let light = node
                .endpoints
                .iter()
                .find(|endpoint| endpoint.id == 3)
                .unwrap();
            assert_eq!(
                light
                    .device_types
                    .iter()
                    .map(|kind| kind.dtype)
                    .collect::<Vec<_>>(),
                [device_type]
            );
            for cluster in [6, 8, 0x300] {
                assert_eq!(
                    light.clusters.iter().any(|metadata| metadata.id == cluster),
                    expected_clusters.contains(&cluster)
                );
            }
            assert!(!light.clusters.iter().any(|cluster| cluster.id == 0x39));
            assert!(light.clusters.iter().any(|cluster| cluster.id == 0x1d));
            assert!(light.clusters.iter().any(|cluster| cluster.id == 3));
        });
    }
}

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
    assert!(super::super::measurement_mapping_issue(function).is_some());
}

#[test]
fn an_unsupported_measurement_range_has_a_stable_version_and_no_value_reports() {
    use crate::matter::sensors::TemperatureHandler;
    use rs_matter::dm::{Dataver, clusters::decl::temperature_measurement::ClusterHandler};

    setup!(_baseline, _model, im);
    let (physical, _) = PhysicalDevice::new(
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
    let function = &physical.functions()[0];
    let handler = TemperatureHandler::new(function.clone(), 3, Dataver::new(100)).unwrap();
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
            assert!(super::super::measurement_mapping_issue(function).is_some());
        }
    });
}

#[test]
fn sensor_publishes_parent_and_two_measurement_child_endpoints() {
    setup!(device, handler, _im);
    handler.access(|node| {
        let parent = node
            .endpoints
            .iter()
            .find(|endpoint| endpoint.id == 2)
            .unwrap();
        assert_eq!(
            parent
                .device_types
                .iter()
                .map(|kind| (kind.dtype, kind.drev))
                .collect::<Vec<_>>(),
            [(0x0013, 1)]
        );
        let parent_clusters = parent
            .clusters
            .iter()
            .map(|cluster| cluster.id)
            .collect::<Vec<_>>();
        assert!(parent_clusters.contains(&0x1d));
        assert!(parent_clusters.contains(&0x39));
        assert!(!parent_clusters.contains(&0x402));
        assert!(!parent_clusters.contains(&0x405));

        for (endpoint_id, device_type, device_revision, cluster_id, revision) in
            [(3, 0x0302, 3, 0x0402, 4), (4, 0x0307, 2, 0x0405, 3)]
        {
            let endpoint = node
                .endpoints
                .iter()
                .find(|endpoint| endpoint.id == endpoint_id)
                .unwrap();
            assert_eq!(
                endpoint
                    .device_types
                    .iter()
                    .map(|kind| (kind.dtype, kind.drev))
                    .collect::<Vec<_>>(),
                [(device_type, device_revision)]
            );
            let cluster = endpoint
                .clusters
                .iter()
                .find(|cluster| cluster.id == cluster_id)
                .unwrap();
            assert_eq!(cluster.revision, revision);
            assert_eq!(cluster.feature_map, 0);
            for attribute in [0, 1, 2, 0xfff8, 0xfff9, 0xfffb, 0xfffc, 0xfffd] {
                assert!(cluster.attribute(attribute).is_some(), "{attribute}");
            }
            assert!(cluster.attribute(3).is_none(), "no fake Tolerance");
            assert!(cluster.command(0).is_none());
            assert!(endpoint.clusters.iter().any(|cluster| cluster.id == 3));
            assert!(endpoint.clusters.iter().any(|cluster| cluster.id == 0x1d));
            assert!(!endpoint.clusters.iter().any(|cluster| cluster.id == 6));
            assert!(
                !endpoint
                    .clusters
                    .iter()
                    .any(|cluster| matches!(cluster.id, 8 | 0x300 | 4))
            );
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
        MeasurementRange::new(-30.0, 100.0)
            .unwrap()
            .temperature_bounds(),
        Some((-3000, 10000))
    );
    assert_eq!(
        MeasurementRange::new(0.0, 100.0).unwrap().humidity_bounds(),
        Some((0, 10000))
    );
    assert_eq!(
        MeasurementRange::new(-273.15, 327.67)
            .unwrap()
            .temperature_bounds(),
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

#[test]
fn battery_capability_adds_power_source_to_the_parent_only() {
    setup!(device, handler, _im, true);
    handler.access(|node| {
        let parent = node
            .endpoints
            .iter()
            .find(|endpoint| endpoint.id == 2)
            .unwrap();
        assert_eq!(
            parent
                .device_types
                .iter()
                .map(|kind| (kind.dtype, kind.drev))
                .collect::<Vec<_>>(),
            [(0x0013, 1), (0x0011, 1)]
        );
        let power = parent
            .clusters
            .iter()
            .find(|cluster| cluster.id == 0x002f)
            .unwrap();
        assert_eq!(power.revision, 3);
        assert_eq!(power.feature_map, 0x02);
        for attribute in [
            0, 1, 2, 12, 14, 15, 16, 31, 0xfff8, 0xfff9, 0xfffb, 0xfffc, 0xfffd,
        ] {
            assert!(
                power.attribute(attribute).is_some(),
                "attribute {attribute}"
            );
        }
        for attribute in [
            3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30,
        ] {
            assert!(
                power.attribute(attribute).is_none(),
                "unexpected {attribute}"
            );
        }
        assert!(power.command(0).is_none());
        for event in [0, 1, 2] {
            assert!(power.event(event).is_none(), "unexpected event {event}");
        }
        for endpoint_id in [3, 4] {
            let endpoint = node
                .endpoints
                .iter()
                .find(|endpoint| endpoint.id == endpoint_id)
                .unwrap();
            assert!(!endpoint.clusters.iter().any(|cluster| cluster.id == 0x002f));
            assert!(
                !endpoint
                    .device_types
                    .iter()
                    .any(|kind| kind.dtype == 0x0011)
            );
        }
        assert!(!node.endpoints.iter().any(|endpoint| endpoint.id > 4));
    });
}

#[test]
fn without_battery_the_parent_has_no_power_source_cluster() {
    setup!(device, handler, _im, false);
    handler.access(|node| {
        let parent = node
            .endpoints
            .iter()
            .find(|endpoint| endpoint.id == 2)
            .unwrap();
        assert_eq!(
            parent
                .device_types
                .iter()
                .map(|kind| (kind.dtype, kind.drev))
                .collect::<Vec<_>>(),
            [(0x0013, 1)]
        );
        assert!(!parent.clusters.iter().any(|cluster| cluster.id == 0x002f));
    });
}

#[test]
fn battery_attributes_are_nullable_shared_and_fail_before_the_first_value() {
    setup!(device, handler, im, true);
    block_on(async {
        let ctx = Context::at(&im, 2, 0x002f, 12);
        assert!(
            report_value(&read_bytes(&handler, &ctx).await.unwrap())
                .null()
                .is_ok()
        );
        for attribute in [14, 15] {
            let ctx = Context::at(&im, 2, 0x002f, attribute);
            let mut buffer = [0; 4096];
            let mut write = rs_matter::utils::storage::WriteBuf::new(&mut buffer);
            let error = handler
                .read(
                    &ctx,
                    rs_matter::dm::ReadReplyInstance::new(&ctx.attribute, &mut write),
                )
                .await
                .unwrap_err();
            assert_eq!(error.code(), ErrorCode::Failure, "attribute {attribute}");
        }
        for (attribute, expected) in [(0, 0), (1, 0), (16, 0)] {
            let ctx = Context::at(&im, 2, 0x002f, attribute);
            assert_eq!(
                report_value(&read_bytes(&handler, &ctx).await.unwrap())
                    .u8()
                    .unwrap(),
                expected,
                "attribute {attribute}"
            );
        }
        let ctx = Context::at(&im, 2, 0x002f, 2);
        assert_eq!(
            report_value(&read_bytes(&handler, &ctx).await.unwrap())
                .utf8()
                .unwrap(),
            "Battery"
        );
        device.observe_battery(55.0, Some(1), 2).unwrap();
        let ctx = Context::at(&im, 2, 0x002f, 12);
        assert_eq!(
            report_value(&read_bytes(&handler, &ctx).await.unwrap())
                .u8()
                .unwrap(),
            110
        );
        let ctx = Context::at(&im, 2, 0x002f, 14);
        assert_eq!(
            report_value(&read_bytes(&handler, &ctx).await.unwrap())
                .u8()
                .unwrap(),
            0
        );
        let ctx = Context::at(&im, 2, 0x002f, 15);
        assert!(
            !report_value(&read_bytes(&handler, &ctx).await.unwrap())
                .bool()
                .unwrap()
        );
        let ctx = Context::at(&im, 2, 0x002f, 31);
        assert_eq!(
            report_value(&read_bytes(&handler, &ctx).await.unwrap())
                .array()
                .unwrap()
                .iter()
                .map(|value| value.unwrap().u16().unwrap())
                .collect::<Vec<_>>(),
            [2, 3, 4]
        );
    });
}

#[test]
fn battery_thresholds_and_adjacent_percentages_share_one_value() {
    setup!(device, handler, im, true);
    block_on(async {
        for (percent, doubled, level, replacement) in [
            (0.0, 0, 2, true),
            (1.0, 2, 2, true),
            (4.0, 8, 2, true),
            (5.0, 10, 2, true),
            (6.0, 12, 1, false),
            (9.0, 18, 1, false),
            (10.0, 20, 1, false),
            (11.0, 22, 0, false),
            (100.0, 200, 0, false),
        ] {
            device
                .observe_battery(percent, Some(percent as i64), 1)
                .unwrap();
            for (attribute, expected) in [(12, doubled), (14, level)] {
                let ctx = Context::at(&im, 2, 0x002f, attribute);
                assert_eq!(
                    report_value(&read_bytes(&handler, &ctx).await.unwrap())
                        .u8()
                        .unwrap(),
                    expected,
                    "{percent}% attribute {attribute}"
                );
            }
            let ctx = Context::at(&im, 2, 0x002f, 15);
            assert_eq!(
                report_value(&read_bytes(&handler, &ctx).await.unwrap())
                    .bool()
                    .unwrap(),
                replacement,
                "{percent}%"
            );
        }
    });
}

#[test]
fn battery_attributes_are_read_only() {
    setup!(device, handler, im, true);
    block_on(async {
        device.observe_battery(55.0, Some(1), 2).unwrap();
        for attribute in [0, 1, 2, 12, 14, 15, 16, 31] {
            let mut ctx = Context::at(&im, 2, 0x002f, attribute);
            ctx.data = TLVElement::new(&[0x15, 0x18]);
            let error = handler.write(&ctx).await.unwrap_err();
            assert_eq!(
                error.code(),
                ErrorCode::AttributeNotFound,
                "attribute {attribute}"
            );
        }
    });
}

#[test]
fn battery_changes_bump_versions_and_notify_projected_attribute_edges_only() {
    setup!(device, handler, im, true);
    block_on(async {
        let ctx = Context::at(&im, 2, 0x002f, 12);
        let mut run = pin!(handler.run(&ctx));
        assert!(poll_once(run.as_mut()).await.is_none());
        ctx.changes.borrow_mut().clear();

        device.observe_battery(10.0, Some(1), 2).unwrap();
        assert!(poll_once(run.as_mut()).await.is_none());
        for attribute in [12, 14, 15] {
            assert!(ctx.changes.borrow().contains(&(2, 0x002f, attribute)));
        }
        let version = {
            let bytes = read_bytes(&handler, &ctx).await.unwrap();
            report_dataver(&bytes)
        };
        let count = ctx.changes.borrow().len();
        // A timestamp-only update must not fabricate a report or bump dataver.
        device.observe_battery(10.0, Some(2), 3).unwrap();
        assert!(poll_once(run.as_mut()).await.is_none());
        assert_eq!(ctx.changes.borrow().len(), count);
        assert_eq!(
            report_dataver(&read_bytes(&handler, &ctx).await.unwrap()),
            version
        );

        ctx.changes.borrow_mut().clear();
        device.observe_battery(5.0, Some(3), 4).unwrap();
        assert!(poll_once(run.as_mut()).await.is_none());
        for attribute in [12, 14, 15] {
            assert!(ctx.changes.borrow().contains(&(2, 0x002f, attribute)));
        }
        assert_ne!(
            report_dataver(&read_bytes(&handler, &ctx).await.unwrap()),
            version
        );
    });
}

#[test]
fn multiple_sensor_parents_keep_isolated_battery_state_and_endpoint_lists() {
    let dir = tempfile::tempdir().unwrap();
    let store = crate::storage::Store::open(dir.path()).unwrap();
    let identity = store.load_identity().unwrap();
    let info = basic_info(&identity);
    let matter = Matter::new(&info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
    let buffers: MatterBuffers = MatterBuffers::new();
    let state: EthInteractionModelState = EthInteractionModelState::new(EthNetwork::new_default());
    let crypto = default_crypto(rand::rng(), DAC_PRIVKEY);
    let kv = matter.kv(StoreAdapter::new(store.matter()));
    let first = battery_sensor_named("one", "Sensor One", true);
    let second = battery_sensor_named("two", "Sensor Two", true);
    let handler = model_handler(vec![
        entry(&first, 2, &[("temperature", 3), ("humidity", 4)]),
        entry(&second, 5, &[("temperature", 6), ("humidity", 7)]),
    ]);
    let im = InteractionModel::new(&matter, &crypto, &buffers, &handler, &kv, &state);
    block_on(async {
        first.observe_battery(11.0, Some(1), 2).unwrap();
        for (endpoint, expected) in [(2, [2, 3, 4]), (5, [5, 6, 7])] {
            let ctx = Context::at(&im, endpoint, 0x002f, 31);
            assert_eq!(
                report_value(&read_bytes(&handler, &ctx).await.unwrap())
                    .array()
                    .unwrap()
                    .iter()
                    .map(|value| value.unwrap().u16().unwrap())
                    .collect::<Vec<_>>(),
                expected
            );
            let ctx = Context::at(&im, endpoint, 0x002f, 12);
            let bytes = read_bytes(&handler, &ctx).await.unwrap();
            let value = report_value(&bytes);
            if endpoint == 2 {
                assert_eq!(value.u8().unwrap(), 22);
            } else {
                assert!(value.null().is_ok());
            }
        }
    });
}
