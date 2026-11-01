use super::support::{
    Context, attr, fan_model, model_handler, read_bytes, report_dataver, report_value, single_entry,
};
use crate::device::{
    AirConditionerCommand, AirConditionerFanSpeed as Speed, AirConditionerMode as Mode,
    AirConditionerStep as Step, AirConditionerValue as Value, Availability, CommandReceiver,
    DeviceKey, DeviceRequest, FunctionDescription, FunctionKey, FunctionKind, MeasurementRange,
    PhysicalDevice, PhysicalDeviceDescription, TemperatureGrid,
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

fn sensor() -> PhysicalDevice {
    PhysicalDevice::new(
        PhysicalDeviceDescription::new(
            DeviceKey::new("xiaomi", "sensor"),
            "Sensor",
            vec![FunctionDescription {
                id: "temperature:3:7".into(),
                label: "Temperature".into(),
                kind: FunctionKind::Temperature(MeasurementRange::new(-300.0, 400.0).unwrap()),
            }],
        )
        .unwrap(),
    )
    .0
}

#[test]
fn publishes_thermostat_and_only_declared_features() {
    for swing in [false, true] {
        let device = device(swing).0;
        let model = model_handler(vec![single_entry(&device)]);
        model.access(|node| {
            let endpoint = node.endpoint(2).unwrap();
            assert_eq!(
                endpoint
                    .device_types
                    .iter()
                    .map(|kind| (kind.dtype, kind.drev))
                    .collect::<Vec<_>>(),
                [(0x301, 6), (0x2b, 4)]
            );
            assert_eq!(endpoint.cluster(6).unwrap().feature_map, 2);
            let thermostat = endpoint.cluster(0x201).unwrap();
            assert_eq!((thermostat.revision, thermostat.feature_map), (11, 35));
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
fn ambient_temperature_reads_versions_and_reports_only_protocol_value_changes() {
    let (device, mut receiver) = device(true);
    let sensor_device = sensor();
    let sensor = &sensor_device.functions()[0];
    let ac = &device.functions()[0];
    fan_model!(&device, handler, im);
    let ctx = Context::new(&im, 0x201, 0);
    block_on(async {
        let read = || read_bytes(&handler, &ctx);
        let first = read().await.unwrap();
        assert!(report_value(&first).null().is_ok());
        let initial_version = report_dataver(&first);
        let mut run = pin!(handler.run(&ctx));
        assert!(poll_once(run.as_mut()).await.is_none());
        ctx.changes.borrow_mut().clear();

        ac.set_ambient_temperature_source(Some((
            sensor.function_key().clone(),
            Some(sensor.clone()),
        )));
        assert!(poll_once(run.as_mut()).await.is_none());
        assert!(ctx.changes.borrow().is_empty());
        sensor.set_temperature_source_available(true);
        assert!(poll_once(run.as_mut()).await.is_none());
        assert!(ctx.changes.borrow().is_empty());

        // The target setpoint stays independent of the sensor value.
        ac.observe_air_conditioner(Value::Temperature(24), Some(1), 1);
        assert!(poll_once(run.as_mut()).await.is_none());
        ctx.changes.borrow_mut().clear();
        let mut version = report_dataver(&read().await.unwrap());
        assert_ne!(version, initial_version);
        for (value, expected) in [
            (0.0, Some(0)),
            (21.234, Some(2123)),
            (400.0, None),
            (22.0, Some(2200)),
        ] {
            sensor.observe_measurement(value, Some(2), 2).unwrap();
            assert!(poll_once(run.as_mut()).await.is_none());
            assert_eq!(ctx.changes.replace(Vec::new()), [(2, 0x201, 0)]);
            let bytes = read().await.unwrap();
            match expected {
                Some(expected) => assert_eq!(report_value(&bytes).i16().unwrap(), expected),
                None => assert!(report_value(&bytes).null().is_ok()),
            }
            let next = report_dataver(&bytes);
            assert_ne!(next, version);
            version = next;
            assert_eq!(
                crate::matter::measurement_mapping_issue(ac),
                (value == 400.0).then_some("native ambient temperature cannot be represented")
            );
            assert_eq!(sensor.snapshot().measurement().unwrap().value, Some(value));
        }
        sensor.observe_measurement(22.004, Some(3), 3).unwrap();
        assert!(poll_once(run.as_mut()).await.is_none());
        assert!(ctx.changes.borrow().is_empty());
        assert_eq!(report_dataver(&read().await.unwrap()), version);
        sensor.observe_measurement(22.004, Some(4), 4).unwrap();
        assert!(poll_once(run.as_mut()).await.is_none());
        assert!(ctx.changes.borrow().is_empty());
        assert_eq!(report_dataver(&read().await.unwrap()), version);

        sensor.set_temperature_source_available(false);
        assert!(poll_once(run.as_mut()).await.is_none());
        assert_eq!(ctx.changes.replace(Vec::new()), [(2, 0x201, 0)]);
        let bytes = read().await.unwrap();
        assert!(report_value(&bytes).null().is_ok());
        assert_ne!(report_dataver(&bytes), version);
        version = report_dataver(&bytes);
        ac.set_ambient_temperature_source(Some((
            FunctionKey::new(DeviceKey::new("xiaomi", "missing"), "temperature:3:7"),
            None,
        )));
        assert!(poll_once(run.as_mut()).await.is_none());
        assert!(ctx.changes.borrow().is_empty());
        ac.set_ambient_temperature_source(None);
        assert!(poll_once(run.as_mut()).await.is_none());
        assert!(ctx.changes.borrow().is_empty());
        assert_eq!(report_dataver(&read().await.unwrap()), version);
        assert_eq!(
            ac.snapshot().air_conditioner().unwrap().temperature.value,
            Some(24)
        );
        assert!(poll_once(receiver.next_operation()).await.is_none());
    });
}

#[test]
fn automatic_fan_placeholder_preserves_mode_and_nullable_settings() {
    let (device, mut receiver) = device(true);
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
        function.observe_air_conditioner(Value::Power(true), Some(1), 1);
        for id in [3, 6] {
            ctx.attribute = attr(0x202, id);
            assert_eq!(
                read_bytes(&handler, &ctx).await.unwrap_err().code(),
                ErrorCode::Failure
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
        for (id, expected) in [(3, 33), (6, 1)] {
            ctx.attribute = attr(0x202, id);
            assert_eq!(
                report_value(&read_bytes(&handler, &ctx).await.unwrap())
                    .u8()
                    .unwrap(),
                expected
            );
        }
        assert_eq!(
            function.snapshot().air_conditioner().unwrap().speed.value,
            Some(Speed::Auto)
        );
        function.observe_air_conditioner(Value::Power(false), Some(2), 2);
        for id in [0, 2, 3, 5, 6] {
            ctx.attribute = attr(0x202, id);
            assert_eq!(
                report_value(&read_bytes(&handler, &ctx).await.unwrap())
                    .u8()
                    .unwrap(),
                0
            );
        }
        assert!(poll_once(receiver.next_operation()).await.is_none());
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
