use super::*;

#[test]
fn real_model_run_drives_transition_acceptance_reports_and_dataver() {
    setup_light!(false, device, receiver, handler, ctx);
    let function = device.functions()[0].clone();
    device.set_availability(Availability::Available);
    function
        .observe_light(LightPatch {
            power: Some(true),
            brightness_percent: Some(1.0),
            ..Default::default()
        })
        .unwrap();
    block_on(async {
        let before = report_dataver(&read_bytes(&handler, &ctx).await.unwrap());
        ctx.command.cluster_id = 8;
        ctx.command.cmd_id = 0;
        ctx.data = TLVElement::new(&[
            0x15, 0x24, 0, 254, 0x25, 1, 1, 0, 0x24, 2, 0, 0x24, 3, 0, 0x18,
        ]);
        let mut output = [0; 128];
        let mut invoke = pin!(handler.invoke(
            &ctx,
            InvokeReplyInstance::new(&ctx.command, WriteBuf::new(&mut output))
        ));
        let mut run = pin!(handler.run(&ctx));
        assert!(poll_once(run.as_mut()).await.is_none());
        assert!(poll_once(invoke.as_mut()).await.is_none());
        let DeviceRequest::Light(first) = receiver.next_operation().await.unwrap() else {
            panic!("missing light startup")
        };
        first.accept_unchanged().unwrap();
        assert!(poll_once(invoke.as_mut()).await.is_none());
        let mut final_seen = false;
        futures_lite::future::or(
            async {
                loop {
                    let DeviceRequest::Light(request) = receiver.next_operation().await.unwrap()
                    else {
                        panic!("unexpected power frame")
                    };
                    assert_eq!(request.target.power(), None);
                    let final_frame = request.final_frame;
                    request.mark_sent().unwrap();
                    function
                        .observe_light(LightPatch {
                            brightness_percent: request.target.brightness_percent(),
                            ..Default::default()
                        })
                        .unwrap();
                    request.accept().unwrap();
                    if final_frame {
                        final_seen = true;
                        break;
                    }
                }
            },
            async {
                run.as_mut().await.unwrap();
            },
        )
        .await;
        invoke.await.unwrap();
        assert!(final_seen);
        assert!(poll_once(run.as_mut()).await.is_none());
        assert_eq!(
            report_value(&read_bytes(&handler, &ctx).await.unwrap())
                .u8()
                .unwrap(),
            254
        );
        assert_ne!(
            report_dataver(&read_bytes(&handler, &ctx).await.unwrap()),
            before
        );
        assert!(ctx.changes.borrow().contains(&(2, 8, 0)));
        assert!(!ctx.changes.borrow().contains(&(2, 8, 1)));
        function
            .take_over_light(LightDimension::Brightness)
            .unwrap();
    });
}
