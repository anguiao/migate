use super::support::{Context, attr, read_bytes, report_dataver, report_value, single_entry};
use super::*;
use crate::device::{
    Availability, CommandReceiver, DeviceKey, DeviceRequest, FunctionDescription, FunctionKind,
    HeaterCommand, HeaterMode, HeaterStep, HeaterValue, PhysicalDevice, PhysicalDeviceDescription,
    TemperatureGrid, TemperatureSourceState,
};
use futures_lite::future::{block_on, poll_once};
use rs_matter::{
    dm::{AsyncHandler, InvokeReplyInstance},
    error::ErrorCode,
    tlv::TLVElement,
    utils::storage::WriteBuf,
};
use std::pin::pin;

const HEATER: &str = "heater:3";

fn heater() -> (PhysicalDevice, CommandReceiver) {
    let (device, mut receivers) = PhysicalDevice::new(
        PhysicalDeviceDescription::new(
            DeviceKey::new("xiaomi", "bath-heater"),
            "Bath heater",
            vec![FunctionDescription {
                id: HEATER.into(),
                label: "暖风".into(),
                kind: FunctionKind::Heater {
                    temperature: TemperatureGrid::new(25, 45, 1).unwrap(),
                },
            }],
        )
        .unwrap(),
    );
    device.set_availability(Availability::Available);
    (device, receivers.remove(0))
}

async fn next_heater(receiver: &mut CommandReceiver) -> crate::device::HeaterRequest {
    let DeviceRequest::Heater(request) = receiver.next_operation().await.unwrap() else {
        panic!("expected heater request")
    };
    request
}

fn observe(function: &crate::device::Function, power: bool, temperature: i16) {
    function.observe_heater(HeaterValue::Power(power), Some(1), 1);
    function.observe_heater(HeaterValue::Temperature(temperature), Some(1), 1);
}

#[test]
fn publishes_a_heating_only_thermostat_without_cooling_fan_or_running_state() {
    let (device, _receiver) = heater();
    fan_model!(&device, handler, im);
    model_handler(vec![single_entry(&device)]).access(|node| {
        let endpoint = node.endpoint(2).unwrap();
        assert_eq!(
            endpoint
                .device_types
                .iter()
                .map(|kind| (kind.dtype, kind.drev))
                .collect::<Vec<_>>(),
            [(0x301, 6)]
        );
        let thermostat = endpoint.cluster(0x201).unwrap();
        assert_eq!((thermostat.revision, thermostat.feature_map), (11, 1));
        for id in [0, 3, 4, 18, 27, 28, 0xfff8, 0xfff9, 0xfffb, 0xfffc, 0xfffd] {
            assert!(thermostat.attribute(id).is_some(), "{id:#x}");
        }
        for id in [5, 6, 17, 19, 20, 21, 22, 23, 24, 25, 30, 41] {
            assert!(thermostat.attribute(id).is_none(), "{id:#x}");
        }
        assert!(thermostat.command(0).is_some());
        assert!(thermostat.command(1).is_none());
        assert!(endpoint.cluster(6).is_none());
        assert!(endpoint.cluster(0x202).is_none());
    });
    let _ = &im;
}

#[test]
fn reads_only_real_heating_values_and_writes_each_target_separately() {
    let (device, mut receiver) = heater();
    fan_model!(&device, handler, im);
    let function = &device.functions()[0];
    block_on(async {
        // Unknown state keeps the projected values unavailable.
        let mut initial = Context::new(&im, 0x201, 0);
        assert!(
            report_value(&read_bytes(&handler, &initial).await.unwrap())
                .null()
                .is_ok()
        );
        for (id, error) in [(18, ErrorCode::Failure), (28, ErrorCode::Failure)] {
            initial.attribute = attr(0x201, id);
            assert_eq!(
                read_bytes(&handler, &initial).await.unwrap_err().code(),
                error,
                "{id}"
            );
        }
        initial.attribute = attr(0x201, 3);
        assert_eq!(
            report_value(&read_bytes(&handler, &initial).await.unwrap())
                .i16()
                .unwrap(),
            2500
        );
        initial.attribute = attr(0x201, 4);
        assert_eq!(
            report_value(&read_bytes(&handler, &initial).await.unwrap())
                .i16()
                .unwrap(),
            4500
        );
        for id in [17, 5, 6, 25] {
            initial.attribute = attr(0x201, id);
            assert_eq!(
                read_bytes(&handler, &initial).await.unwrap_err().code(),
                ErrorCode::AttributeNotFound,
                "{id}"
            );
        }
        initial.attribute = attr(0x201, 25);
        initial.data = TLVElement::new(&[0x00, 5]);
        assert_eq!(
            handler.write(&initial).await.unwrap_err().code(),
            ErrorCode::AttributeNotFound
        );
        assert!(poll_once(receiver.next_operation()).await.is_none());
        observe(function, false, 25);
        initial.attribute = attr(0x201, 28);
        assert_eq!(
            report_value(&read_bytes(&handler, &initial).await.unwrap())
                .u8()
                .unwrap(),
            0
        );
        initial.attribute = attr(0x201, 18);
        assert_eq!(
            report_value(&read_bytes(&handler, &initial).await.unwrap())
                .i16()
                .unwrap(),
            2500
        );

        for (attribute, raw, expected) in [
            (28, 4, HeaterCommand::Mode(HeaterMode::Heat)),
            (28, 0, HeaterCommand::Power(false)),
        ] {
            let mut context = Context::new(&im, 0x201, attribute);
            let bytes = [0x04, raw];
            context.data = TLVElement::new(&bytes);
            let mut write = pin!(handler.write(&context));
            assert!(poll_once(write.as_mut()).await.is_none());
            let request = next_heater(&mut receiver).await;
            assert_eq!(request.command, expected);
            request.mark_sent().unwrap();
            request.step_accepted().unwrap();
            request.accept().unwrap();
            write.await.unwrap();
        }

        let mut context = Context::new(&im, 0x201, 17);
        context.data = TLVElement::new(&[0x01, 0xc4, 0x09]);
        assert_eq!(
            handler.write(&context).await.unwrap_err().code(),
            ErrorCode::AttributeNotFound
        );
        let mut context = Context::new(&im, 0x201, 18);
        context.data = TLVElement::new(&[0x01, 0xc4, 0x09]);
        let mut write = pin!(handler.write(&context));
        assert!(poll_once(write.as_mut()).await.is_none());
        let request = next_heater(&mut receiver).await;
        assert_eq!(request.step(), HeaterStep::Temperature(25));
        request.mark_sent().unwrap();
        request.step_accepted().unwrap();
        request.accept().unwrap();
        write.await.unwrap();
        assert_eq!(
            function.snapshot().heater().unwrap().temperature.value,
            Some(25)
        );
        assert_eq!(
            function.snapshot().heater().unwrap().power.value,
            Some(false)
        );
    });
}

#[test]
fn built_in_temperature_is_nullable_and_reports_only_valid_feedback_changes() {
    let (device, mut receiver) = heater();
    fan_model!(&device, handler, im);
    let function = &device.functions()[0];
    let ctx = Context::new(&im, 0x201, 0);
    block_on(async {
        let first = read_bytes(&handler, &ctx).await.unwrap();
        assert!(report_value(&first).null().is_ok());
        let initial_version = report_dataver(&first);
        let mut run = pin!(handler.run(&ctx));
        assert!(poll_once(run.as_mut()).await.is_none());
        ctx.changes.borrow_mut().clear();

        function.set_heater_temperature_source_available(true);
        assert!(poll_once(run.as_mut()).await.is_none());
        assert!(ctx.changes.borrow().is_empty());
        let bytes = read_bytes(&handler, &ctx).await.unwrap();
        assert!(report_value(&bytes).null().is_ok());
        assert_eq!(report_dataver(&bytes), initial_version);

        function
            .observe_heater_temperature(22.4, Some(10), 11)
            .unwrap();
        assert!(poll_once(run.as_mut()).await.is_none());
        assert_eq!(ctx.changes.replace(Vec::new()), [(2, 0x201, 0)]);
        let bytes = read_bytes(&handler, &ctx).await.unwrap();
        assert_eq!(report_value(&bytes).i16().unwrap(), 2240);
        assert_ne!(report_dataver(&bytes), initial_version);

        function
            .observe_heater_temperature(22.404, Some(12), 13)
            .unwrap();
        assert!(poll_once(run.as_mut()).await.is_none());
        assert!(ctx.changes.borrow().is_empty());

        function.set_heater_temperature_source_available(false);
        assert!(poll_once(run.as_mut()).await.is_none());
        assert_eq!(ctx.changes.replace(Vec::new()), [(2, 0x201, 0)]);
        let bytes = read_bytes(&handler, &ctx).await.unwrap();
        assert!(report_value(&bytes).null().is_ok());
        assert_eq!(
            function.snapshot().heater().unwrap().temperature_source,
            TemperatureSourceState::Unavailable
        );
        assert_eq!(
            function
                .snapshot()
                .heater()
                .unwrap()
                .current_temperature
                .value,
            Some(22.404)
        );

        function.set_heater_temperature_source_available(true);
        assert!(poll_once(run.as_mut()).await.is_none());
        let bytes = read_bytes(&handler, &ctx).await.unwrap();
        assert!(report_value(&bytes).null().is_ok());
        function
            .observe_heater_temperature(23.0, Some(20), 21)
            .unwrap();
        assert!(poll_once(run.as_mut()).await.is_none());
        assert_eq!(ctx.changes.replace(Vec::new()), [(2, 0x201, 0)]);
        assert_eq!(
            report_value(&read_bytes(&handler, &ctx).await.unwrap())
                .i16()
                .unwrap(),
            2300
        );
        assert!(poll_once(receiver.next_operation()).await.is_none());
    });
}
#[test]
fn setpoint_raise_lower_follows_the_heating_only_profile() {
    let (device, mut receiver) = heater();
    fan_model!(&device, handler, im);
    let function = &device.functions()[0];
    block_on(async {
        // No known target cannot be adjusted.
        let mut active = Context::new(&im, 0x201, 0);
        active.command.cmd_id = 0;
        active.data = TLVElement::new(&[0x15, 0x24, 0, 0, 0x20, 1, 100, 0x18]);
        let mut output = [0; 128];
        assert_eq!(
            handler
                .invoke(
                    &active,
                    InvokeReplyInstance::new(&active.command, WriteBuf::new(&mut output))
                )
                .await
                .unwrap_err()
                .code(),
            ErrorCode::InvalidInState
        );
        assert!(poll_once(receiver.next_operation()).await.is_none());

        observe(function, false, 25);

        // Cooling is not part of the heating-only profile.
        let mut cool = Context::new(&im, 0x201, 0);
        cool.command.cmd_id = 0;
        cool.data = TLVElement::new(&[0x15, 0x24, 0, 1, 0x20, 1, 100, 0x18]);
        let mut output = [0; 128];
        let mut invoke = pin!(handler.invoke(
            &cool,
            InvokeReplyInstance::new(&cool.command, WriteBuf::new(&mut output))
        ));
        assert_eq!(
            poll_once(invoke.as_mut())
                .await
                .expect("cooling must be rejected before any native request")
                .unwrap_err()
                .code(),
            ErrorCode::InvalidCommand
        );
        assert!(poll_once(receiver.next_operation()).await.is_none());

        // Heat and Both adjust only the target and never infer power.
        for mode in [0, 2] {
            let mut context = Context::new(&im, 0x201, 0);
            context.command.cmd_id = 0;
            let bytes = [0x15, 0x24, 0, mode, 0x20, 1, 10, 0x18];
            context.data = TLVElement::new(&bytes);
            let mut output = [0; 128];
            let mut invoke = pin!(handler.invoke(
                &context,
                InvokeReplyInstance::new(&context.command, WriteBuf::new(&mut output))
            ));
            assert!(poll_once(invoke.as_mut()).await.is_none());
            let request = next_heater(&mut receiver).await;
            assert_eq!(
                request.command,
                HeaterCommand::Temperature(26),
                "mode {mode}"
            );
            assert_eq!(request.step(), HeaterStep::Temperature(26));
            request.mark_sent().unwrap();
            request.step_accepted().unwrap();
            request.accept().unwrap();
            invoke.await.unwrap();
            assert_eq!(
                function.snapshot().heater().unwrap().power.value,
                Some(false)
            );
            assert_eq!(
                function.snapshot().heater().unwrap().temperature.value,
                Some(25)
            );
        }

        // Non-integer centidegree targets and invalid enum values are rejected.
        for (mode, amount) in [(0, 5), (0, 1)] {
            let mut context = Context::new(&im, 0x201, 0);
            context.command.cmd_id = 0;
            let bytes = [0x15, 0x24, 0, mode, 0x20, 1, amount, 0x18];
            context.data = TLVElement::new(&bytes);
            let mut output = [0; 128];
            let mut invoke = pin!(handler.invoke(
                &context,
                InvokeReplyInstance::new(&context.command, WriteBuf::new(&mut output))
            ));
            let error = poll_once(invoke.as_mut())
                .await
                .expect("invalid setpoint step must not queue a native request")
                .unwrap_err();
            assert_eq!(error.code(), ErrorCode::ConstraintError);
            assert!(poll_once(receiver.next_operation()).await.is_none());
        }

        for (base, amount, expected) in [(40, 100_u8, 45_i16), (25, 0x9c_u8, 25_i16)] {
            function.observe_heater(HeaterValue::Temperature(base), Some(3), 3);
            let bytes = [0x15, 0x24, 0, 0, 0x20, 1, amount, 0x18];
            let mut context = Context::new(&im, 0x201, 0);
            context.command.cmd_id = 0;
            context.data = TLVElement::new(&bytes);
            let mut output = [0; 128];
            let mut invoke = pin!(handler.invoke(
                &context,
                InvokeReplyInstance::new(&context.command, WriteBuf::new(&mut output))
            ));
            assert!(poll_once(invoke.as_mut()).await.is_none());
            let request = next_heater(&mut receiver).await;
            assert_eq!(request.command, HeaterCommand::Temperature(expected));
            assert_eq!(request.step(), HeaterStep::Temperature(expected));
            request.mark_sent().unwrap();
            request.step_accepted().unwrap();
            request.accept().unwrap();
            invoke.await.unwrap();
        }

        let mut invalid = Context::new(&im, 0x201, 0);
        invalid.command.cmd_id = 0;
        invalid.data = TLVElement::new(&[0x15, 0x24, 0, 3, 0x20, 1, 100, 0x18]);
        let mut output = [0; 128];
        assert_eq!(
            handler
                .invoke(
                    &invalid,
                    InvokeReplyInstance::new(&invalid.command, WriteBuf::new(&mut output))
                )
                .await
                .unwrap_err()
                .code(),
            ErrorCode::InvalidCommand
        );
        assert!(poll_once(receiver.next_operation()).await.is_none());
    });
}
