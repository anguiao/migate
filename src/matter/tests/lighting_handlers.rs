use super::*;
use crate::device::{DeviceRequest, LightDimension, LightPatch};

macro_rules! setup_light {
    ($rgb:expr, $device:ident, $receiver:ident, $registry:ident, $handler:ident, $ctx:ident) => {
        setup_light!(@source crate::matter::tests::support::light($rgb),$device,$receiver,$registry,$handler,$ctx);
    };
    (@source $source:expr, $device:ident, $receiver:ident, $registry:ident, $handler:ident, $ctx:ident) => {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let identity = store.load_identity().unwrap();
        let ($device, mut $receiver) = $source;
        let $registry = crate::matter::tests::support::test_registry(&store, &$device);
        let $handler = model::handler(&$registry, &store, StoreAdapter::new(store.matter()), rand::rng()).unwrap();
        let info = basic_info(&identity);
        let matter = Matter::new(&info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
        let buffers: MatterBuffers = MatterBuffers::new();
        let state: EthInteractionModelState = EthInteractionModelState::new(EthNetwork::new_default());
        let crypto = default_crypto(rand::rng(), DAC_PRIVKEY);
        let kv = matter.kv(StoreAdapter::new(store.matter()));
        let im = InteractionModel::new(&matter, &crypto, &buffers, &$handler, &kv, &state);
        let mut $ctx = Context::new(&im, 8, 0);
    };
}

#[test]
fn real_light_handler_reads_unknown_and_rejects_unsupported_commands_and_writes() {
    for rgb in [false, true] {
        setup_light!(rgb, device, receiver, registry, handler, ctx);
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
            for attribute in if rgb { vec![7, 8, 0x4001] } else { vec![7] } {
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
                    0,
                    &[
                        0x15, 0x24, 0, 10, 0x24, 1, 0, 0x25, 2, 0, 0, 0x24, 3, 0, 0x24, 4, 0, 0x18,
                    ][..],
                ),
            ] {
                ctx.attribute.cluster_id = cluster;
                ctx.command.cluster_id = cluster;
                ctx.command.cmd_id = command;
                ctx.data = TLVElement::new(data);
                let error = handler
                    .invoke(
                        &ctx,
                        InvokeReplyInstance::new(&ctx.command, WriteBuf::new(&mut [0; 128])),
                    )
                    .await
                    .unwrap_err();
                assert_eq!(error.code(), ErrorCode::CommandNotFound);
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
        assert_eq!(registry.list().len(), 1);
    }
}

#[test]
fn real_model_run_drives_transition_acceptance_reports_and_dataver() {
    setup_light!(false, device, receiver, registry, handler, ctx);
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
        first.skip_unchanged().unwrap();
        assert!(poll_once(invoke.as_mut()).await.is_none());
        let mut final_seen = false;
        futures_lite::future::or(
            async {
                loop {
                    let DeviceRequest::Light(request) = receiver.next_operation().await.unwrap()
                    else {
                        panic!("unexpected power frame")
                    };
                    assert_eq!(request.target.power, None);
                    let final_frame = request.final_frame;
                    request.mark_sent().unwrap();
                    function
                        .observe_light(LightPatch {
                            brightness_percent: request.target.brightness_percent,
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
    assert_eq!(registry.list().len(), 1);
}

#[test]
fn light_rebuild_retains_endpoint_and_refreshes_temperature_bounds_and_versions() {
    setup_light!(false, device, receiver, registry, handler, ctx);
    device.set_availability(Availability::Available);
    block_on(async {
        ctx.attribute = attr(0x300, 0x400b);
        let mut run = pin!(handler.run(&ctx));
        assert!(poll_once(run.as_mut()).await.is_none());
        let before = report_dataver(&read_bytes(&handler, &ctx).await.unwrap());
        assert_eq!(
            report_value(&read_bytes(&handler, &ctx).await.unwrap())
                .u16()
                .unwrap(),
            154
        );
        let mut description = device.description();
        description.functions[0].capabilities[2] =
            crate::device::Capability::ColorTemperature(crate::device::NumericRange {
                min: 3000,
                max: 4000,
                step: 1,
            });
        let (replacement, _receivers) = crate::device::PhysicalDevice::new(description);
        replacement.set_availability(Availability::Available);
        registry.remove(device.key()).unwrap();
        registry.insert(replacement);
        assert!(poll_once(run.as_mut()).await.is_none());
        assert_eq!(
            report_value(&read_bytes(&handler, &ctx).await.unwrap())
                .u16()
                .unwrap(),
            250
        );
        assert_ne!(
            report_dataver(&read_bytes(&handler, &ctx).await.unwrap()),
            before
        );
        handler.access(|node| assert!(node.endpoints.iter().any(|e| e.id == 2)));
        assert!(receiver.next_operation().await.is_none());
    });
}

#[test]
fn real_handler_level_percent_raw_chain_keeps_high_resolution_and_actual_temperature_range() {
    for fixture in [
        include_str!("../../../tests/fixtures/miot/yeelink-light-light3.json"),
        include_str!("../../../tests/fixtures/miot/xiaomi-light-ceil04.json"),
    ] {
        let instance = serde_json::from_str(fixture).unwrap();
        let parsed = crate::xiaomi::capability::parse_device(
            crate::device::DeviceKey::new("xiaomi", "precision-light"),
            &instance,
        )
        .unwrap();
        let binding = parsed.functions[0].lighting.clone().unwrap();
        let description = crate::device::PhysicalDeviceDescription {
            key: parsed.key,
            label: "Precision".into(),
            functions: parsed
                .functions
                .into_iter()
                .map(|f| f.description)
                .collect(),
        };
        let (device, mut receivers) = crate::device::PhysicalDevice::new(description);
        setup_light!(@source (device,receivers.remove(0)),device,receiver,registry,handler,ctx);
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
                let percent = request.target.brightness_percent.unwrap();
                assert_eq!(
                    percent,
                    crate::device::brightness_from_matter_level(111).unwrap()
                );
                let raw = binding.brightness.brightness_to_raw(percent).unwrap();
                let actual = binding
                    .brightness
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
                    crate::device::brightness_to_matter_level(actual).unwrap()
                );
                if binding.brightness.range.max == 65535 {
                    assert!(actual.fract() > 0.0);
                    assert!((actual - percent).abs() < 0.002);
                }
            }
            ctx.attribute = attr(0x300, 0x400b);
            assert_eq!(
                report_value(&read_bytes(&handler, &ctx).await.unwrap())
                    .u16()
                    .unwrap(),
                binding.temperature.range.mired_bounds().unwrap().0
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
                    request.target.color,
                    Some(crate::device::LightColorTarget::Temperature(3333))
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
        assert_eq!(registry.list().len(), 1);
    }
}

#[test]
fn actual_onlevel_write_affects_ordinary_on_and_off_but_min_with_onoff_closes_power() {
    setup_light!(false, device, receiver, registry, handler, ctx);
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
            assert_eq!(request.target.power, Some(power));
            assert_eq!(request.target.brightness_percent, Some(1.0));
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
            assert_eq!(request.target.brightness_percent, Some(1.0));
            assert_eq!(
                request.target.power,
                if command == 4 { Some(false) } else { None }
            );
            request.skip_unchanged().unwrap();
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
    assert_eq!(registry.list().len(), 1);
}
