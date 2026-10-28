use super::support::{model_handler, single_entry};
use crate::device::{
    DeviceKey, FunctionDescription, FunctionKind, PhysicalDevice, PhysicalDeviceDescription,
};
use rs_matter::dm::Metadata;

fn curtain() -> (PhysicalDevice, crate::device::CommandReceiver) {
    let (device, mut receivers) = PhysicalDevice::new(
        PhysicalDeviceDescription::new(
            DeviceKey::new("xiaomi", "test-curtain"),
            "Curtain",
            vec![FunctionDescription {
                id: "main".into(),
                label: "Curtain".into(),
                kind: FunctionKind::Curtain,
            }],
        )
        .unwrap(),
    );
    (device, receivers.remove(0))
}

#[test]
fn curtain_publishes_only_supported_window_covering_elements() {
    let (device, _) = curtain();
    let model = model_handler(vec![single_entry(&device)]);
    model.access(|node| {
        let endpoint = node.endpoint(2).unwrap();
        assert!(
            endpoint
                .device_types
                .iter()
                .any(|d| d.dtype == 0x0202 && d.drev == 6)
        );
        assert!(endpoint.device_types.iter().any(|d| d.dtype == 0x0013));
        assert_eq!(
            endpoint.clusters.iter().map(|c| c.id).collect::<Vec<_>>(),
            [0x1d, 4, 3, 0x39, 0x102]
        );
        let cluster = endpoint.cluster(0x102).unwrap();
        assert_eq!(cluster.revision, 8);
        assert_eq!(cluster.feature_map, 5);
        for id in [
            0, 7, 8, 10, 11, 13, 14, 23, 0xfff8, 0xfff9, 0xfffb, 0xfffc, 0xfffd,
        ] {
            assert!(cluster.attribute(id).is_some(), "missing attribute {id}");
        }
        for id in [1, 2, 3, 4, 5, 6, 9, 12, 15, 16, 17, 18, 19, 26] {
            assert!(cluster.attribute(id).is_none(), "unexpected attribute {id}");
        }
        for id in [0, 1, 2, 5] {
            assert!(cluster.command(id).is_some(), "missing command {id}");
        }
        for id in [4, 7, 8] {
            assert!(cluster.command(id).is_none(), "unexpected command {id}");
        }
    });
}

use super::support::{Context, attr, read_bytes, report_value};
use crate::{
    device::{
        Availability, CommandError, CurtainCommand, CurtainMotion, CurtainValue, DeviceRequest,
    },
    matter::{model::basic_info, storage::StoreAdapter},
    storage::Store,
};
use futures_lite::future::{block_on, poll_once};
use rs_matter::{
    MATTER_PORT, Matter,
    crypto::default_crypto,
    dm::{
        AsyncHandler, InvokeReplyInstance,
        devices::test::{DAC_PRIVKEY, TEST_DEV_ATT, TEST_DEV_COMM},
    },
    error::ErrorCode,
    im::{InteractionModel, InteractionModelState},
    tlv::TLVElement,
    transport::exchange::MatterBuffers,
    utils::storage::WriteBuf,
};
use std::pin::pin;

macro_rules! setup {
    ($device:ident, $receiver:ident, $handler:ident, $ctx:ident) => {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let identity = store.load_identity().unwrap();
        let ($device, mut $receiver) = curtain();
        let $handler = model_handler(vec![single_entry(&$device)]);
        let info = basic_info(&identity);
        let matter = Matter::new(&info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
        let buffers: MatterBuffers = MatterBuffers::new();
        let state: InteractionModelState = InteractionModelState::new();
        let crypto = default_crypto(rand::rng(), DAC_PRIVKEY);
        let kv = matter.kv(StoreAdapter::new(store.matter()));
        let im = InteractionModel::new(&matter, &crypto, &buffers, &$handler, &kv, &state);
        let mut $ctx = Context::new(&im, 0x102, 0);
    };
}

#[test]
fn curtain_reads_native_values_and_validates_mode() {
    setup!(device, receiver, handler, ctx);
    let _ = &mut receiver;
    block_on(async {
        for id in [8, 11, 14] {
            ctx.attribute = attr(0x102, id);
            assert!(
                report_value(&read_bytes(&handler, &ctx).await.unwrap())
                    .null()
                    .is_ok()
            );
        }
        ctx.attribute = attr(0x102, 10);
        assert_eq!(
            read_bytes(&handler, &ctx).await.unwrap_err().code(),
            ErrorCode::Failure
        );
        for (id, expected) in [(0, 4), (7, 9), (13, 255), (23, 0)] {
            ctx.attribute = attr(0x102, id);
            assert_eq!(
                report_value(&read_bytes(&handler, &ctx).await.unwrap())
                    .u8()
                    .unwrap(),
                expected
            );
        }
        let function = &device.functions()[0];
        function
            .observe_curtain(CurtainValue::CurrentPosition(37), Some(1), 2)
            .unwrap();
        function
            .observe_curtain(CurtainValue::TargetPosition(80), Some(1), 2)
            .unwrap();
        for (id, expected) in [(8, 63), (11, 2000), (14, 6300)] {
            ctx.attribute = attr(0x102, id);
            assert_eq!(
                report_value(&read_bytes(&handler, &ctx).await.unwrap())
                    .u16()
                    .unwrap(),
                expected
            );
        }
        for (motion, expected) in [
            (CurtainMotion::Opening, 5),
            (CurtainMotion::Closing, 10),
            (CurtainMotion::Stopped, 0),
        ] {
            function
                .observe_curtain(CurtainValue::Status(motion), Some(1), 2)
                .unwrap();
            ctx.attribute = attr(0x102, 10);
            assert_eq!(
                report_value(&read_bytes(&handler, &ctx).await.unwrap())
                    .u8()
                    .unwrap(),
                expected
            );
        }
        ctx.attribute = attr(0x102, 23);
        for value in [0, 1, 2, 4, 8, 16, 128, 255] {
            let bytes = [0x04, value];
            let mut write_ctx = Context::new(ctx.base, 0x102, 23);
            write_ctx.data = TLVElement::new(&bytes);
            let result = handler.write(&write_ctx).await;
            if value == 0 {
                result.unwrap();
            } else {
                assert_eq!(result.unwrap_err().code(), ErrorCode::ConstraintError);
            }
        }
    });
}

#[test]
fn curtain_dispatches_commands_and_waits_for_receipts() {
    setup!(device, receiver, handler, ctx);
    device.set_availability(Availability::Available);
    block_on(async {
        for (id, bytes, target) in [
            (0, &[0x15, 0x18][..], 100.0),
            (1, &[0x15, 0x18][..], 0.0),
            (5, &[0x15, 0x25, 0, 0x89, 0x13, 0x18][..], 49.99),
            (5, &[0x15, 0x25, 0, 0, 0, 0x18][..], 100.0),
            (5, &[0x15, 0x25, 0, 0x10, 0x27, 0x18][..], 0.0),
        ] {
            ctx.command.cmd_id = id;
            ctx.data = TLVElement::new(bytes);
            let mut out = [0; 128];
            let mut invoke = pin!(handler.invoke(
                &ctx,
                InvokeReplyInstance::new(&ctx.command, WriteBuf::new(&mut out))
            ));
            assert!(poll_once(invoke.as_mut()).await.is_none());
            let DeviceRequest::Curtain(request) = receiver.next_operation().await.unwrap() else {
                panic!("expected curtain")
            };
            assert_eq!(request.command, CurtainCommand::Target(target));
            request.mark_sent().unwrap();
            request.accept().unwrap();
            invoke.await.unwrap();
        }
        ctx.command.cmd_id = 2;
        ctx.data = TLVElement::new(&[0x15, 0x18]);
        {
            let mut out = [0; 128];
            let mut invoke = pin!(handler.invoke(
                &ctx,
                InvokeReplyInstance::new(&ctx.command, WriteBuf::new(&mut out))
            ));
            assert!(poll_once(invoke.as_mut()).await.is_none());
            let DeviceRequest::Curtain(request) = receiver.next_operation().await.unwrap() else {
                panic!("expected curtain")
            };
            assert_eq!(request.command, CurtainCommand::Stop);
            request.mark_sent().unwrap();
            request.reject(CommandError::Timeout);
            assert_eq!(invoke.await.unwrap_err().code(), ErrorCode::RxTimeout);
        }
        for bytes in [&[][..], &[0x15], &[0x09], &[0x15, 0x24]] {
            ctx.data = TLVElement::new(bytes);
            assert_eq!(
                handler
                    .invoke(
                        &ctx,
                        InvokeReplyInstance::new(&ctx.command, WriteBuf::new(&mut [0; 128]))
                    )
                    .await
                    .unwrap_err()
                    .code(),
                ErrorCode::InvalidCommand
            );
        }
        ctx.command.cmd_id = 5;
        for bytes in [&[0x15, 0x25, 0, 0x11, 0x27, 0x18][..], &[0x15, 0x18]] {
            ctx.data = TLVElement::new(bytes);
            assert!(
                handler
                    .invoke(
                        &ctx,
                        InvokeReplyInstance::new(&ctx.command, WriteBuf::new(&mut [0; 128]))
                    )
                    .await
                    .is_err()
            );
        }
    });
}

#[test]
fn stop_projection_follows_feedback_and_yields_to_new_commands_and_reports() {
    setup!(device, receiver, handler, ctx);
    device.set_availability(Availability::Available);
    let function = &device.functions()[0];
    block_on(async {
        let observe = |value| function.observe_curtain(value, Some(7), 7).unwrap();
        observe(CurtainValue::CurrentPosition(20));
        observe(CurtainValue::TargetPosition(80));
        observe(CurtainValue::Status(CurtainMotion::Opening));
        ctx.attribute = attr(0x102, 11);
        let read_target = async || {
            report_value(&read_bytes(&handler, &ctx).await.unwrap())
                .u16()
                .unwrap()
        };
        assert_eq!(read_target().await, 2000);

        // A stopped report before the request is sent cannot complete it.
        let first = function.begin_curtain(CurtainCommand::Stop).unwrap();
        let DeviceRequest::Curtain(request) = receiver.next_operation().await.unwrap() else {
            panic!()
        };
        observe(CurtainValue::Status(CurtainMotion::Stopped));
        assert_eq!(read_target().await, 2000);
        request.mark_sent().unwrap();
        assert_eq!(read_target().await, 2000);
        // Same value and timestamps still constitute a new physical report.
        observe(CurtainValue::Status(CurtainMotion::Stopped));
        assert_eq!(read_target().await, 8000);
        observe(CurtainValue::CurrentPosition(25));
        assert_eq!(read_target().await, 7500);
        let native = function.snapshot().curtain().unwrap();
        assert_eq!(native.current_position.value, Some(25));
        assert_eq!(native.target_position.value, Some(80));
        assert_eq!(native.status.value, Some(CurtainMotion::Stopped));

        // A new target takes over immediately; the old receipt and stop report
        // cannot restore the previous stop's projection.
        let next = function
            .begin_curtain(CurtainCommand::Target(50.0))
            .unwrap();
        assert_eq!(read_target().await, 2000);
        request.accept().unwrap();
        first.accepted().await.unwrap();
        observe(CurtainValue::Status(CurtainMotion::Stopped));
        observe(CurtainValue::CurrentPosition(30));
        assert_eq!(read_target().await, 2000);
        let DeviceRequest::Curtain(next_request) = receiver.next_operation().await.unwrap() else {
            panic!()
        };
        next_request.mark_sent().unwrap();
        next_request.accept().unwrap();
        next.accepted().await.unwrap();

        // A report for the unchanged target clears stop handling even when all
        // report timestamps and values repeat within one processing turn.
        observe(CurtainValue::Status(CurtainMotion::Opening));
        let stop = function.begin_curtain(CurtainCommand::Stop).unwrap();
        let DeviceRequest::Curtain(request) = receiver.next_operation().await.unwrap() else {
            panic!()
        };
        request.mark_sent().unwrap();
        request.accept().unwrap();
        stop.accepted().await.unwrap();
        assert_eq!(read_target().await, 2000);
        observe(CurtainValue::Status(CurtainMotion::Stopped));
        observe(CurtainValue::CurrentPosition(40));
        assert_eq!(read_target().await, 6000);
        observe(CurtainValue::TargetPosition(80));
        observe(CurtainValue::Status(CurtainMotion::Stopped));
        assert_eq!(read_target().await, 2000);

        // Stop and resume in one processing turn must not lose the resume.
        let stop = function.begin_curtain(CurtainCommand::Stop).unwrap();
        let DeviceRequest::Curtain(request) = receiver.next_operation().await.unwrap() else {
            panic!()
        };
        request.mark_sent().unwrap();
        request.accept().unwrap();
        stop.accepted().await.unwrap();
        observe(CurtainValue::Status(CurtainMotion::Stopped));
        observe(CurtainValue::Status(CurtainMotion::Closing));
        observe(CurtainValue::Status(CurtainMotion::Stopped));
        observe(CurtainValue::CurrentPosition(45));
        assert_eq!(read_target().await, 2000);
    });
}

#[test]
fn stop_failure_and_unsent_barriers_do_not_overwrite_later_target() {
    setup!(device, receiver, handler, ctx);
    device.set_availability(Availability::Available);
    let function = &device.functions()[0];
    block_on(async {
        let observe = |value| function.observe_curtain(value, None, 1).unwrap();
        observe(CurtainValue::CurrentPosition(30));
        observe(CurtainValue::TargetPosition(80));
        observe(CurtainValue::Status(CurtainMotion::Opening));
        ctx.attribute = attr(0x102, 11);
        let read_target = async || {
            report_value(&read_bytes(&handler, &ctx).await.unwrap())
                .u16()
                .unwrap()
        };
        // A sent stop can report failure, after which an unrelated stop does not
        // replace the native target. A timeout is treated the same way.
        for error in [
            CommandError::Rejected("denied".into()),
            CommandError::Timeout,
        ] {
            let stop = function.begin_curtain(CurtainCommand::Stop).unwrap();
            let DeviceRequest::Curtain(request) = receiver.next_operation().await.unwrap() else {
                panic!()
            };
            request.mark_sent().unwrap();
            request.reject(error.clone());
            assert_eq!(stop.accepted().await.unwrap_err(), error);
            observe(CurtainValue::Status(CurtainMotion::Stopped));
            assert_eq!(read_target().await, 2000);
        }
        // A stop behind an in-flight target must still be transmitted, but it
        // cannot reactivate stale stop handling after a later target is queued.
        let old = function
            .begin_curtain(CurtainCommand::Target(60.0))
            .unwrap();
        let DeviceRequest::Curtain(inflight) = receiver.next_operation().await.unwrap() else {
            panic!()
        };
        inflight.mark_sent().unwrap();
        let stop = function.begin_curtain(CurtainCommand::Stop).unwrap();
        let newer = function
            .begin_curtain(CurtainCommand::Target(50.0))
            .unwrap();
        inflight.accept().unwrap();
        old.accepted().await.unwrap();
        let DeviceRequest::Curtain(request) = receiver.next_operation().await.unwrap() else {
            panic!()
        };
        assert_eq!(request.command, CurtainCommand::Stop);
        request.mark_sent().unwrap();
        observe(CurtainValue::Status(CurtainMotion::Stopped));
        observe(CurtainValue::CurrentPosition(31));
        assert_eq!(read_target().await, 2000);
        request.accept().unwrap();
        stop.accepted().await.unwrap();
        let DeviceRequest::Curtain(request) = receiver.next_operation().await.unwrap() else {
            panic!()
        };
        request.mark_sent().unwrap();
        request.accept().unwrap();
        newer.accepted().await.unwrap();
    });
}

#[test]
fn curtain_reports_only_changed_projection_and_preserves_values_on_disconnect() {
    setup!(device, receiver, handler, ctx);
    let _ = &mut receiver;
    device.set_availability(Availability::Available);
    let function = &device.functions()[0];
    block_on(async {
        function
            .observe_curtain(CurtainValue::CurrentPosition(20), Some(1), 1)
            .unwrap();
        function
            .observe_curtain(CurtainValue::TargetPosition(80), Some(1), 1)
            .unwrap();
        function
            .observe_curtain(CurtainValue::Status(CurtainMotion::Opening), Some(1), 1)
            .unwrap();
        ctx.attribute = attr(0x102, 11);
        let mut run = pin!(handler.run(&ctx));
        assert!(poll_once(run.as_mut()).await.is_none());
        ctx.changes.borrow_mut().clear();
        let version = super::support::report_dataver(&read_bytes(&handler, &ctx).await.unwrap());
        function
            .observe_curtain(CurtainValue::CurrentPosition(21), Some(2), 2)
            .unwrap();
        assert!(poll_once(run.as_mut()).await.is_none());
        assert_eq!(&*ctx.changes.borrow(), &[(2, 0x102, 8), (2, 0x102, 14)]);
        let next = super::support::report_dataver(&read_bytes(&handler, &ctx).await.unwrap());
        assert_ne!(next, version);
        ctx.changes.borrow_mut().clear();
        function
            .observe_curtain(CurtainValue::CurrentPosition(21), Some(3), 3)
            .unwrap();
        assert!(poll_once(run.as_mut()).await.is_none());
        assert!(ctx.changes.borrow().is_empty());
        assert_eq!(
            super::support::report_dataver(&read_bytes(&handler, &ctx).await.unwrap()),
            next
        );
        function
            .observe_curtain(CurtainValue::TargetPosition(70), Some(4), 4)
            .unwrap();
        function
            .observe_curtain(CurtainValue::Status(CurtainMotion::Closing), Some(4), 4)
            .unwrap();
        assert!(poll_once(run.as_mut()).await.is_none());
        assert_eq!(&*ctx.changes.borrow(), &[(2, 0x102, 11), (2, 0x102, 10)]);
        ctx.changes.borrow_mut().clear();
        device.set_availability(Availability::Unavailable("offline".into()));
        assert!(poll_once(run.as_mut()).await.is_none());
        assert!(
            ctx.changes
                .borrow()
                .iter()
                .all(|(_, cluster, _)| *cluster != 0x102)
        );
        assert_eq!(
            report_value(&read_bytes(&handler, &ctx).await.unwrap())
                .u16()
                .unwrap(),
            3000
        );
    });
}

#[test]
fn later_rejected_target_still_retires_previous_stop_projection() {
    setup!(device, receiver, handler, ctx);
    device.set_availability(Availability::Available);
    let function = &device.functions()[0];
    block_on(async {
        function
            .observe_curtain(CurtainValue::CurrentPosition(25), None, 1)
            .unwrap();
        function
            .observe_curtain(CurtainValue::TargetPosition(80), None, 1)
            .unwrap();
        let stop = function.begin_curtain(CurtainCommand::Stop).unwrap();
        let DeviceRequest::Curtain(request) = receiver.next_operation().await.unwrap() else {
            panic!()
        };
        request.mark_sent().unwrap();
        request.accept().unwrap();
        stop.accepted().await.unwrap();
        function
            .observe_curtain(CurtainValue::Status(CurtainMotion::Stopped), None, 1)
            .unwrap();
        ctx.attribute = attr(0x102, 11);
        assert_eq!(
            report_value(&read_bytes(&handler, &ctx).await.unwrap())
                .u16()
                .unwrap(),
            7500
        );
        device.set_availability(Availability::Unavailable("offline".into()));
        assert!(
            function
                .begin_curtain(CurtainCommand::Target(10.0))
                .is_err()
        );
        assert_eq!(
            report_value(&read_bytes(&handler, &ctx).await.unwrap())
                .u16()
                .unwrap(),
            2000
        );
        function
            .observe_curtain(CurtainValue::Status(CurtainMotion::Stopped), None, 1)
            .unwrap();
        assert_eq!(
            report_value(&read_bytes(&handler, &ctx).await.unwrap())
                .u16()
                .unwrap(),
            2000
        );
    });
}

#[test]
fn stop_feedback_accepts_either_report_order_and_unknown_current() {
    setup!(device, receiver, handler, ctx);
    device.set_availability(Availability::Available);
    let function = &device.functions()[0];
    block_on(async {
        function
            .observe_curtain(CurtainValue::TargetPosition(80), None, 1)
            .unwrap();
        ctx.attribute = attr(0x102, 11);
        let stop = function.begin_curtain(CurtainCommand::Stop).unwrap();
        let DeviceRequest::Curtain(request) = receiver.next_operation().await.unwrap() else {
            panic!()
        };
        request.mark_sent().unwrap();
        request.accept().unwrap();
        stop.accepted().await.unwrap();
        // Receipt alone is not a physical stop.
        assert_eq!(
            report_value(&read_bytes(&handler, &ctx).await.unwrap())
                .u16()
                .unwrap(),
            2000
        );
        function
            .observe_curtain(CurtainValue::Status(CurtainMotion::Stopped), None, 1)
            .unwrap();
        assert!(
            report_value(&read_bytes(&handler, &ctx).await.unwrap())
                .null()
                .is_ok()
        );
        function
            .observe_curtain(CurtainValue::CurrentPosition(44), None, 1)
            .unwrap();
        assert_eq!(
            report_value(&read_bytes(&handler, &ctx).await.unwrap())
                .u16()
                .unwrap(),
            5600
        );

        function
            .observe_curtain(CurtainValue::Status(CurtainMotion::Opening), None, 1)
            .unwrap();
        let stop = function.begin_curtain(CurtainCommand::Stop).unwrap();
        let DeviceRequest::Curtain(request) = receiver.next_operation().await.unwrap() else {
            panic!()
        };
        request.mark_sent().unwrap();
        request.accept().unwrap();
        stop.accepted().await.unwrap();
        function
            .observe_curtain(CurtainValue::CurrentPosition(50), None, 1)
            .unwrap();
        assert_eq!(
            report_value(&read_bytes(&handler, &ctx).await.unwrap())
                .u16()
                .unwrap(),
            2000
        );
        function
            .observe_curtain(CurtainValue::Status(CurtainMotion::Stopped), None, 1)
            .unwrap();
        assert_eq!(
            report_value(&read_bytes(&handler, &ctx).await.unwrap())
                .u16()
                .unwrap(),
            5000
        );
    });
}

#[test]
fn curtain_protocol_lists_match_published_metadata() {
    setup!(device, receiver, handler, ctx);
    let _ = (&device, &mut receiver);
    block_on(async {
        for (attribute, expected) in [
            (0xfff8, vec![]),
            (0xfff9, vec![0, 1, 2, 5]),
            (
                0xfffb,
                vec![
                    0, 7, 8, 10, 11, 13, 14, 23, 0xfff8, 0xfff9, 0xfffb, 0xfffc, 0xfffd,
                ],
            ),
        ] {
            ctx.attribute = attr(0x102, attribute);
            let bytes = read_bytes(&handler, &ctx).await.unwrap();
            let values = report_value(&bytes)
                .array()
                .unwrap()
                .iter()
                .map(|item| item.unwrap().u32().unwrap())
                .collect::<Vec<_>>();
            assert_eq!(values, expected);
        }
        ctx.attribute = attr(0x102, 0xfffc);
        assert_eq!(
            report_value(&read_bytes(&handler, &ctx).await.unwrap())
                .u32()
                .unwrap(),
            5
        );
        ctx.attribute = attr(0x102, 0xfffd);
        assert_eq!(
            report_value(&read_bytes(&handler, &ctx).await.unwrap())
                .u16()
                .unwrap(),
            8
        );
        for command in [4, 7, 8] {
            ctx.command.cmd_id = command;
            assert_eq!(
                handler
                    .invoke(
                        &ctx,
                        InvokeReplyInstance::new(&ctx.command, WriteBuf::new(&mut [0; 128]))
                    )
                    .await
                    .unwrap_err()
                    .code(),
                ErrorCode::CommandNotFound
            );
        }
    });
}

#[test]
fn repeating_stop_keeps_confirmed_position_until_a_new_target_or_motion() {
    setup!(device, receiver, handler, ctx);
    device.set_availability(Availability::Available);
    let function = &device.functions()[0];
    block_on(async {
        function
            .observe_curtain(CurtainValue::CurrentPosition(25), None, 1)
            .unwrap();
        function
            .observe_curtain(CurtainValue::TargetPosition(80), None, 1)
            .unwrap();
        let first = function.begin_curtain(CurtainCommand::Stop).unwrap();
        let DeviceRequest::Curtain(request) = receiver.next_operation().await.unwrap() else {
            panic!()
        };
        request.mark_sent().unwrap();
        function
            .observe_curtain(CurtainValue::Status(CurtainMotion::Stopped), None, 1)
            .unwrap();
        ctx.attribute = attr(0x102, 11);
        let read_target = async || {
            report_value(&read_bytes(&handler, &ctx).await.unwrap())
                .u16()
                .unwrap()
        };
        assert_eq!(read_target().await, 7500);
        let second = function.begin_curtain(CurtainCommand::Stop).unwrap();
        assert_eq!(read_target().await, 7500);
        request.accept().unwrap();
        first.accepted().await.unwrap();
        let DeviceRequest::Curtain(request) = receiver.next_operation().await.unwrap() else {
            panic!()
        };
        request.mark_sent().unwrap();
        request.reject(CommandError::Timeout);
        assert_eq!(second.accepted().await.unwrap_err(), CommandError::Timeout);
        assert_eq!(read_target().await, 7500);
        function
            .observe_curtain(CurtainValue::Status(CurtainMotion::Opening), None, 1)
            .unwrap();
        assert_eq!(read_target().await, 2000);
    });
}

#[test]
fn queued_later_stops_do_not_mask_feedback_from_the_first_sent_stop() {
    for both_queued_before_send in [false, true] {
        setup!(device, receiver, handler, ctx);
        device.set_availability(Availability::Available);
        let function = &device.functions()[0];
        block_on(async {
            function
                .observe_curtain(CurtainValue::CurrentPosition(25), Some(1), 1)
                .unwrap();
            function
                .observe_curtain(CurtainValue::TargetPosition(80), Some(1), 1)
                .unwrap();
            function
                .observe_curtain(CurtainValue::Status(CurtainMotion::Opening), Some(1), 1)
                .unwrap();
            ctx.attribute = attr(0x102, 11);
            let mut run = pin!(handler.run(&ctx));
            assert!(poll_once(run.as_mut()).await.is_none());
            ctx.changes.borrow_mut().clear();
            let first = function.begin_curtain(CurtainCommand::Stop).unwrap();
            let second_before = if both_queued_before_send {
                Some(function.begin_curtain(CurtainCommand::Stop).unwrap())
            } else {
                None
            };
            let DeviceRequest::Curtain(first_request) = receiver.next_operation().await.unwrap()
            else {
                panic!()
            };
            if both_queued_before_send {
                // An old cached stopped status cannot arm an unsent request.
                function
                    .observe_curtain(CurtainValue::Status(CurtainMotion::Stopped), Some(1), 1)
                    .unwrap();
                assert!(poll_once(run.as_mut()).await.is_none());
                assert_eq!(
                    report_value(&read_bytes(&handler, &ctx).await.unwrap())
                        .u16()
                        .unwrap(),
                    2000
                );
                ctx.changes.borrow_mut().clear();
            }
            first_request.mark_sent().unwrap();
            let second = second_before
                .unwrap_or_else(|| function.begin_curtain(CurtainCommand::Stop).unwrap());
            let version =
                super::support::report_dataver(&read_bytes(&handler, &ctx).await.unwrap());
            // This report belongs to the sent Stop, even with another Stop queued.
            function
                .observe_curtain(CurtainValue::Status(CurtainMotion::Stopped), Some(1), 1)
                .unwrap();
            assert!(poll_once(run.as_mut()).await.is_none());
            let bytes = read_bytes(&handler, &ctx).await.unwrap();
            assert_eq!(report_value(&bytes).u16().unwrap(), 7500);
            assert_ne!(super::support::report_dataver(&bytes), version);
            assert!(ctx.changes.borrow().contains(&(2, 0x102, 11)));
            first_request.accept().unwrap();
            first.accepted().await.unwrap();
            let DeviceRequest::Curtain(second_request) = receiver.next_operation().await.unwrap()
            else {
                panic!()
            };
            assert_eq!(second_request.command, CurtainCommand::Stop);
            second_request.mark_sent().unwrap();
            second_request.accept().unwrap();
            second.accepted().await.unwrap();
            assert_eq!(
                report_value(&read_bytes(&handler, &ctx).await.unwrap())
                    .u16()
                    .unwrap(),
                7500
            );
        });
    }
}

#[test]
fn rejecting_first_stop_preserves_the_later_queued_stop() {
    setup!(device, receiver, handler, ctx);
    device.set_availability(Availability::Available);
    let function = &device.functions()[0];
    block_on(async {
        function
            .observe_curtain(CurtainValue::CurrentPosition(25), None, 1)
            .unwrap();
        function
            .observe_curtain(CurtainValue::TargetPosition(80), None, 1)
            .unwrap();
        let first = function.begin_curtain(CurtainCommand::Stop).unwrap();
        let second = function.begin_curtain(CurtainCommand::Stop).unwrap();
        let DeviceRequest::Curtain(request) = receiver.next_operation().await.unwrap() else {
            panic!()
        };
        request.mark_sent().unwrap();
        function
            .observe_curtain(CurtainValue::Status(CurtainMotion::Stopped), None, 1)
            .unwrap();
        ctx.attribute = attr(0x102, 11);
        let read_target = async || {
            report_value(&read_bytes(&handler, &ctx).await.unwrap())
                .u16()
                .unwrap()
        };
        assert_eq!(read_target().await, 7500);
        request.reject(CommandError::Timeout);
        assert_eq!(first.accepted().await.unwrap_err(), CommandError::Timeout);
        assert_eq!(read_target().await, 2000);
        let DeviceRequest::Curtain(request) = receiver.next_operation().await.unwrap() else {
            panic!()
        };
        assert_eq!(request.command, CurtainCommand::Stop);
        request.mark_sent().unwrap();
        function
            .observe_curtain(CurtainValue::Status(CurtainMotion::Stopped), None, 1)
            .unwrap();
        assert_eq!(read_target().await, 7500);
        request.accept().unwrap();
        second.accepted().await.unwrap();
    });
}

#[test]
fn later_native_target_and_resumed_motion_retire_older_stop_work() {
    setup!(device, receiver, handler, ctx);
    device.set_availability(Availability::Available);
    let function = &device.functions()[0];
    block_on(async {
        let observe = |value| function.observe_curtain(value, Some(1), 1).unwrap();
        observe(CurtainValue::CurrentPosition(25));
        observe(CurtainValue::TargetPosition(80));
        observe(CurtainValue::Status(CurtainMotion::Opening));
        ctx.attribute = attr(0x102, 11);
        let read_target = async || {
            report_value(&read_bytes(&handler, &ctx).await.unwrap())
                .u16()
                .unwrap()
        };
        let first = function.begin_curtain(CurtainCommand::Stop).unwrap();
        let second = function.begin_curtain(CurtainCommand::Stop).unwrap();
        let DeviceRequest::Curtain(request) = receiver.next_operation().await.unwrap() else {
            panic!()
        };
        request.mark_sent().unwrap();
        // Even an identical native target report retires both older Stops.
        observe(CurtainValue::TargetPosition(80));
        observe(CurtainValue::Status(CurtainMotion::Stopped));
        assert_eq!(read_target().await, 2000);
        request.accept().unwrap();
        first.accepted().await.unwrap();
        let DeviceRequest::Curtain(request) = receiver.next_operation().await.unwrap() else {
            panic!()
        };
        request.mark_sent().unwrap();
        observe(CurtainValue::Status(CurtainMotion::Stopped));
        assert_eq!(read_target().await, 2000);
        request.accept().unwrap();
        second.accepted().await.unwrap();

        // A fresh Stop can still settle, and resumed motion prevents an old
        // duplicate stopped report from taking over again.
        let fresh = function.begin_curtain(CurtainCommand::Stop).unwrap();
        let DeviceRequest::Curtain(request) = receiver.next_operation().await.unwrap() else {
            panic!()
        };
        request.mark_sent().unwrap();
        observe(CurtainValue::Status(CurtainMotion::Stopped));
        assert_eq!(read_target().await, 7500);
        observe(CurtainValue::Status(CurtainMotion::Closing));
        observe(CurtainValue::Status(CurtainMotion::Stopped));
        assert_eq!(read_target().await, 2000);
        request.accept().unwrap();
        fresh.accepted().await.unwrap();
    });
}

#[test]
fn unsent_stop_failure_leaves_the_next_stop_usable() {
    setup!(device, receiver, handler, ctx);
    device.set_availability(Availability::Available);
    let function = &device.functions()[0];
    block_on(async {
        function
            .observe_curtain(CurtainValue::CurrentPosition(25), None, 1)
            .unwrap();
        function
            .observe_curtain(CurtainValue::TargetPosition(80), None, 1)
            .unwrap();
        let first = function.begin_curtain(CurtainCommand::Stop).unwrap();
        let second = function.begin_curtain(CurtainCommand::Stop).unwrap();
        let DeviceRequest::Curtain(request) = receiver.next_operation().await.unwrap() else {
            panic!()
        };
        assert!(matches!(request.accept(), Err(CommandError::SendFailed(_))));
        assert!(matches!(
            first.accepted().await,
            Err(CommandError::SendFailed(_))
        ));
        ctx.attribute = attr(0x102, 11);
        function
            .observe_curtain(CurtainValue::Status(CurtainMotion::Stopped), None, 1)
            .unwrap();
        assert_eq!(
            report_value(&read_bytes(&handler, &ctx).await.unwrap())
                .u16()
                .unwrap(),
            2000
        );
        let DeviceRequest::Curtain(request) = receiver.next_operation().await.unwrap() else {
            panic!()
        };
        request.mark_sent().unwrap();
        function
            .observe_curtain(CurtainValue::Status(CurtainMotion::Stopped), None, 1)
            .unwrap();
        assert_eq!(
            report_value(&read_bytes(&handler, &ctx).await.unwrap())
                .u16()
                .unwrap(),
            7500
        );
        request.accept().unwrap();
        second.accepted().await.unwrap();
    });
}
