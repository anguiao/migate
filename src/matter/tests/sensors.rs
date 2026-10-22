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

fn sensor() -> PhysicalDevice {
    let (device, receivers) = PhysicalDevice::new(
        PhysicalDeviceDescription::new(
            DeviceKey::new("xiaomi", "sensor"),
            "Sensor",
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
        .unwrap(),
    );
    assert!(receivers.is_empty());
    device
}

macro_rules! setup {
    ($device:ident, $handler:ident, $im:ident) => {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::storage::Store::open(dir.path()).unwrap();
        let identity = store.load_identity().unwrap();
        let $device = sensor();
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
    assert_eq!(
        units::temperature_matter_range(MeasurementRange::new(-30.0, 100.0).unwrap()),
        Some((-3000, 10000))
    );
    assert_eq!(
        units::humidity_matter_range(MeasurementRange::new(0.0, 100.0).unwrap()),
        Some((0, 10000))
    );
    assert_eq!(
        units::temperature_matter_range(MeasurementRange::new(-273.15, 327.67).unwrap()),
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
