use super::*;

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
        function.observe_curtain(CurtainValue::CurrentPosition(37), Some(1), 2);
        function.observe_curtain(CurtainValue::TargetPosition(80), Some(1), 2);
        for (id, expected) in [(8, 63), (11, 2000), (14, 6300)] {
            ctx.attribute = attr(0x102, id);
            assert_eq!(read_u16(&handler, &ctx).await, expected);
        }
        for (motion, expected) in [
            (CurtainMotion::Opening, 5),
            (CurtainMotion::Closing, 10),
            (CurtainMotion::Stopped, 0),
        ] {
            function.observe_curtain(CurtainValue::Status(motion), Some(1), 2);
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
fn curtain_reports_only_changed_projection_and_preserves_values_on_disconnect() {
    setup!(device, receiver, handler, ctx);
    let _ = &mut receiver;
    device.set_availability(Availability::Available);
    let function = &device.functions()[0];
    block_on(async {
        function.observe_curtain(CurtainValue::CurrentPosition(20), Some(1), 1);
        function.observe_curtain(CurtainValue::TargetPosition(80), Some(1), 1);
        function.observe_curtain(CurtainValue::Status(CurtainMotion::Opening), Some(1), 1);
        ctx.attribute = attr(0x102, 11);
        let mut run = pin!(handler.run(&ctx));
        assert!(poll_once(run.as_mut()).await.is_none());
        ctx.changes.borrow_mut().clear();
        let version = report_dataver(&read_bytes(&handler, &ctx).await.unwrap());
        function.observe_curtain(CurtainValue::CurrentPosition(21), Some(2), 2);
        assert!(poll_once(run.as_mut()).await.is_none());
        assert_eq!(&*ctx.changes.borrow(), &[(2, 0x102, 8), (2, 0x102, 14)]);
        let next = report_dataver(&read_bytes(&handler, &ctx).await.unwrap());
        assert_ne!(next, version);
        ctx.changes.borrow_mut().clear();
        function.observe_curtain(CurtainValue::CurrentPosition(21), Some(3), 3);
        assert!(poll_once(run.as_mut()).await.is_none());
        assert!(ctx.changes.borrow().is_empty());
        assert_eq!(
            report_dataver(&read_bytes(&handler, &ctx).await.unwrap()),
            next
        );
        function.observe_curtain(CurtainValue::TargetPosition(70), Some(4), 4);
        function.observe_curtain(CurtainValue::Status(CurtainMotion::Closing), Some(4), 4);
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
        assert_eq!(read_u16(&handler, &ctx).await, 3000);
    });
}

#[test]
fn queued_later_stops_do_not_mask_feedback_from_the_first_sent_stop() {
    for both_queued_before_send in [false, true] {
        setup!(device, receiver, handler, ctx);
        device.set_availability(Availability::Available);
        let function = &device.functions()[0];
        block_on(async {
            function.observe_curtain(CurtainValue::CurrentPosition(25), Some(1), 1);
            function.observe_curtain(CurtainValue::TargetPosition(80), Some(1), 1);
            function.observe_curtain(CurtainValue::Status(CurtainMotion::Opening), Some(1), 1);
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
            let first_request = next_curtain(&mut receiver).await;
            if both_queued_before_send {
                // An old cached stopped status cannot arm an unsent request.
                function.observe_curtain(CurtainValue::Status(CurtainMotion::Stopped), Some(1), 1);
                assert!(poll_once(run.as_mut()).await.is_none());
                assert_eq!(read_u16(&handler, &ctx).await, 2000);
                ctx.changes.borrow_mut().clear();
            }
            first_request.mark_sent().unwrap();
            let second = second_before
                .unwrap_or_else(|| function.begin_curtain(CurtainCommand::Stop).unwrap());
            let version = report_dataver(&read_bytes(&handler, &ctx).await.unwrap());
            // This report belongs to the sent Stop, even with another Stop queued.
            function.observe_curtain(CurtainValue::Status(CurtainMotion::Stopped), Some(1), 1);
            assert!(poll_once(run.as_mut()).await.is_none());
            let bytes = read_bytes(&handler, &ctx).await.unwrap();
            assert_eq!(report_value(&bytes).u16().unwrap(), 7500);
            assert_ne!(report_dataver(&bytes), version);
            assert!(ctx.changes.borrow().contains(&(2, 0x102, 11)));
            first_request.accept().unwrap();
            first.accepted().await.unwrap();
            let second_request = next_curtain(&mut receiver).await;
            assert_eq!(second_request.command, CurtainCommand::Stop);
            second_request.mark_sent().unwrap();
            second_request.accept().unwrap();
            second.accepted().await.unwrap();
            assert_eq!(read_u16(&handler, &ctx).await, 7500);
        });
    }
}
