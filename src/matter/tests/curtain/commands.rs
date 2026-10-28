use super::*;

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
            let request = next_curtain(&mut receiver).await;
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
            let request = next_curtain(&mut receiver).await;
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
        let observe = |value| function.observe_curtain(value, Some(7), 7);
        observe(CurtainValue::CurrentPosition(20));
        observe(CurtainValue::TargetPosition(80));
        observe(CurtainValue::Status(CurtainMotion::Opening));
        ctx.attribute = attr(0x102, 11);
        let read_target = async || read_u16(&handler, &ctx).await;
        assert_eq!(read_target().await, 2000);

        // A stopped report before the request is sent cannot complete it.
        let first = function.begin_curtain(CurtainCommand::Stop).unwrap();
        let request = next_curtain(&mut receiver).await;
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
        let next_request = next_curtain(&mut receiver).await;
        next_request.mark_sent().unwrap();
        next_request.accept().unwrap();
        next.accepted().await.unwrap();

        // A report for the unchanged target clears stop handling even when all
        // report timestamps and values repeat within one processing turn.
        observe(CurtainValue::Status(CurtainMotion::Opening));
        let stop = function.begin_curtain(CurtainCommand::Stop).unwrap();
        let request = next_curtain(&mut receiver).await;
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
        let request = next_curtain(&mut receiver).await;
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
