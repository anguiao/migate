use super::support::{
    Context, attr, fan_device, fan_model, model_handler, observe_fan_state, read_bytes,
    report_dataver, report_value, single_entry,
};
use crate::device::FanValue;
use futures_lite::future::{block_on, poll_once};
use rs_matter::{
    dm::{AsyncHandler, Metadata},
    error::ErrorCode,
};
use std::pin::pin;

#[path = "fan/commands.rs"]
mod commands;
#[path = "fan/metadata.rs"]
mod metadata;
#[path = "fan/reports.rs"]
mod reports;

#[test]
fn fan_endpoint_publishes_confirmed_device_type_cluster_and_shape() {
    for swing in [false, true] {
        let device = fan_device("fan", swing).0;
        let model = model_handler(vec![single_entry(&device)]);
        model.access(|node| {
            let endpoint = node.endpoint(2).unwrap();
            assert!(
                endpoint
                    .device_types
                    .iter()
                    .any(|device| device.dtype == 0x2b && device.drev == 4)
            );
            assert!(
                endpoint
                    .device_types
                    .iter()
                    .any(|device| device.dtype == 0x13)
            );
            assert_eq!(
                endpoint
                    .clusters
                    .iter()
                    .map(|cluster| cluster.id)
                    .collect::<Vec<_>>(),
                [0x1d, 4, 3, 0x39, 6, 0x202]
            );
            let cluster = endpoint.cluster(0x202).unwrap();
            assert_eq!(cluster.revision, 6);
            assert_eq!(cluster.feature_map, if swing { 5 } else { 1 });
            for id in 0..=6 {
                assert!(cluster.attribute(id).is_some());
            }
            for id in [7, 8] {
                assert_eq!(cluster.attribute(id).is_some(), swing);
            }
            for id in [9, 10, 11] {
                assert!(cluster.attribute(id).is_none());
            }
            assert!(cluster.command(0).is_none());
        });
    }
}
#[test]
fn fan_read_projection_uses_power_and_retained_speed_only() {
    let device = fan_device("fan", true).0;
    fan_model!(&device, handler, im);
    let mut ctx = Context::new(&im, 0x202, 0);
    let function = &device.functions()[0];
    block_on(async {
        for id in [0, 2, 3, 5, 6, 8] {
            ctx.attribute = attr(0x202, id);
            assert_eq!(
                read_bytes(&handler, &ctx).await.unwrap_err().code(),
                ErrorCode::Failure
            );
        }
        function.observe_fan(FanValue::Speed(40), Some(1), 1);
        function.observe_fan(FanValue::Mode(1), Some(1), 1);
        ctx.attribute = attr(0x202, 3);
        assert_eq!(
            read_bytes(&handler, &ctx).await.unwrap_err().code(),
            ErrorCode::Failure
        );
        function.observe_fan(FanValue::Power(true), Some(2), 2);
        function.observe_fan(FanValue::Swing(true), Some(2), 2);
        for (id, value) in [
            (0, 2),
            (2, 40),
            (3, 40),
            (4, 100),
            (5, 40),
            (6, 40),
            (7, 1),
            (8, 1),
        ] {
            ctx.attribute = attr(0x202, id);
            assert_eq!(
                report_value(&read_bytes(&handler, &ctx).await.unwrap())
                    .u8()
                    .unwrap(),
                value
            );
        }
        for (speed, mode) in [(1, 1), (33, 1), (34, 2), (66, 2), (67, 3), (100, 3)] {
            function.observe_fan(FanValue::Speed(speed), Some(3), 3);
            ctx.attribute = attr(0x202, 0);
            assert_eq!(
                report_value(&read_bytes(&handler, &ctx).await.unwrap())
                    .u8()
                    .unwrap(),
                mode
            );
        }
        function.observe_fan(FanValue::Power(false), Some(4), 4);
        for id in [0, 2, 3, 5, 6] {
            ctx.attribute = attr(0x202, id);
            assert_eq!(
                report_value(&read_bytes(&handler, &ctx).await.unwrap())
                    .u8()
                    .unwrap(),
                0
            );
        }
        assert_eq!(function.snapshot().fan().unwrap().speed.value, Some(100));
        let before = report_dataver(&read_bytes(&handler, &ctx).await.unwrap());
        function.observe_fan(FanValue::Mode(0), Some(5), 5);
        function.observe_fan(FanValue::Speed(70), Some(5), 5);
        assert_eq!(
            report_dataver(&read_bytes(&handler, &ctx).await.unwrap()),
            before
        );
        let mut run = pin!(handler.run(&ctx));
        assert!(poll_once(run.as_mut()).await.is_none());
    });
}
