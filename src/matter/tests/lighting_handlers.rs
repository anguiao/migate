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
fn light_reads_sync_cluster_versions_before_filtering_without_polling_run() {
    use crate::device::{LightMode, Rgb};

    for (patch, cluster, changed_attribute) in [
        (
            LightPatch {
                power: Some(false),
                ..Default::default()
            },
            6,
            0,
        ),
        (
            LightPatch {
                brightness_percent: Some(60.0),
                ..Default::default()
            },
            8,
            0,
        ),
        (
            LightPatch {
                temperature_kelvin: Some(5000),
                ..Default::default()
            },
            0x300,
            7,
        ),
        (
            LightPatch {
                color: Some(Rgb {
                    red: 0,
                    green: 255,
                    blue: 0,
                }),
                ..Default::default()
            },
            0x300,
            3,
        ),
        (
            LightPatch {
                mode: Some(LightMode::Rgb),
                ..Default::default()
            },
            0x300,
            8,
        ),
    ] {
        setup_light!(true, device, receiver, registry, handler, ctx);
        let function = device.functions()[0].clone();
        function
            .observe_light(LightPatch {
                power: Some(true),
                brightness_percent: Some(50.0),
                temperature_kelvin: Some(4000),
                color: Some(Rgb {
                    red: 255,
                    green: 0,
                    blue: 0,
                }),
                mode: Some(LightMode::Temperature),
            })
            .unwrap();
        block_on(async {
            let reports = Context::new(ctx.base, 8, 0);
            let mut run = pin!(handler.run(&reports));
            assert!(poll_once(run.as_mut()).await.is_none());
            reports.changes.borrow_mut().clear();
            let mut baseline = Vec::new();
            for (cluster, attribute) in [
                (6, 0),
                (8, 0),
                (8, 2),
                (0x300, 0),
                (0x300, 1),
                (0x300, 7),
                (0x300, 3),
                (0x300, 4),
                (0x300, 8),
                (0x300, 0x4001),
                (0x300, 0x400b),
            ] {
                ctx.attribute = attr(cluster, attribute);
                baseline.push((
                    cluster,
                    attribute,
                    read_bytes(&handler, &ctx).await.unwrap(),
                ));
            }
            function.observe_light(patch).unwrap();

            let mut latest = None;
            for (attribute_cluster, attribute, before) in baseline {
                ctx.attribute = attr(attribute_cluster, attribute);
                ctx.attribute.dataver = Some(report_dataver(&before));
                let filtered = read_bytes(&handler, &ctx).await.unwrap();
                if attribute_cluster == cluster && patch.color.is_none() {
                    assert!(!filtered.is_empty(), "new cluster state was filtered out");
                    let version = report_dataver(&filtered);
                    assert_ne!(version, report_dataver(&before));
                    if let Some(previous) = latest {
                        assert_eq!(version, previous);
                    }
                    latest = Some(version);
                    if attribute == changed_attribute {
                        if cluster == 6 {
                            assert_ne!(
                                report_value(&filtered).bool().unwrap(),
                                report_value(&before).bool().unwrap()
                            );
                        } else {
                            assert_ne!(
                                report_value(&filtered).u32().unwrap(),
                                report_value(&before).u32().unwrap()
                            );
                        }
                    }
                    ctx.attribute.dataver = None;
                    assert_eq!(read_bytes(&handler, &ctx).await.unwrap(), filtered);
                    ctx.attribute.dataver = Some(version);
                    assert!(read_bytes(&handler, &ctx).await.unwrap().is_empty());
                } else {
                    assert!(filtered.is_empty(), "unrelated cluster version changed");
                }
            }
            assert!(reports.changes.borrow().is_empty());
            assert!(poll_once(run.as_mut()).await.is_none());
            assert_eq!(
                reports
                    .changes
                    .borrow()
                    .contains(&(2, cluster, changed_attribute)),
                patch.color.is_none(),
                "inactive RGB cache changes do not change the active CT projection"
            );
            assert!(poll_once(receiver.next_operation()).await.is_none());
        });
        assert_eq!(registry.list().len(), 1);
    }
}

#[test]
fn ct_projection_reads_ignore_inactive_rgb_and_rgb_keeps_its_ct_cache_readable() {
    use crate::device::{LightMode, Rgb};

    setup_light!(true, device, receiver, registry, handler, ctx);
    let function = device.functions()[0].clone();
    function
        .observe_light(LightPatch {
            power: Some(true),
            temperature_kelvin: Some(6500),
            color: Some(Rgb {
                red: 255,
                green: 0,
                blue: 0,
            }),
            mode: Some(LightMode::Temperature),
            ..Default::default()
        })
        .unwrap();
    block_on(async {
        let reports = Context::new(ctx.base, 0x300, 0);
        let mut run = pin!(handler.run(&reports));
        assert!(poll_once(run.as_mut()).await.is_none());
        reports.changes.borrow_mut().clear();
        let mut baseline = Vec::new();
        for (attribute, expected) in [
            (0, 218),
            (1, 7),
            (3, 20554),
            (4, 21209),
            (7, 154),
            (8, 2),
            (0x4001, 2),
        ] {
            ctx.attribute = attr(0x300, attribute);
            let bytes = read_bytes(&handler, &ctx).await.unwrap();
            assert_eq!(report_value(&bytes).u16().unwrap(), expected);
            baseline.push((attribute, bytes));
        }
        let blue = Rgb {
            red: 0,
            green: 0,
            blue: 255,
        };
        function
            .observe_light(LightPatch {
                color: Some(blue),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(function.snapshot().lighting.unwrap().color, Some(blue));
        for (attribute, before) in &baseline {
            ctx.attribute = attr(0x300, *attribute);
            assert_eq!(read_bytes(&handler, &ctx).await.unwrap(), *before);
            ctx.attribute.dataver = Some(report_dataver(before));
            assert!(read_bytes(&handler, &ctx).await.unwrap().is_empty());
        }
        assert!(poll_once(run.as_mut()).await.is_none());
        assert!(reports.changes.borrow().is_empty());

        function
            .observe_light(LightPatch {
                mode: Some(LightMode::Rgb),
                ..Default::default()
            })
            .unwrap();
        for (attribute, expected) in [
            (0, 169),
            (1, 254),
            (3, 9830),
            (4, 3932),
            (7, 154),
            (8, 0),
            (0x4001, 0),
        ] {
            ctx.attribute = attr(0x300, attribute);
            let bytes = read_bytes(&handler, &ctx).await.unwrap();
            assert_eq!(report_value(&bytes).u16().unwrap(), expected);
            assert_ne!(report_dataver(&bytes), report_dataver(&baseline[0].1));
        }
        assert!(poll_once(run.as_mut()).await.is_none());
        assert!(!reports.changes.borrow().contains(&(2, 0x300, 7)));
        reports.changes.borrow_mut().clear();
        ctx.attribute = attr(0x300, 7);
        let before = read_bytes(&handler, &ctx).await.unwrap();
        function
            .observe_light(LightPatch {
                temperature_kelvin: Some(2700),
                ..Default::default()
            })
            .unwrap();
        let after = read_bytes(&handler, &ctx).await.unwrap();
        assert_eq!(report_value(&after).u16().unwrap(), 370);
        assert_ne!(report_dataver(&after), report_dataver(&before));
        assert!(poll_once(run.as_mut()).await.is_none());
        assert!(reports.changes.borrow().is_empty());
        assert!(poll_once(receiver.next_operation()).await.is_none());
    });
    assert_eq!(registry.list().len(), 1);
}

#[test]
fn light_unfiltered_reads_use_new_versions_and_encoded_equal_values_keep_versions() {
    use crate::device::{LightMode, Rgb};

    setup_light!(true, device, receiver, registry, handler, ctx);
    let function = device.functions()[0].clone();
    function
        .observe_light(LightPatch {
            power: Some(true),
            brightness_percent: Some(50.0),
            temperature_kelvin: Some(4000),
            color: Some(Rgb {
                red: 255,
                green: 0,
                blue: 0,
            }),
            mode: Some(LightMode::Rgb),
        })
        .unwrap();
    block_on(async {
        let reports = Context::new(ctx.base, 8, 0);
        let mut run = pin!(handler.run(&reports));
        assert!(poll_once(run.as_mut()).await.is_none());
        reports.changes.borrow_mut().clear();
        let mut baseline = Vec::new();
        for (cluster, attribute) in [(6, 0), (8, 0), (0x300, 7), (0x300, 3), (0x300, 4)] {
            ctx.attribute = attr(cluster, attribute);
            baseline.push((
                cluster,
                attribute,
                read_bytes(&handler, &ctx).await.unwrap(),
            ));
        }
        function
            .observe_light(LightPatch {
                brightness_percent: Some(50.01),
                temperature_kelvin: Some(4001),
                color: Some(Rgb {
                    red: 128,
                    green: 0,
                    blue: 0,
                }),
                ..Default::default()
            })
            .unwrap();
        for (cluster, attribute, before) in baseline {
            ctx.attribute = attr(cluster, attribute);
            assert_eq!(read_bytes(&handler, &ctx).await.unwrap(), before);
            ctx.attribute.dataver = Some(report_dataver(&before));
            assert!(read_bytes(&handler, &ctx).await.unwrap().is_empty());
        }
        assert!(poll_once(run.as_mut()).await.is_none());
        assert!(reports.changes.borrow().is_empty());

        ctx.attribute = attr(8, 0);
        let before = read_bytes(&handler, &ctx).await.unwrap();
        function
            .observe_light(LightPatch {
                brightness_percent: Some(75.0),
                ..Default::default()
            })
            .unwrap();
        let after = read_bytes(&handler, &ctx).await.unwrap();
        assert_ne!(report_dataver(&after), report_dataver(&before));
        assert_eq!(report_value(&after).u8().unwrap(), 190);
        assert!(poll_once(receiver.next_operation()).await.is_none());
    });
    assert_eq!(registry.list().len(), 1);
}

#[test]
fn light_unknown_projection_edges_invalidate_filters_before_run() {
    use crate::device::{LightMode, Rgb};

    setup_light!(true, device, receiver, registry, handler, ctx);
    let function = device.functions()[0].clone();
    block_on(async {
        let level_before = report_dataver(&read_bytes(&handler, &ctx).await.unwrap());
        ctx.attribute = attr(0x300, 0x400a);
        let color_before = report_dataver(&read_bytes(&handler, &ctx).await.unwrap());
        function
            .observe_light(LightPatch {
                brightness_percent: Some(25.0),
                temperature_kelvin: Some(4000),
                color: Some(Rgb {
                    red: 255,
                    green: 0,
                    blue: 0,
                }),
                mode: Some(LightMode::Rgb),
                ..Default::default()
            })
            .unwrap();
        ctx.attribute = attr(8, 0);
        ctx.attribute.dataver = Some(level_before);
        let level = read_bytes(&handler, &ctx).await.unwrap();
        assert!(!level.is_empty());
        assert_eq!(report_value(&level).u8().unwrap(), 62);
        ctx.attribute = attr(0x300, 7);
        ctx.attribute.dataver = Some(color_before);
        let color = read_bytes(&handler, &ctx).await.unwrap();
        assert!(!color.is_empty());
        assert_eq!(report_value(&color).u16().unwrap(), 250);
        function
            .observe_light(LightPatch {
                color: Some(Rgb {
                    red: 0,
                    green: 0,
                    blue: 0,
                }),
                ..Default::default()
            })
            .unwrap();
        ctx.attribute = attr(0x300, 3);
        ctx.attribute.dataver = Some(report_dataver(&color));
        assert_eq!(
            read_bytes(&handler, &ctx).await.unwrap_err().code(),
            ErrorCode::Failure
        );
        assert!(poll_once(receiver.next_operation()).await.is_none());
    });
    assert_eq!(registry.list().len(), 1);
}

#[test]
fn light_remaining_time_start_and_stop_change_versions_before_run() {
    for cluster in [8, 0x300] {
        setup_light!(false, device, receiver, registry, handler, ctx);
        device.set_availability(Availability::Available);
        let function = device.functions()[0].clone();
        function
            .observe_light(LightPatch {
                power: Some(true),
                brightness_percent: Some(1.0),
                temperature_kelvin: Some(5000),
                ..Default::default()
            })
            .unwrap();
        block_on(async {
            let remaining_attribute = if cluster == 8 { 1 } else { 2 };
            ctx.attribute = attr(cluster, remaining_attribute);
            let before = report_dataver(&read_bytes(&handler, &ctx).await.unwrap());
            let mut command = Context::new(ctx.base, cluster, 0);
            command.command.cmd_id = if cluster == 8 { 0 } else { 0xa };
            command.data = TLVElement::new(if cluster == 8 {
                &[
                    0x15, 0x24, 0, 254, 0x25, 1, 20, 0, 0x24, 2, 0, 0x24, 3, 0, 0x18,
                ]
            } else {
                &[
                    0x15, 0x25, 0, 44, 1, 0x25, 1, 20, 0, 0x24, 2, 0, 0x24, 3, 0, 0x18,
                ]
            });
            let mut output = [0; 128];
            let mut invoke = pin!(handler.invoke(
                &command,
                InvokeReplyInstance::new(&command.command, WriteBuf::new(&mut output))
            ));
            assert!(poll_once(invoke.as_mut()).await.is_none());
            ctx.attribute.dataver = Some(before);
            let started = read_bytes(&handler, &ctx).await.unwrap();
            assert!(!started.is_empty(), "start RemainingTime was filtered out");
            assert_eq!(report_value(&started).u16().unwrap(), 20);
            let started_version = report_dataver(&started);
            assert_ne!(started_version, before);

            let DeviceRequest::Light(first) = receiver.next_operation().await.unwrap() else {
                panic!("missing light startup")
            };
            first.mark_sent().unwrap();
            first.accept().unwrap();
            invoke.await.unwrap();
            let mut stop = Context::new(ctx.base, cluster, 0);
            stop.command.cmd_id = if cluster == 8 { 3 } else { 0x47 };
            stop.data = TLVElement::new(&[0x15, 0x24, 0, 0, 0x24, 1, 0, 0x18]);
            handler
                .invoke(
                    &stop,
                    InvokeReplyInstance::new(&stop.command, WriteBuf::new(&mut [0; 128])),
                )
                .await
                .unwrap();
            ctx.attribute.dataver = Some(started_version);
            let stopped = read_bytes(&handler, &ctx).await.unwrap();
            assert!(!stopped.is_empty(), "Stop RemainingTime was filtered out");
            assert_eq!(report_value(&stopped).u16().unwrap(), 0);
            assert_ne!(report_dataver(&stopped), started_version);
            assert!(ctx.changes.borrow().is_empty());
            let mut run = pin!(handler.run(&ctx));
            assert!(poll_once(run.as_mut()).await.is_none());
            assert!(
                ctx.changes
                    .borrow()
                    .contains(&(2, cluster, remaining_attribute))
            );
            assert!(poll_once(receiver.next_operation()).await.is_none());
        });
        assert_eq!(registry.list().len(), 1);
    }
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
                let raw = binding
                    .brightness
                    .as_ref()
                    .unwrap()
                    .brightness_to_raw(percent)
                    .unwrap();
                let actual = binding
                    .brightness
                    .as_ref()
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
                    crate::device::brightness_to_matter_level(actual).unwrap()
                );
                if binding.brightness.as_ref().unwrap().range.max == 65535 {
                    assert!(actual.fract() > 0.0);
                    assert!((actual - percent).abs() < 0.002);
                }
            }
            ctx.attribute = attr(0x300, 0x400b);
            assert_eq!(
                report_value(&read_bytes(&handler, &ctx).await.unwrap())
                    .u16()
                    .unwrap(),
                binding
                    .temperature
                    .as_ref()
                    .unwrap()
                    .range
                    .mired_bounds()
                    .unwrap()
                    .0
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

#[test]
fn real_rgb_dispatch_accepts_hs_commands_and_reports_hs_attributes() {
    use crate::device::{LightColorTarget, LightMode};

    setup_light!(true, device, receiver, registry, handler, ctx);
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
                let Some(LightColorTarget::Rgb(rgb)) = request.target.color else {
                    panic!("HS command did not dispatch RGB")
                };
                assert!(request.final_frame);
                assert_eq!(request.target.power, None);
                assert_eq!(request.target.brightness_percent, None);
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
    assert_eq!(registry.list().len(), 1);
}
