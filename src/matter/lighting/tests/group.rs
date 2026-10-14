//! Group protocol-entry coverage through the real Matter lighting handlers.
//!
//! These drive `Lighting::new` with the actual `OnOff`/`Level`/`Color`
//! handler implementations and fake physical receivers. They pin the group
//! protocol boundary: endpoint settings interpreted once, Toggle resolved
//! from aggregate power, command parsing and routing, unknown reporting,
//! and achromatic state-only startup.

use super::*;
use crate::{
    device::{
        Capability, Category, CommandReceiver, DeviceKey, FunctionDescription, Group, Light,
        LightColorTarget, NumericRange, PhysicalDevice, PhysicalDeviceDescription, Rgb,
    },
    matter::lighting::OnOffHandler,
};
use rs_matter::{
    dm::clusters::decl::on_off::ClusterAsyncHandler as _, error::ErrorCode, tlv::Nullable,
};

fn full() -> Vec<Capability> {
    vec![
        Capability::OnOff,
        Capability::Brightness,
        Capability::ColorTemperature(NumericRange {
            min: 2700,
            max: 6500,
            step: 1,
        }),
        Capability::ColorRgb,
    ]
}

fn dimmable() -> Vec<Capability> {
    vec![Capability::OnOff, Capability::Brightness]
}

fn rgb_light() -> Vec<Capability> {
    vec![Capability::OnOff, Capability::ColorRgb]
}

fn ct_light(min: u32, max: u32, step: u32) -> Vec<Capability> {
    vec![
        Capability::OnOff,
        Capability::ColorTemperature(NumericRange { min, max, step }),
    ]
}

macro_rules! group_case {
    ($caps:expr, $count:expr, $devices:ident, $members:ident, $receivers:ident, $group:ident, $light:ident, $executor:ident, $clock:ident, $ctx:ident, $body:block) => {{
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let identity = store.load_identity().unwrap();
        let mut $devices: Vec<PhysicalDevice> = Vec::new();
        let mut $members: Vec<Function> = Vec::new();
        let mut $receivers: Vec<CommandReceiver> = Vec::new();
        for (index, capabilities) in $caps.into_iter().enumerate() {
            let description = PhysicalDeviceDescription {
                key: DeviceKey::new("xiaomi", format!("member-{index}")),
                label: format!("Member {index}"),
                functions: vec![FunctionDescription {
                    id: "main".into(),
                    label: "Main".into(),
                    category: Category::Light,
                    capabilities,
                }],
            };
            let (device, mut receivers) = PhysicalDevice::new(description);
            device.set_availability(Availability::Available);
            $members.push(device.functions()[0].clone());
            $receivers.push(receivers.remove(0));
            $devices.push(device);
        }
        let $group = Group::new(
            DeviceKey::new("xiaomi", "virtual-group"),
            "Virtual Group",
            $members[..$count].to_vec(),
        )
        .unwrap();
        let registry = crate::matter::tests::published_registry(&store, &$devices[0]);
        let handler = model::handler(
            &registry,
            &store,
            StoreAdapter::new(store.matter()),
            support::test_executor(),
            rand::rng(),
            None,
        )
        .unwrap();
        let info = model::basic_info(&identity);
        let matter = Matter::new(&info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
        let buffers: MatterBuffers = MatterBuffers::new();
        let state: EthInteractionModelState =
            EthInteractionModelState::new(EthNetwork::new_default());
        let crypto = default_crypto(rand::rng(), DAC_PRIVKEY);
        let kv = matter.kv(StoreAdapter::new(store.matter()));
        let im = InteractionModel::new(&matter, &crypto, &buffers, &handler, &kv, &state);
        let $ctx = Context::new(&im, 8, 0);
        let $clock = Rc::new(TestClock::default());
        let mapping =
            crate::matter::publication::map_capabilities(&$group.description().capabilities)
                .unwrap();
        let $executor = LightingExecutor::new($clock.clone());
        let $light = Lighting::new(
            Light::Group($group.clone()),
            2,
            Dataver::new(1),
            Dataver::new(2),
            Dataver::new(3),
            mapping,
            $executor.clone(),
        );
        $body
    }};
}

fn observe(function: &Function, patch: LightPatch) {
    function.observe_light(patch).unwrap();
}

fn observe_on(function: &Function, brightness: Option<f64>) {
    observe(
        function,
        LightPatch {
            power: Some(true),
            brightness_percent: brightness,
            ..Default::default()
        },
    );
}

fn group_reported<H: rs_matter::dm::HandlerContext>(ctx: &Context<'_, H>) -> Vec<u32> {
    let mut attributes: Vec<_> = ctx
        .changes
        .borrow_mut()
        .drain(..)
        .filter_map(|(endpoint, cluster, attribute)| {
            (endpoint == 2 && cluster == 0x300).then_some(attribute)
        })
        .collect();
    attributes.sort_unstable();
    attributes
}

fn level_bytes(level: u8, transition: u16, mask: u8, override_: u8) -> Vec<u8> {
    let time = transition.to_le_bytes();
    vec![
        0x15, 0x24, 0, level, 0x25, 1, time[0], time[1], 0x24, 2, mask, 0x24, 3, override_, 0x18,
    ]
}

fn move_level_bytes(mode: u8, rate: u8) -> Vec<u8> {
    vec![
        0x15, 0x24, 0, mode, 0x24, 1, rate, 0x24, 2, 0, 0x24, 3, 0, 0x18,
    ]
}

fn step_level_bytes(mode: u8, size: u8, transition: u16) -> Vec<u8> {
    let time = transition.to_le_bytes();
    vec![
        0x15, 0x24, 0, mode, 0x24, 1, size, 0x25, 2, time[0], time[1], 0x24, 3, 0, 0x24, 4, 0, 0x18,
    ]
}

fn hs_to_bytes(hue: u8, saturation: u8, transition: u16) -> Vec<u8> {
    let time = transition.to_le_bytes();
    vec![
        0x15, 0x24, 0, hue, 0x24, 1, saturation, 0x25, 2, time[0], time[1], 0x24, 3, 0, 0x24, 4, 0,
        0x18,
    ]
}

fn hue_to_bytes(hue: u8, direction: u8, transition: u16) -> Vec<u8> {
    let time = transition.to_le_bytes();
    vec![
        0x15, 0x24, 0, hue, 0x24, 1, direction, 0x25, 2, time[0], time[1], 0x24, 3, 0, 0x24, 4, 0,
        0x18,
    ]
}

fn saturation_to_bytes(saturation: u8, transition: u16) -> Vec<u8> {
    let time = transition.to_le_bytes();
    vec![
        0x15, 0x24, 0, saturation, 0x25, 1, time[0], time[1], 0x24, 2, 0, 0x24, 3, 0, 0x18,
    ]
}

fn axis_move_bytes(mode: u8, rate: u8) -> Vec<u8> {
    vec![
        0x15, 0x24, 0, mode, 0x24, 1, rate, 0x24, 2, 0, 0x24, 3, 0, 0x18,
    ]
}

fn axis_step_bytes(mode: u8, size: u8, transition: u8) -> Vec<u8> {
    vec![
        0x15, 0x24, 0, mode, 0x24, 1, size, 0x24, 2, transition, 0x24, 3, 0, 0x24, 4, 0, 0x18,
    ]
}

fn xy_to_bytes(x: u16, y: u16, transition: u16) -> Vec<u8> {
    let (x, y, time) = (x.to_le_bytes(), y.to_le_bytes(), transition.to_le_bytes());
    vec![
        0x15, 0x25, 0, x[0], x[1], 0x25, 1, y[0], y[1], 0x25, 2, time[0], time[1], 0x24, 3, 0,
        0x24, 4, 0, 0x18,
    ]
}

fn axis_xy_bytes(rate_x: i16, rate_y: i16) -> Vec<u8> {
    let (x, y) = (rate_x.to_le_bytes(), rate_y.to_le_bytes());
    vec![
        0x15, 0x21, 0, x[0], x[1], 0x21, 1, y[0], y[1], 0x24, 2, 0, 0x24, 3, 0, 0x18,
    ]
}

fn step_xy_bytes(step_x: i16, step_y: i16, transition: u16) -> Vec<u8> {
    let (x, y, time) = (
        step_x.to_le_bytes(),
        step_y.to_le_bytes(),
        transition.to_le_bytes(),
    );
    vec![
        0x15, 0x21, 0, x[0], x[1], 0x21, 1, y[0], y[1], 0x25, 2, time[0], time[1], 0x24, 3, 0,
        0x24, 4, 0, 0x18,
    ]
}

fn ct_bytes(mireds: u16, transition: u16) -> Vec<u8> {
    let (mireds, time) = (mireds.to_le_bytes(), transition.to_le_bytes());
    vec![
        0x15, 0x25, 0, mireds[0], mireds[1], 0x25, 1, time[0], time[1], 0x24, 2, 0, 0x24, 3, 0,
        0x18,
    ]
}

macro_rules! run_color_invoke {
    ($members:expr, $receivers:expr, $invoke:expr) => {{
        let mut invoke = pin!($invoke);
        let first = poll_once(invoke.as_mut()).await;
        assert!(first.is_none(), "first poll completed: {first:?}");
        for (function, receiver) in $members.iter().zip($receivers.iter_mut()) {
            let request = frame(receiver).await;
            assert!(matches!(
                request.target.color,
                Some(LightColorTarget::Rgb(_))
            ));
            accept(function, request);
        }
        invoke.await.unwrap();
    }};
}

#[test]
fn group_level_move_and_step_parse_and_route_to_members() {
    use l::ClusterAsyncHandler as _;
    group_case!(
        vec![dimmable(), dimmable()],
        2,
        _devices,
        members,
        receivers,
        _group,
        light,
        _executor,
        _clock,
        ctx,
        {
            observe_on(&members[0], Some(20.0));
            observe_on(&members[1], Some(80.0));
            block_on(async {
                let handler = LevelHandler(&light);
                let move_bytes = move_level_bytes(0, 50);
                let mut invoke = pin!(
                    handler.handle_move(&ctx, l::MoveRequest::new(TLVElement::new(&move_bytes)))
                );
                assert!(poll_once(invoke.as_mut()).await.is_none());
                let a = frame(&mut receivers[0]).await;
                let b = frame(&mut receivers[1]).await;
                assert!(a.target.brightness_percent.is_some());
                assert!(b.target.brightness_percent.is_some());
                accept(&members[0], a);
                accept(&members[1], b);
                invoke.await.unwrap();

                let step_bytes = step_level_bytes(0, 20, 10);
                let mut invoke = pin!(
                    handler.handle_step(&ctx, l::StepRequest::new(TLVElement::new(&step_bytes)))
                );
                assert!(poll_once(invoke.as_mut()).await.is_none());
                let a = frame(&mut receivers[0]).await;
                let b = frame(&mut receivers[1]).await;
                assert!(a.target.brightness_percent.is_some());
                assert!(b.target.brightness_percent.is_some());
                accept(&members[0], a);
                accept(&members[1], b);
                invoke.await.unwrap();
            });
        }
    );
}

#[test]
fn group_on_uses_the_group_on_level_not_member_endpoint_settings() {
    use l::ClusterAsyncHandler as _;
    group_case!(
        vec![full(), full()],
        2,
        _devices,
        members,
        receivers,
        _group,
        light,
        executor,
        _clock,
        ctx,
        {
            for function in &members {
                observe(function, LightPatch::default());
            }
            // A member endpoint with a conflicting OnLevel must not be used.
            let member = Lighting::new(
                Light::Single(members[0].clone()),
                2,
                Dataver::new(1),
                Dataver::new(2),
                Dataver::new(3),
                crate::matter::publication::map_light(members[0].description()).unwrap(),
                executor.clone(),
            );
            block_on(async {
                LevelHandler(&member)
                    .set_on_level(&ctx, Nullable::some(50))
                    .await
                    .unwrap();
                LevelHandler(&light)
                    .set_on_level(&ctx, Nullable::some(200))
                    .await
                    .unwrap();
                let handler = OnOffHandler(&light);
                let mut invoke = pin!(handler.handle_on(&ctx));
                assert!(poll_once(invoke.as_mut()).await.is_none());
                let a = frame(&mut receivers[0]).await;
                let b = frame(&mut receivers[1]).await;
                assert_eq!(a.target.power, Some(true));
                assert_eq!(b.target.power, Some(true));
                assert!(a.target.brightness_percent.unwrap() > 78.0);
                assert!(b.target.brightness_percent.unwrap() > 78.0);
                accept(&members[0], a);
                accept(&members[1], b);
                invoke.await.unwrap();
            });
        }
    );
}

#[test]
fn group_toggle_resolves_aggregate_power_and_unknown_fails() {
    group_case!(
        vec![full(), full()],
        2,
        devices,
        members,
        receivers,
        _group,
        light,
        _executor,
        _clock,
        ctx,
        {
            observe(
                &members[0],
                LightPatch {
                    power: Some(true),
                    ..Default::default()
                },
            );
            observe(
                &members[1],
                LightPatch {
                    power: Some(false),
                    ..Default::default()
                },
            );
            block_on(async {
                let handler = OnOffHandler(&light);
                let mut invoke = pin!(handler.handle_toggle(&ctx));
                assert!(poll_once(invoke.as_mut()).await.is_none());
                let a = match receivers[0].next_operation().await.unwrap() {
                    DeviceRequest::Power(request) => request,
                    DeviceRequest::Light(_) => panic!("expected power"),
                };
                let b = match receivers[1].next_operation().await.unwrap() {
                    DeviceRequest::Power(request) => request,
                    DeviceRequest::Light(_) => panic!("expected power"),
                };
                // Aggregate is On, so Toggle resolves one definite Off target
                // for both members instead of inverting each one.
                assert!(!a.target);
                assert!(!b.target);
                a.accept().unwrap();
                b.accept().unwrap();
                invoke.await.unwrap();
            });
            for device in &devices {
                device.set_availability(Availability::Unavailable("offline".into()));
                device.set_availability(Availability::Available);
            }
            block_on(async {
                let result = OnOffHandler(&light).handle_toggle(&ctx).await;
                assert_eq!(result.unwrap_err().code(), ErrorCode::InvalidState);
                assert!(poll_once(receivers[0].next_operation()).await.is_none());
                assert!(poll_once(receivers[1].next_operation()).await.is_none());
            });
        }
    );
}

#[test]
fn group_execute_if_off_returns_protocol_success_without_member_writes() {
    use l::ClusterAsyncHandler as _;
    group_case!(
        vec![dimmable(), dimmable()],
        2,
        _devices,
        members,
        receivers,
        _group,
        light,
        _executor,
        _clock,
        ctx,
        {
            for function in &members {
                observe(
                    function,
                    LightPatch {
                        power: Some(false),
                        brightness_percent: Some(50.0),
                        ..Default::default()
                    },
                );
            }
            block_on(async {
                let bytes = level_bytes(128, 10, 0, 0);
                LevelHandler(&light)
                    .handle_move_to_level(&ctx, l::MoveToLevelRequest::new(TLVElement::new(&bytes)))
                    .await
                    .unwrap();
                assert!(poll_once(receivers[0].next_operation()).await.is_none());
                assert!(poll_once(receivers[1].next_operation()).await.is_none());

                // The group endpoint Options interpret ExecuteIfOff once and
                // enable both members without touching member settings.
                LevelHandler(&light)
                    .set_options(&ctx, l::OptionsBitmap::from_bits_retain(1))
                    .await
                    .unwrap();
                let handler = LevelHandler(&light);
                let mut invoke = pin!(handler.handle_move_to_level(
                    &ctx,
                    l::MoveToLevelRequest::new(TLVElement::new(&bytes))
                ));
                assert!(poll_once(invoke.as_mut()).await.is_none());
                let a = frame(&mut receivers[0]).await;
                let b = frame(&mut receivers[1]).await;
                accept(&members[0], a);
                accept(&members[1], b);
                invoke.await.unwrap();
            });
        }
    );
}

#[test]
fn group_hue_and_saturation_commands_cover_each_axis() {
    use c::ClusterAsyncHandler as _;
    group_case!(
        vec![rgb_light(), rgb_light()],
        2,
        _devices,
        members,
        receivers,
        _group,
        light,
        _executor,
        _clock,
        ctx,
        {
            for (function, color) in [
                (
                    &members[0],
                    Rgb {
                        red: 255,
                        green: 0,
                        blue: 0,
                    },
                ),
                (
                    &members[1],
                    Rgb {
                        red: 0,
                        green: 0,
                        blue: 255,
                    },
                ),
            ] {
                observe(
                    function,
                    LightPatch {
                        color: Some(color),
                        mode: Some(LightMode::Rgb),
                        ..Default::default()
                    },
                );
            }
            block_on(async {
                // Group power stays unknown: ExecuteIfOff is interpreted once
                // at the entry and every member resolves its own HS start.
                let handler = ColorHandler::<false, true>(&light);
                handler
                    .set_options(&ctx, c::OptionsBitmap::from_bits_retain(1))
                    .await
                    .unwrap();
                let bytes = hue_to_bytes(120, 0, 10);
                run_color_invoke!(
                    members,
                    receivers,
                    handler.handle_move_to_hue(
                        &ctx,
                        c::MoveToHueRequest::new(TLVElement::new(&bytes))
                    )
                );
                let bytes = axis_step_bytes(1, 10, 10);
                run_color_invoke!(
                    members,
                    receivers,
                    handler.handle_step_hue(&ctx, c::StepHueRequest::new(TLVElement::new(&bytes)))
                );
                let bytes = axis_move_bytes(1, 20);
                run_color_invoke!(
                    members,
                    receivers,
                    handler.handle_move_hue(&ctx, c::MoveHueRequest::new(TLVElement::new(&bytes)))
                );
                let bytes = saturation_to_bytes(200, 10);
                run_color_invoke!(
                    members,
                    receivers,
                    handler.handle_move_to_saturation(
                        &ctx,
                        c::MoveToSaturationRequest::new(TLVElement::new(&bytes))
                    )
                );
                let bytes = axis_step_bytes(1, 10, 10);
                run_color_invoke!(
                    members,
                    receivers,
                    handler.handle_step_saturation(
                        &ctx,
                        c::StepSaturationRequest::new(TLVElement::new(&bytes))
                    )
                );
                let bytes = axis_move_bytes(1, 20);
                run_color_invoke!(
                    members,
                    receivers,
                    handler.handle_move_saturation(
                        &ctx,
                        c::MoveSaturationRequest::new(TLVElement::new(&bytes))
                    )
                );
                let bytes = hs_to_bytes(120, 200, 10);
                run_color_invoke!(
                    members,
                    receivers,
                    handler.handle_move_to_hue_and_saturation(
                        &ctx,
                        c::MoveToHueAndSaturationRequest::new(TLVElement::new(&bytes))
                    )
                );
                // Stop is a protocol no-op for members with no HS action.
                handler
                    .handle_stop_move_step(
                        &ctx,
                        c::StopMoveStepRequest::new(TLVElement::new(&[
                            0x15, 0x24, 0, 0, 0x24, 1, 0, 0x18,
                        ])),
                    )
                    .await
                    .unwrap();
                assert!(poll_once(receivers[0].next_operation()).await.is_none());
                assert!(poll_once(receivers[1].next_operation()).await.is_none());
            });
        }
    );
}

#[test]
fn group_xy_commands_cover_target_move_step_and_stop() {
    use c::ClusterAsyncHandler as _;
    group_case!(
        vec![rgb_light(), rgb_light()],
        2,
        _devices,
        members,
        receivers,
        _group,
        light,
        _executor,
        _clock,
        ctx,
        {
            for (function, color) in [
                (
                    &members[0],
                    Rgb {
                        red: 255,
                        green: 0,
                        blue: 0,
                    },
                ),
                (
                    &members[1],
                    Rgb {
                        red: 0,
                        green: 0,
                        blue: 255,
                    },
                ),
            ] {
                observe(
                    function,
                    LightPatch {
                        color: Some(color),
                        mode: Some(LightMode::Rgb),
                        ..Default::default()
                    },
                );
            }
            block_on(async {
                let handler = ColorHandler::<false, true>(&light);
                handler
                    .set_options(&ctx, c::OptionsBitmap::from_bits_retain(1))
                    .await
                    .unwrap();
                let bytes = xy_to_bytes(30000, 30000, 10);
                run_color_invoke!(
                    members,
                    receivers,
                    handler.handle_move_to_color(
                        &ctx,
                        c::MoveToColorRequest::new(TLVElement::new(&bytes))
                    )
                );
                let bytes = axis_xy_bytes(100, 100);
                run_color_invoke!(
                    members,
                    receivers,
                    handler
                        .handle_move_color(&ctx, c::MoveColorRequest::new(TLVElement::new(&bytes)))
                );
                let bytes = step_xy_bytes(100, 100, 10);
                run_color_invoke!(
                    members,
                    receivers,
                    handler
                        .handle_step_color(&ctx, c::StepColorRequest::new(TLVElement::new(&bytes)))
                );
                handler
                    .handle_stop_move_step(
                        &ctx,
                        c::StopMoveStepRequest::new(TLVElement::new(&[
                            0x15, 0x24, 0, 0, 0x24, 1, 0, 0x18,
                        ])),
                    )
                    .await
                    .unwrap();
                assert!(poll_once(receivers[0].next_operation()).await.is_none());
                assert!(poll_once(receivers[1].next_operation()).await.is_none());
            });
        }
    );
}

#[test]
fn group_ct_request_clamps_to_common_bounds_at_the_entry() {
    use c::ClusterAsyncHandler as _;
    group_case!(
        vec![ct_light(2700, 6500, 100), ct_light(3000, 6000, 100)],
        2,
        _devices,
        members,
        receivers,
        _group,
        light,
        _executor,
        _clock,
        ctx,
        {
            for function in &members {
                observe(
                    function,
                    LightPatch {
                        power: Some(true),
                        temperature_kelvin: Some(4000),
                        mode: Some(LightMode::Temperature),
                        ..Default::default()
                    },
                );
            }
            block_on(async {
                // 154 mireds is 6500K, outside the 3000..6000K common range.
                let handler = ColorHandler::<true, false>(&light);
                let bytes = ct_bytes(154, 0);
                let mut invoke = pin!(handler.handle_move_to_color_temperature(
                    &ctx,
                    c::MoveToColorTemperatureRequest::new(TLVElement::new(&bytes))
                ));
                assert!(poll_once(invoke.as_mut()).await.is_none());
                for (function, receiver) in members.iter().zip(receivers.iter_mut()) {
                    let request = frame(receiver).await;
                    assert!(matches!(
                        request.target.color,
                        Some(LightColorTarget::Temperature(6000))
                    ));
                    accept(function, request);
                }
                invoke.await.unwrap();
            });
        }
    );
}

#[test]
fn group_known_to_unknown_ct_dirties_the_attribute_and_reads_fail() {
    use c::ClusterAsyncHandler as _;
    group_case!(
        vec![ct_light(2700, 6500, 100), ct_light(3000, 6000, 100)],
        2,
        devices,
        members,
        _receivers,
        _group,
        light,
        _executor,
        _clock,
        ctx,
        {
            for function in &members {
                observe(
                    function,
                    LightPatch {
                        power: Some(true),
                        temperature_kelvin: Some(4000),
                        mode: Some(LightMode::Temperature),
                        ..Default::default()
                    },
                );
            }
            light.reports(&ctx);
            let _ = group_reported(&ctx);
            block_on(async {
                assert!(
                    ColorHandler::<true, false>(&light)
                        .color_temperature_mireds(&ctx)
                        .await
                        .is_ok()
                );
            });
            for device in &devices {
                device.set_availability(Availability::Unavailable("lost".into()));
            }
            light.reports(&ctx);
            let attributes = group_reported(&ctx);
            assert!(attributes.contains(&7));
            assert!(attributes.contains(&8));
            assert!(attributes.contains(&0x4001));
            assert_eq!(light.refresh().temperature, None);
            block_on(async {
                assert!(
                    ColorHandler::<true, false>(&light)
                        .color_temperature_mireds(&ctx)
                        .await
                        .is_err()
                );
            });
        }
    );
}

#[test]
fn group_known_to_unknown_rgb_dirties_the_attributes_and_reads_fail() {
    use c::ClusterAsyncHandler as _;
    group_case!(
        vec![rgb_light(), rgb_light()],
        2,
        devices,
        members,
        _receivers,
        _group,
        light,
        _executor,
        _clock,
        ctx,
        {
            for function in &members {
                observe(
                    function,
                    LightPatch {
                        power: Some(true),
                        color: Some(Rgb {
                            red: 255,
                            green: 0,
                            blue: 0,
                        }),
                        mode: Some(LightMode::Rgb),
                        ..Default::default()
                    },
                );
            }
            light.reports(&ctx);
            let _ = group_reported(&ctx);
            for device in &devices {
                device.set_availability(Availability::Unavailable("lost".into()));
            }
            light.reports(&ctx);
            let attributes = group_reported(&ctx);
            for attribute in [0, 1, 3, 4, 8, 0x4001] {
                assert!(attributes.contains(&attribute), "missing {attribute}");
            }
            assert_eq!(light.refresh().xy, None);
            block_on(async {
                assert!(
                    ColorHandler::<false, true>(&light)
                        .current_x(&ctx)
                        .await
                        .is_err()
                );
            });
        }
    );
}

#[test]
fn group_achromatic_hs_accepts_state_only_and_retains_hue_intent() {
    use c::ClusterAsyncHandler as _;
    group_case!(
        vec![rgb_light(), rgb_light()],
        2,
        _devices,
        members,
        receivers,
        _group,
        light,
        executor,
        _clock,
        ctx,
        {
            for function in &members {
                observe(
                    function,
                    LightPatch {
                        color: Some(Rgb {
                            red: 255,
                            green: 255,
                            blue: 255,
                        }),
                        mode: Some(LightMode::Rgb),
                        ..Default::default()
                    },
                );
            }
            block_on(async {
                let handler = ColorHandler::<false, true>(&light);
                handler
                    .set_options(&ctx, c::OptionsBitmap::from_bits_retain(1))
                    .await
                    .unwrap();
                let bytes = hs_to_bytes(120, 0, 10);
                let invoke = pin!(handler.handle_move_to_hue_and_saturation(
                    &ctx,
                    c::MoveToHueAndSaturationRequest::new(TLVElement::new(&bytes))
                ));
                // Like the single-light entry, acceptance is local; the hue
                // trajectory stays committed and progresses afterwards.
                invoke.await.unwrap();
                for function in &members {
                    assert_eq!(
                        executor.remaining_ms(function.function_key(), LightDimension::Color),
                        Some(1000)
                    );
                }
                for (function, receiver) in members.iter().zip(receivers.iter_mut()) {
                    let request = frame(receiver).await;
                    assert!(matches!(
                        request.target.color,
                        Some(LightColorTarget::Rgb(Rgb {
                            red: 255,
                            green: 255,
                            blue: 255
                        }))
                    ));
                    accept(function, request);
                }
            });
        }
    );
}

#[test]
fn shared_group_remaining_reports_follow_external_start_takeover_completion_and_stop() {
    use crate::lighting::{
        group::Fanout,
        planning::{ColorPlan, LevelPlan},
    };

    async fn start(
        light: &Lighting,
        members: &[Function],
        receivers: &mut [CommandReceiver],
        dimension: LightDimension,
        transition_ms: u64,
    ) {
        let current = members[0].snapshot().lighting.unwrap();
        let command = match dimension {
            LightDimension::Brightness => Fanout::Level(LevelPlan::MoveTo {
                target: if current.brightness_percent.unwrap() > 60.0 {
                    100
                } else {
                    200
                },
                transition_ms,
                with_on_off: false,
            }),
            LightDimension::Color => Fanout::Color(ColorPlan::TemperatureMoveTo {
                target_mireds: if current.temperature_kelvin.unwrap() < 4000 {
                    200
                } else {
                    300
                },
                transition_ms,
            }),
        };
        let mut invoke = pin!(light.execute_group(light.group().unwrap(), command, None));
        assert!(poll_once(invoke.as_mut()).await.is_none());
        settle(members, receivers).await;
        invoke.await.unwrap();
    }

    async fn settle(members: &[Function], receivers: &mut [CommandReceiver]) {
        for (function, receiver) in members.iter().zip(receivers.iter_mut()) {
            accept(function, frame(receiver).await);
        }
    }

    group_case!(
        vec![full(), full(), full()],
        2,
        _devices,
        members,
        receivers,
        _group,
        a,
        executor,
        clock,
        ctx,
        {
            for member in &members {
                observe(
                    member,
                    LightPatch {
                        power: Some(true),
                        brightness_percent: Some(50.0),
                        temperature_kelvin: Some(4000),
                        mode: Some(LightMode::Temperature),
                        ..Default::default()
                    },
                );
            }
            let group_b = Group::new(
                DeviceKey::new("xiaomi", "group-b"),
                "B",
                members[1..].to_vec(),
            )
            .unwrap();
            let b = Lighting::new(
                Light::Group(group_b),
                3,
                Dataver::new(1),
                Dataver::new(2),
                Dataver::new(3),
                a.mapping,
                executor.clone(),
            );
            for dimension in [LightDimension::Brightness, LightDimension::Color] {
                let attribute = match dimension {
                    LightDimension::Brightness => (2, 8, 1),
                    LightDimension::Color => (2, 0x300, 2),
                };
                let check = |value, report| {
                    a.progress();
                    a.reports(&ctx);
                    let remaining = match dimension {
                        LightDimension::Brightness => a.level_state().remaining_time.value,
                        LightDimension::Color => a.color_state().remaining_time.value,
                    };
                    assert_eq!(remaining, value, "{dimension:?} at {}", clock.now());
                    assert_eq!(ctx.changes.borrow().contains(&attribute), report);
                    ctx.changes.borrow_mut().clear();
                };
                let base = clock.now();
                block_on(async {
                    // A short transition stays quiet through completion.
                    start(&a, &members[..2], &mut receivers[..2], dimension, 500).await;
                    check(5, false);
                    clock.set(base + 500);
                    executor.advance();
                    settle(&members[..2], &mut receivers[..2]).await;
                    check(0, false);

                    // B shares only one member with A; its long action must reset
                    // A's old short-transition suppression and report both edges.
                    start(&b, &members[1..], &mut receivers[1..], dimension, 3000).await;
                    check(30, true);
                    clock.set(base + 3000);
                    check(5, false);
                    clock.set(base + 3500);
                    executor.advance();
                    settle(&members[1..], &mut receivers[1..]).await;
                    check(0, true);

                    start(&b, &members[1..], &mut receivers[1..], dimension, 2000).await;
                    check(20, true);
                    clock.set(base + 4000);
                    check(15, false);
                    start(&b, &members[1..], &mut receivers[1..], dimension, 5000).await;
                    check(50, true);
                    clock.set(base + 8500);
                    check(5, false);
                    // Stop from A ends its shared action and still reports the
                    // long action's end when its remaining time is below 1s.
                    a.stop(dimension).await.unwrap();
                    check(0, true);
                    assert_eq!(
                        executor.remaining_ms(members[2].function_key(), dimension),
                        Some(500)
                    );
                    b.stop(dimension).await.unwrap();
                });
            }
        }
    );
}
