use super::support::{
    Context, attr, fan_model, model_handler, read_bytes, report_dataver, report_value, single_entry,
};
use crate::device::{
    AirConditionerCommand, AirConditionerFanSpeed as Speed, AirConditionerMode as Mode,
    AirConditionerStep as Step, AirConditionerValue as Value, Availability, CommandReceiver,
    DeviceKey, DeviceRequest, FunctionDescription, FunctionKind, PhysicalDevice,
    PhysicalDeviceDescription, TemperatureGrid,
};
use futures_lite::future::{block_on, poll_once};
use rs_matter::{
    dm::{AsyncHandler, InvokeReplyInstance, Metadata},
    error::ErrorCode,
    tlv::TLVElement,
    utils::storage::WriteBuf,
};
use std::pin::pin;

fn device(swing: bool) -> (PhysicalDevice, CommandReceiver) {
    let (device, mut receivers) = PhysicalDevice::new(
        PhysicalDeviceDescription::new(
            DeviceKey::new("xiaomi", "ac"),
            "AC",
            vec![FunctionDescription {
                id: "air-conditioner:3".into(),
                label: "AC".into(),
                kind: FunctionKind::AirConditioner {
                    temperature: TemperatureGrid::new(16, 30, 1).unwrap(),
                    vertical_swing: swing,
                },
            }],
        )
        .unwrap(),
    );
    device.set_availability(Availability::Available);
    (device, receivers.remove(0))
}

#[test]
fn publishes_room_ac_and_only_declared_features() {
    for swing in [false, true] {
        let device = device(swing).0;
        let model = model_handler(vec![single_entry(&device)]);
        model.access(|node| {
            let endpoint = node.endpoint(2).unwrap();
            assert!(
                endpoint
                    .device_types
                    .iter()
                    .any(|kind| kind.dtype == 0x72 && kind.drev == 3)
            );
            assert_eq!(endpoint.cluster(6).unwrap().feature_map, 2);
            let thermostat = endpoint.cluster(0x201).unwrap();
            assert_eq!((thermostat.revision, thermostat.feature_map), (11, 99));
            for id in [0, 3, 4, 5, 6, 17, 18, 25, 27, 28] {
                assert!(thermostat.attribute(id).is_some());
            }
            for id in [7, 8, 19, 20, 21, 22, 23, 24, 30, 41] {
                assert!(thermostat.attribute(id).is_none());
            }
            assert!(thermostat.command(0).is_some());
            assert!(thermostat.command(1).is_none());
            let fan = endpoint.cluster(0x202).unwrap();
            assert_eq!(
                (fan.revision, fan.feature_map),
                (6, if swing { 7 } else { 3 })
            );
            assert_eq!(fan.attribute(7).is_some(), swing);
            assert_eq!(fan.attribute(8).is_some(), swing);
        });
    }
}

#[test]
fn automatic_fan_and_thermal_reads_never_invent_measurements() {
    let device = device(true).0;
    fan_model!(&device, handler, im);
    let function = &device.functions()[0];
    block_on(async {
        let mut ctx = Context::new(&im, 0x201, 0);
        assert!(
            report_value(&read_bytes(&handler, &ctx).await.unwrap())
                .null()
                .is_ok()
        );
        for (cluster, id) in [
            (6, 0),
            (0x201, 17),
            (0x201, 18),
            (0x201, 28),
            (0x202, 0),
            (0x202, 2),
            (0x202, 3),
            (0x202, 5),
            (0x202, 6),
        ] {
            ctx.attribute = attr(cluster, id);
            assert_eq!(
                read_bytes(&handler, &ctx).await.unwrap_err().code(),
                ErrorCode::Failure,
                "{cluster:x}.{id}"
            );
        }
        for value in [
            Value::Power(true),
            Value::Mode(Mode::Heat),
            Value::Temperature(24),
            Value::Speed(Speed::Auto),
            Value::Swing(true),
        ] {
            function.observe_air_conditioner(value, Some(1), 1);
        }
        for (cluster, id, expected) in [
            (0x201, 28, 4),
            (0x202, 0, 5),
            (0x202, 1, 2),
            (0x202, 4, 3),
            (0x202, 7, 2),
            (0x202, 8, 2),
        ] {
            ctx.attribute = attr(cluster, id);
            assert_eq!(
                report_value(&read_bytes(&handler, &ctx).await.unwrap())
                    .u8()
                    .unwrap(),
                expected
            );
        }
        for id in [17, 18] {
            ctx.attribute = attr(0x201, id);
            assert_eq!(
                report_value(&read_bytes(&handler, &ctx).await.unwrap())
                    .i16()
                    .unwrap(),
                2400
            );
        }
        for id in [2, 5] {
            ctx.attribute = attr(0x202, id);
            assert!(
                report_value(&read_bytes(&handler, &ctx).await.unwrap())
                    .null()
                    .is_ok()
            );
        }
        for id in [3, 6] {
            ctx.attribute = attr(0x202, id);
            assert_eq!(
                read_bytes(&handler, &ctx).await.unwrap_err().code(),
                ErrorCode::Failure
            );
        }
    });
}

#[test]
fn writes_wait_for_receipts_null_is_noop_and_auto_is_not_off() {
    let (device, mut receiver) = device(true);
    fan_model!(&device, handler, im);
    let function = &device.functions()[0];
    function.observe_air_conditioner(Value::Power(true), Some(1), 1);
    function.observe_air_conditioner(Value::Mode(Mode::Cool), Some(1), 1);
    block_on(async {
        for id in [2, 5] {
            let mut ctx = Context::new(&im, 0x202, id);
            ctx.data = TLVElement::new(&[0x14]);
            handler.write(&ctx).await.unwrap();
            assert!(poll_once(receiver.next_operation()).await.is_none());
        }
        for (attribute, raw, expected, step) in [
            (
                0,
                5,
                AirConditionerCommand::Speed(Speed::Auto),
                Step::Speed(Speed::Auto),
            ),
            (
                0,
                6,
                AirConditionerCommand::Speed(Speed::Auto),
                Step::Speed(Speed::Auto),
            ),
            (
                2,
                34,
                AirConditionerCommand::Speed(Speed::Medium),
                Step::Speed(Speed::Medium),
            ),
            (
                5,
                0,
                AirConditionerCommand::Power(false),
                Step::Power(false),
            ),
        ] {
            let bytes = [0x04, raw];
            let mut ctx = Context::new(&im, 0x202, attribute);
            ctx.data = TLVElement::new(&bytes);
            let mut write = pin!(handler.write(&ctx));
            assert!(poll_once(write.as_mut()).await.is_none());
            let DeviceRequest::AirConditioner(request) = receiver.next_operation().await.unwrap()
            else {
                panic!("expected air-conditioner request")
            };
            assert_eq!(request.command, expected);
            for required in request.plan().unwrap() {
                request.mark_sent(required).unwrap();
                request.step_accepted(required).unwrap();
            }
            assert!(request.plan().unwrap().contains(&step));
            assert!(poll_once(write.as_mut()).await.is_none());
            request.accept().unwrap();
            write.await.unwrap();
        }
        assert_eq!(
            function.snapshot().air_conditioner().unwrap().power.value,
            Some(true)
        );
    });
}

#[test]
fn thermal_and_fan_reports_follow_observation_not_receipt() {
    let (device, mut receiver) = device(true);
    fan_model!(&device, handler, im);
    let function = &device.functions()[0];
    for value in [
        Value::Power(true),
        Value::Mode(Mode::Cool),
        Value::Temperature(24),
        Value::Speed(Speed::Low),
    ] {
        function.observe_air_conditioner(value, Some(1), 1);
    }
    let ctx = Context::new(&im, 0x201, 17);
    block_on(async {
        let initial = report_dataver(&read_bytes(&handler, &ctx).await.unwrap());
        let mut run = pin!(handler.run(&ctx));
        assert!(poll_once(run.as_mut()).await.is_none());
        ctx.changes.borrow_mut().clear();
        let action = function
            .begin_air_conditioner(AirConditionerCommand::Temperature(25))
            .unwrap();
        let DeviceRequest::AirConditioner(request) = receiver.next_operation().await.unwrap()
        else {
            panic!("expected air-conditioner request")
        };
        request.mark_sent(Step::Temperature(25)).unwrap();
        request.step_accepted(Step::Temperature(25)).unwrap();
        request.accept().unwrap();
        action.accepted().await.unwrap();
        assert!(poll_once(run.as_mut()).await.is_none());
        assert_eq!(*ctx.changes.borrow(), Vec::new());
        assert_eq!(
            report_dataver(&read_bytes(&handler, &ctx).await.unwrap()),
            initial
        );
        function.observe_air_conditioner(Value::Temperature(25), Some(2), 2);
        assert!(poll_once(run.as_mut()).await.is_none());
        assert_eq!(
            ctx.changes.replace(Vec::new()),
            [(2, 0x201, 17), (2, 0x201, 18)]
        );
        assert_ne!(
            report_dataver(&read_bytes(&handler, &ctx).await.unwrap()),
            initial
        );
    });
}

#[test]
fn thermal_writes_share_one_target_and_raise_lower_clamps_once() {
    let (device, mut receiver) = device(true);
    fan_model!(&device, handler, im);
    let function = &device.functions()[0];
    function.observe_air_conditioner(Value::Power(true), Some(1), 1);
    function.observe_air_conditioner(Value::Mode(Mode::Heat), Some(1), 1);
    block_on(async {
        let mut invoke_ctx = Context::new(&im, 0x201, 0);
        invoke_ctx.command.cmd_id = 0;
        invoke_ctx.data = TLVElement::new(&[0x15, 0x24, 0, 2, 0x20, 1, 100, 0x18]);
        assert_eq!(
            handler
                .invoke(
                    &invoke_ctx,
                    InvokeReplyInstance::new(&invoke_ctx.command, WriteBuf::new(&mut [0; 128]))
                )
                .await
                .unwrap_err()
                .code(),
            ErrorCode::InvalidInState
        );
        function.observe_air_conditioner(Value::Temperature(24), Some(1), 1);
        for attribute in [17, 18] {
            let mut ctx = Context::new(&im, 0x201, attribute);
            ctx.data = TLVElement::new(&[0x01, 0xc4, 0x09]);
            let mut write = pin!(handler.write(&ctx));
            assert!(poll_once(write.as_mut()).await.is_none());
            let DeviceRequest::AirConditioner(request) = receiver.next_operation().await.unwrap()
            else {
                panic!("expected air-conditioner request")
            };
            assert_eq!(request.plan().unwrap(), [Step::Temperature(25)]);
            request.mark_sent(Step::Temperature(25)).unwrap();
            request.step_accepted(Step::Temperature(25)).unwrap();
            request.accept().unwrap();
            write.await.unwrap();
        }
        let mut output = [0; 128];
        let mut invoke = pin!(handler.invoke(
            &invoke_ctx,
            InvokeReplyInstance::new(&invoke_ctx.command, WriteBuf::new(&mut output))
        ));
        assert!(poll_once(invoke.as_mut()).await.is_none());
        let DeviceRequest::AirConditioner(request) = receiver.next_operation().await.unwrap()
        else {
            panic!("expected air-conditioner request")
        };
        assert_eq!(request.plan().unwrap(), [Step::Temperature(30)]);
        request.mark_sent(Step::Temperature(30)).unwrap();
        request.step_accepted(Step::Temperature(30)).unwrap();
        request.accept().unwrap();
        invoke.await.unwrap();
        assert_eq!(
            function
                .snapshot()
                .air_conditioner()
                .unwrap()
                .temperature
                .value,
            Some(24)
        );
        for (id, bytes, expected) in [(25, &[0x00, 5][..], 0), (27, &[0x04, 0][..], 4)] {
            let mut ctx = Context::new(&im, 0x201, id);
            ctx.data = TLVElement::new(bytes);
            handler.write(&ctx).await.unwrap();
            let bytes = read_bytes(&handler, &ctx).await.unwrap();
            assert_eq!(
                report_value(&bytes)
                    .i8()
                    .or_else(|_| report_value(&bytes).u8().map(|value| value as i8))
                    .unwrap(),
                expected
            );
        }
    });
}
