use super::*;

#[test]
fn real_light_handler_reads_unknown_and_rejects_unsupported_commands_and_writes() {
    for rgb in [false, true] {
        setup_light!(rgb, device, receiver, handler, ctx);
        device.set_availability(Availability::Available);
        block_on(async {
            assert!(
                report_value(&read_bytes(&handler, &ctx).await.unwrap())
                    .null()
                    .is_ok()
            );
            ctx.attribute = attr(0x300, 16);
            assert!(
                report_value(&read_bytes(&handler, &ctx).await.unwrap())
                    .null()
                    .is_ok()
            );
            for attribute in if rgb {
                vec![0, 1, 7, 8, 0x4001]
            } else {
                vec![7]
            } {
                ctx.attribute = attr(0x300, attribute);
                assert_eq!(
                    read_bytes(&handler, &ctx).await.unwrap_err().code(),
                    ErrorCode::Failure
                );
            }
            for (cluster, command, data) in [
                (3, 0x40, &[0x15, 0x24, 0, 0, 0x24, 1, 0, 0x18][..]),
                (6, 0x40, &[0x15, 0x24, 0, 0, 0x24, 1, 0, 0x18][..]),
                (8, 8, &[0x15, 0x25, 0, 100, 0, 0x18][..]),
                (
                    0x300,
                    if rgb { 0x40 } else { 0 },
                    &[
                        0x15, 0x24, 0, 10, 0x24, 1, 0, 0x25, 2, 0, 0, 0x24, 3, 0, 0x24, 4, 0, 0x18,
                    ][..],
                ),
            ] {
                ctx.attribute.cluster_id = cluster;
                ctx.command.cluster_id = cluster;
                ctx.command.cmd_id = command;
                ctx.data = TLVElement::new(data);
                let error = match handler
                    .invoke(
                        &ctx,
                        InvokeReplyInstance::new(&ctx.command, WriteBuf::new(&mut [0; 128])),
                    )
                    .await
                {
                    Ok(()) => panic!(
                        "cluster {cluster} command {command} rgb={rgb} unexpectedly succeeded"
                    ),
                    Err(error) => error,
                };
                assert_eq!(
                    error.code(),
                    ErrorCode::CommandNotFound,
                    "cluster {cluster} command {command} rgb={rgb}"
                );
                assert!(poll_once(receiver.next_operation()).await.is_none());
            }
            ctx.attribute = attr(8, 15);
            ctx.data = TLVElement::new(&[0x04, 2]);
            assert_eq!(
                handler.write(&ctx).await.unwrap_err().code(),
                ErrorCode::ConstraintError
            );
            ctx.data = TLVElement::new(&[0x04, 1]);
            handler.write(&ctx).await.unwrap();
            ctx.attribute = attr(8, 17);
            ctx.data = TLVElement::new(&[0x04, 255]);
            assert_eq!(
                handler.write(&ctx).await.unwrap_err().code(),
                ErrorCode::ConstraintError
            );
            assert!(poll_once(receiver.next_operation()).await.is_none());
        });
    }
}

#[test]
fn real_handler_level_percent_raw_chain_keeps_high_resolution_and_actual_temperature_range() {
    for fixture in [
        include_str!("../../../../tests/fixtures/miot/yeelink-light-light3.json"),
        include_str!("../../../../tests/fixtures/miot/xiaomi-light-ceil04.json"),
    ] {
        let instance = serde_json::from_str(fixture).unwrap();
        let parsed = crate::xiaomi::capability::tests::parse_device(
            crate::device::DeviceKey::new("xiaomi", "precision-light"),
            &instance,
        )
        .unwrap();
        let binding = parsed.functions()[0].lighting().cloned().unwrap();
        let description = parsed.physical_description("Precision");
        let (device, mut receivers) = crate::device::PhysicalDevice::new(description);
        setup_light!(@source (device,receivers.remove(0)),device,receiver,handler,ctx);
        let function = device.functions()[0].clone();
        device.set_availability(Availability::Available);
        function.observe(true);
        block_on(async {
            ctx.command.cluster_id = 8;
            ctx.command.cmd_id = 0;
            ctx.data = TLVElement::new(&[
                0x15, 0x24, 0, 111, 0x25, 1, 0, 0, 0x24, 2, 0, 0x24, 3, 0, 0x18,
            ]);
            {
                let mut output = [0; 128];
                let mut invoke = pin!(handler.invoke(
                    &ctx,
                    InvokeReplyInstance::new(&ctx.command, WriteBuf::new(&mut output))
                ));
                assert!(poll_once(invoke.as_mut()).await.is_none());
                let DeviceRequest::Light(request) = receiver.next_operation().await.unwrap() else {
                    panic!("missing level request")
                };
                let percent = request.target.brightness_percent().unwrap();
                assert_eq!(percent, crate::matter::units::brightness_from_level(111));
                let raw = binding.brightness().unwrap().brightness_to_raw(percent);
                let actual = binding
                    .brightness()
                    .unwrap()
                    .brightness_from_raw(&serde_json::json!(raw))
                    .unwrap();
                request.mark_sent().unwrap();
                function
                    .observe_light(LightPatch {
                        brightness_percent: Some(actual),
                        ..Default::default()
                    })
                    .unwrap();
                request.accept().unwrap();
                invoke.await.unwrap();
                assert_eq!(
                    report_value(&read_bytes(&handler, &ctx).await.unwrap())
                        .u8()
                        .unwrap(),
                    crate::matter::units::level_from_brightness(actual)
                );
                if binding.brightness().unwrap().range().max() == 65535 {
                    assert!(actual.fract() > 0.0);
                    assert!((actual - percent).abs() < 0.002);
                }
            }
            ctx.attribute = attr(0x300, 0x400b);
            assert_eq!(
                report_value(&read_bytes(&handler, &ctx).await.unwrap())
                    .u16()
                    .unwrap(),
                crate::matter::units::mired_bounds(binding.temperature().unwrap().interval(),).0
            );
            ctx.command.cluster_id = 0x300;
            ctx.command.cmd_id = 10;
            ctx.data = TLVElement::new(&[
                0x15, 0x25, 0, 44, 1, 0x25, 1, 0, 0, 0x24, 2, 0, 0x24, 3, 0, 0x18,
            ]);
            {
                let mut output = [0; 128];
                let mut invoke = pin!(handler.invoke(
                    &ctx,
                    InvokeReplyInstance::new(&ctx.command, WriteBuf::new(&mut output))
                ));
                assert!(poll_once(invoke.as_mut()).await.is_none());
                let DeviceRequest::Light(request) = receiver.next_operation().await.unwrap() else {
                    panic!("missing temperature request")
                };
                assert_eq!(
                    request.target.color_target(),
                    Some(crate::device::LightColorTarget::Temperature(
                        1_000_000.0 / 300.0
                    ))
                );
                request.mark_sent().unwrap();
                function
                    .observe_light(LightPatch {
                        temperature_kelvin: Some(3333),
                        mode: Some(crate::device::LightMode::Temperature),
                        ..Default::default()
                    })
                    .unwrap();
                request.accept().unwrap();
                invoke.await.unwrap();
            }
            ctx.attribute = attr(0x300, 7);
            assert_eq!(
                report_value(&read_bytes(&handler, &ctx).await.unwrap())
                    .u16()
                    .unwrap(),
                300
            );
        });
    }
}

#[test]
fn actual_onlevel_write_affects_ordinary_on_and_off_but_min_with_onoff_closes_power() {
    setup_light!(false, device, receiver, handler, ctx);
    let function = device.functions()[0].clone();
    device.set_availability(Availability::Available);
    function
        .observe_light(LightPatch {
            power: Some(false),
            brightness_percent: Some(60.0),
            ..Default::default()
        })
        .unwrap();
    block_on(async {
        ctx.attribute = attr(8, 17);
        ctx.data = TLVElement::new(&[0x04, 1]);
        handler.write(&ctx).await.unwrap();
        for (command, power) in [(1, true), (0, false)] {
            ctx.attribute.cluster_id = 6;
            ctx.command.cluster_id = 6;
            ctx.command.cmd_id = command;
            ctx.data = TLVElement::new(&[0x15, 0x18]);
            let mut output = [0; 128];
            let mut invoke = pin!(handler.invoke(
                &ctx,
                InvokeReplyInstance::new(&ctx.command, WriteBuf::new(&mut output))
            ));
            let result = poll_once(invoke.as_mut()).await;
            assert!(result.is_none(), "{result:?}");
            let DeviceRequest::Light(request) = receiver.next_operation().await.unwrap() else {
                panic!("missing onlevel action")
            };
            assert_eq!(request.target.power(), Some(power));
            assert_eq!(request.target.brightness_percent(), Some(1.0));
            request.mark_sent().unwrap();
            function
                .observe_light(LightPatch {
                    power: Some(power),
                    brightness_percent: Some(1.0),
                    ..Default::default()
                })
                .unwrap();
            request.accept().unwrap();
            invoke.await.unwrap();
        }
        function.observe(true);
        for command in [0, 4] {
            ctx.attribute.cluster_id = 8;
            ctx.command.cluster_id = 8;
            ctx.command.cmd_id = command;
            ctx.data = TLVElement::new(&[
                0x15, 0x24, 0, 0, 0x25, 1, 0, 0, 0x24, 2, 0, 0x24, 3, 0, 0x18,
            ]);
            let mut output = [0; 128];
            let mut invoke = pin!(handler.invoke(
                &ctx,
                InvokeReplyInstance::new(&ctx.command, WriteBuf::new(&mut output))
            ));
            let result = poll_once(invoke.as_mut()).await;
            assert!(result.is_none(), "{result:?}");
            let DeviceRequest::Light(request) = receiver.next_operation().await.unwrap() else {
                panic!("missing min action")
            };
            assert_eq!(request.target.brightness_percent(), Some(1.0));
            assert_eq!(
                request.target.power(),
                if command == 4 { Some(false) } else { None }
            );
            request.accept_unchanged().unwrap();
            invoke.await.unwrap();
        }
        ctx.command.cmd_id = 4;
        ctx.data = TLVElement::new(&[
            0x15, 0x24, 0, 255, 0x25, 1, 0, 0, 0x24, 2, 0, 0x24, 3, 0, 0x18,
        ]);
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
        assert!(poll_once(receiver.next_operation()).await.is_none());
    });
}

#[test]
fn real_rgb_dispatch_accepts_hs_commands_and_reports_hs_attributes() {
    use crate::device::{LightColorTarget, LightMode};

    setup_light!(true, device, receiver, handler, ctx);
    device.set_availability(Availability::Available);
    let function = device.functions()[0].clone();
    function.observe(true);
    block_on(async {
        ctx.attribute = attr(0x300, 0x400a);
        let before = report_dataver(&read_bytes(&handler, &ctx).await.unwrap());
        assert_eq!(
            report_value(&read_bytes(&handler, &ctx).await.unwrap())
                .u16()
                .unwrap(),
            0x19
        );
        for (command, data, hue, saturation) in [
            (
                6,
                &[
                    0x15, 0x24, 0, 0, 0x24, 1, 254, 0x25, 2, 0, 0, 0x24, 3, 0, 0x24, 4, 0, 0x18,
                ][..],
                0,
                254,
            ),
            (
                0,
                &[
                    0x15, 0x24, 0, 85, 0x24, 1, 0, 0x25, 2, 0, 0, 0x24, 3, 0, 0x24, 4, 0, 0x18,
                ][..],
                85,
                254,
            ),
            (
                2,
                &[
                    0x15, 0x24, 0, 1, 0x24, 1, 84, 0x24, 2, 0, 0x24, 3, 0, 0x24, 4, 0, 0x18,
                ][..],
                169,
                254,
            ),
            (
                3,
                &[
                    0x15, 0x24, 0, 0, 0x25, 1, 0, 0, 0x24, 2, 0, 0x24, 3, 0, 0x18,
                ][..],
                169,
                0,
            ),
            (
                5,
                &[
                    0x15, 0x24, 0, 1, 0x24, 1, 254, 0x24, 2, 0, 0x24, 3, 0, 0x24, 4, 0, 0x18,
                ][..],
                169,
                254,
            ),
        ] {
            ctx.command.cluster_id = 0x300;
            ctx.command.cmd_id = command;
            ctx.data = TLVElement::new(data);
            {
                let mut output = [0; 128];
                let mut invoke = pin!(handler.invoke(
                    &ctx,
                    InvokeReplyInstance::new(&ctx.command, WriteBuf::new(&mut output)),
                ));
                assert!(poll_once(invoke.as_mut()).await.is_none());
                let DeviceRequest::Light(request) = receiver.next_operation().await.unwrap() else {
                    panic!("missing HS request")
                };
                let Some(LightColorTarget::Rgb(rgb)) = request.target.color_target() else {
                    panic!("HS command did not dispatch RGB")
                };
                assert!(request.final_frame);
                assert_eq!(request.target.power(), None);
                assert_eq!(request.target.brightness_percent(), None);
                match (hue, saturation) {
                    (0, 254) => assert_eq!((rgb.red, rgb.green, rgb.blue), (255, 0, 0)),
                    (85, 254) => {
                        assert_eq!((rgb.red, rgb.green), (0, 255));
                        assert!(rgb.blue <= 3);
                    }
                    (169, 254) => {
                        assert_eq!((rgb.red, rgb.blue), (0, 255));
                        assert!(rgb.green <= 3);
                    }
                    (_, 0) => assert_eq!((rgb.red, rgb.green, rgb.blue), (255, 255, 255)),
                    _ => unreachable!(),
                }
                request.mark_sent().unwrap();
                function
                    .observe_light(LightPatch {
                        color: Some(rgb),
                        mode: Some(LightMode::Rgb),
                        ..Default::default()
                    })
                    .unwrap();
                request.accept().unwrap();
                invoke.await.unwrap();
            }
            for (attribute, expected) in [(0, hue), (1, saturation), (8, 0), (0x4001, 0)] {
                ctx.attribute = attr(0x300, attribute);
                let bytes = read_bytes(&handler, &ctx).await.unwrap();
                assert_eq!(report_value(&bytes).u8().unwrap(), expected);
                assert_ne!(report_dataver(&bytes), before);
            }
        }
        for command in [1, 4] {
            ctx.command.cmd_id = command;
            ctx.data =
                TLVElement::new(&[0x15, 0x24, 0, 0, 0x24, 1, 0, 0x24, 2, 0, 0x24, 3, 0, 0x18]);
            handler
                .invoke(
                    &ctx,
                    InvokeReplyInstance::new(&ctx.command, WriteBuf::new(&mut [0; 128])),
                )
                .await
                .unwrap();
        }
        let mut run = pin!(handler.run(&ctx));
        assert!(poll_once(run.as_mut()).await.is_none());
        assert!(ctx.changes.borrow().contains(&(2, 0x300, 0)));
        assert!(ctx.changes.borrow().contains(&(2, 0x300, 1)));
        assert!(poll_once(receiver.next_operation()).await.is_none());
    });
}
