//! Virtual light group publication through the real Matter model.
//!
//! Groups are published from the registry's **applied** configuration as one
//! ordinary light endpoint that shares the registry's stable `Group` handle.
//! Grouped members are hidden (no endpoint, no TagList children); leaving a
//! group restores the member's own reserved endpoint and saved name.

use super::*;

use crate::{
    device::{
        Availability, Capability, Category, CommandReceiver, Composition, DeviceKey,
        DeviceRegistry, DeviceRequest, FunctionDescription, NumericRange, PhysicalDevice,
    },
    storage::{CachedPhysicalDevice, CachedScope, Store},
    xiaomi::{
        capability::{
            BrightnessConversion, LightBindings, ModeBinding, NumericBinding, NumericFormat,
            NumericUnit, ParsedDevice, ParsedFunction, PropertyBinding, parse_device_for_model,
        },
        cloud::group_id,
    },
};
use futures_lite::future::poll_once;
use rs_matter::{
    MATTER_PORT, Matter,
    crypto::default_crypto,
    dm::{
        AsyncHandler, HandlerContext, InvokeReplyInstance,
        devices::test::{DAC_PRIVKEY, TEST_DEV_ATT, TEST_DEV_COMM},
        networks::eth::EthNetwork,
    },
    im::{EthInteractionModelState, InteractionModel},
    tlv::TLVElement,
    transport::exchange::MatterBuffers,
    utils::storage::WriteBuf,
};
use std::pin::pin;

use super::handlers::Context;

pub(in crate::matter) fn cached_light(uid: &str, did: &str, model: &str) -> CachedPhysicalDevice {
    let source = match model {
        "yeelink.light.light3" => {
            include_str!("../../../tests/fixtures/miot/yeelink-light-light3.json")
        }
        "devcea.light.ls2307" => {
            include_str!("../../../tests/fixtures/miot/devcea-light-ls2307.json")
        }
        _ => panic!("unsupported test light model {model}"),
    };
    let instance: serde_json::Value = serde_json::from_str(source).unwrap();
    CachedPhysicalDevice {
        uid: uid.into(),
        home_id: "local".into(),
        group_id: group_id(uid, "local"),
        model: model.into(),
        instance_type: instance["type"].as_str().unwrap().into(),
        device: parse_device_for_model(DeviceKey::new("xiaomi", did), model, &instance).unwrap(),
    }
}

/// A synthetic cached light with exactly the requested capabilities and
/// matching vendor bindings, so storage admission, derived group capabilities
/// and the Matter mapping all see the same shape.
fn synthetic_cached(did: &str, capabilities: &[Capability]) -> CachedPhysicalDevice {
    let has = |wanted: &Capability| capabilities.contains(wanted);
    let on_off = has(&Capability::OnOff);
    let brightness = has(&Capability::Brightness);
    let temperature = capabilities.iter().find_map(|capability| match capability {
        Capability::ColorTemperature(range) => Some(*range),
        _ => None,
    });
    let rgb = has(&Capability::ColorRgb);
    let mut next = 1u32;
    let mut property = |siid: u32| {
        next += 1;
        PropertyBinding { siid, piid: next }
    };
    let power = on_off.then(|| property(2));
    let brightness_binding = if brightness {
        let property = property(2);
        Some(NumericBinding {
            property,
            format: NumericFormat::Uint8,
            unit: NumericUnit::Percentage,
            range: NumericRange {
                min: 1,
                max: 100,
                step: 1,
            },
        })
    } else {
        None
    };
    let temperature_binding = temperature.map(|range| NumericBinding {
        property: property(2),
        format: NumericFormat::Uint16,
        unit: NumericUnit::Kelvin,
        range,
    });
    let color_binding = if rgb {
        let property = property(2);
        Some(NumericBinding {
            property,
            format: NumericFormat::Uint32,
            unit: NumericUnit::Rgb,
            range: NumericRange {
                min: 0,
                max: 0xffffff,
                step: 1,
            },
        })
    } else {
        None
    };
    let mode_binding = if temperature.is_some() && rgb {
        Some(ModeBinding {
            property: property(2),
            format: NumericFormat::Uint8,
            temperature_value: 0,
            rgb_value: 1,
        })
    } else {
        None
    };
    let lighting = if brightness || temperature.is_some() || rgb {
        Some(LightBindings {
            brightness: brightness_binding,
            temperature: temperature_binding,
            color: color_binding,
            mode: mode_binding,
            brightness_conversion: brightness.then_some(BrightnessConversion::NonzeroLinear),
        })
    } else {
        None
    };
    CachedPhysicalDevice {
        uid: "uid".into(),
        home_id: "local".into(),
        group_id: group_id("uid", "local"),
        model: "yeelink.light.light3".into(),
        instance_type: "urn:miot-spec-v2:device:light:0000A001:yeelink-light3:1:0000C802".into(),
        device: ParsedDevice {
            key: DeviceKey::new("xiaomi", did),
            functions: vec![ParsedFunction {
                description: FunctionDescription {
                    id: "main".into(),
                    label: did.into(),
                    category: Category::Light,
                    capabilities: capabilities.to_vec(),
                },
                power,
                lighting,
            }],
            refresh_trigger: None,
        },
    }
}

pub(in crate::matter) struct Member {
    pub(in crate::matter) key: DeviceKey,
    pub(in crate::matter) device: PhysicalDevice,
    pub(in crate::matter) receiver: CommandReceiver,
    pub(in crate::matter) endpoint: u16,
}

pub(in crate::matter) struct GroupRig {
    _dir: tempfile::TempDir,
    pub(in crate::matter) store: Store,
    pub(in crate::matter) registry: DeviceRegistry,
    pub(in crate::matter) members: Vec<Member>,
    pub(in crate::matter) parent: u16,
    pub(in crate::matter) label: String,
}

impl GroupRig {
    pub(in crate::matter) fn new(members: &[(&str, &str, &str)]) -> Self {
        let all = members.iter().map(|(did, _, _)| *did).collect::<Vec<_>>();
        Self::build(members, Some(&all), &[])
    }

    /// Published standalone members without any composition relation yet.
    pub(in crate::matter) fn standalone(members: &[(&str, &str, &str)]) -> Self {
        Self::build(members, None, &[])
    }

    fn build(
        members: &[(&str, &str, &str)],
        group: Option<&[&str]>,
        capabilities: &[(&str, Vec<Capability>)],
    ) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let registry = DeviceRegistry::new();
        let mut collected = Vec::new();
        for (did, model, name) in members {
            let cached = match capabilities.iter().find(|(id, _)| id == did) {
                Some((_, capabilities)) => synthetic_cached(did, capabilities),
                None => cached_light("uid", did, model),
            };
            let stored = store.devices().publish_cached(&cached, name).unwrap();
            let description = stored.description();
            let (device, mut receivers) = PhysicalDevice::new(description);
            registry.insert(device.clone());
            collected.push(Member {
                key: device.key().clone(),
                device,
                receiver: receivers.remove(0),
                endpoint: stored.endpoint,
            });
        }
        let mut rig = Self {
            _dir: dir,
            store,
            registry,
            members: collected,
            parent: 0,
            label: "Living room".into(),
        };
        if let Some(group) = group {
            rig.parent = rig.recompose_named(group, "Living room");
        } else {
            // Standalone publication is part of the applied configuration too.
            let devices = rig
                .members
                .iter()
                .map(|member| member.key.clone())
                .collect::<Vec<_>>();
            rig.registry
                .apply_configuration(0, Vec::new(), &devices, &[])
                .unwrap();
        }
        rig
    }

    pub(in crate::matter) fn member(&self, did: &str) -> &Member {
        self.members
            .iter()
            .find(|member| member.key.id == did)
            .unwrap()
    }

    pub(in crate::matter) fn member_mut(&mut self, did: &str) -> &mut Member {
        self.members
            .iter_mut()
            .find(|member| member.key.id == did)
            .unwrap()
    }

    pub(in crate::matter) fn scope(&self) -> CachedScope {
        CachedScope {
            uid: "uid".into(),
            home_id: "local".into(),
            group_id: group_id("uid", "local"),
        }
    }

    fn configuration_scope(&self) -> crate::device::ConfigurationScope {
        let scope = self.scope();
        crate::device::ConfigurationScope {
            uid: scope.uid,
            home_id: scope.home_id,
            group_id: scope.group_id,
        }
    }

    /// Publish one complete relation set directly through storage and the
    /// registry. This is the between-boot baseline path; a running model must
    /// use [`Self::stage`].
    pub(in crate::matter) fn recompose(&self, members: &[&str]) -> u16 {
        self.recompose_named(members, &self.label)
    }

    pub(in crate::matter) fn recompose_named(&self, members: &[&str], label: &str) -> u16 {
        self.commit_compositions(&self.compositions(members, label))
            .first()
            .copied()
            .unwrap_or(self.parent)
    }

    fn commit_compositions(&self, compositions: &[Composition]) -> Vec<u16> {
        self.registry
            .stage_configuration(
                self.configuration_scope(),
                compositions.to_vec(),
                Vec::new(),
            )
            .unwrap();
        let confirmed = self
            .store
            .devices()
            .apply_pending_configuration(&self.registry, |_| {
                Ok::<_, crate::storage::CompositionCommitError>((Vec::new(), Vec::new()))
            })
            .unwrap()
            .expect("fixture configuration must change");
        confirmed
            .groups
            .iter()
            .map(|group| group.endpoint)
            .collect()
    }

    /// Stage a complete relation set for the running model to apply.
    pub(in crate::matter) fn stage(&self, members: &[&str]) {
        self.stage_groups(&[(self.label.as_str(), members)]);
    }

    pub(in crate::matter) fn stage_groups(&self, groups: &[(&str, &[&str])]) {
        let compositions = Self::group_compositions(groups)
            .into_iter()
            .filter(|group| !group.members.is_empty())
            .collect();
        self.registry
            .stage_configuration(self.configuration_scope(), compositions, Vec::new())
            .unwrap();
    }

    pub(in crate::matter) fn group_endpoint(&self, index: usize) -> u16 {
        self.store
            .devices()
            .load_group_identity(&DeviceKey::new("xiaomi", format!("group-{index}")))
            .unwrap()
            .expect("group identity must be committed")
            .endpoint
    }

    fn group_compositions(groups: &[(&str, &[&str])]) -> Vec<Composition> {
        groups
            .iter()
            .enumerate()
            .map(|(index, (label, members))| Composition {
                key: DeviceKey::new("xiaomi", format!("group-{}", index + 1)),
                label: (*label).into(),
                members: members
                    .iter()
                    .map(|did| DeviceKey::new("xiaomi", *did))
                    .collect(),
            })
            .collect()
    }

    fn compositions(&self, members: &[&str], label: &str) -> Vec<Composition> {
        if members.is_empty() {
            Vec::new()
        } else {
            vec![Composition {
                key: DeviceKey::new("xiaomi", "group-1"),
                label: label.into(),
                members: members
                    .iter()
                    .map(|did| DeviceKey::new("xiaomi", *did))
                    .collect(),
            }]
        }
    }

    pub(in crate::matter) fn model(&self) -> model::Model {
        self.model_with_retry(None)
    }

    pub(in crate::matter) fn model_with_retry(&self, retry: Option<model::Retry>) -> model::Model {
        model::handler(
            &self.registry,
            &self.store,
            StoreAdapter::new(self.store.matter()),
            crate::matter::tests::support::test_executor(),
            rand::rng(),
            retry,
        )
        .unwrap()
    }
}

macro_rules! setup_model {
    ($rig:expr, $handler:ident, $im:ident) => {
        setup_model!($rig, matter, $handler, $im, kv);
    };
    ($rig:expr, $matter:ident, $handler:ident, $im:ident, $kv:ident) => {
        setup_model!($rig, $matter, $handler, $im, $kv, $rig.model());
    };
    ($rig:expr, $matter:ident, $handler:ident, $im:ident, $kv:ident, $model:expr) => {
        let identity = $rig.store.load_identity().unwrap();
        let info = basic_info(&identity);
        let $matter = Matter::new(&info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
        let buffers: MatterBuffers = MatterBuffers::new();
        let state: EthInteractionModelState =
            EthInteractionModelState::new(EthNetwork::new_default());
        let crypto = default_crypto(rand::rng(), DAC_PRIVKEY);
        let $kv = $matter.kv(StoreAdapter::new($rig.store.matter()));
        $matter.startup(&$kv).unwrap();
        let $handler = $model;
        let $im = InteractionModel::new(&$matter, &crypto, &buffers, &$handler, &$kv, &state);
    };
}

macro_rules! rebuild_model {
    ($rig:expr, $matter:ident, $handler:ident, $im:ident, $kv:ident, $ctx:ident, $run:ident, $endpoint:expr) => {
        poll_once($run.as_mut()).await.unwrap().unwrap();
        let rebuild = $handler
            .take_rebuild()
            .expect("structural change requests rebuild");
        drop($run);
        drop($ctx);
        drop($im);
        drop($handler);
        drop($kv);
        drop($matter);
        assert!(rebuild.apply().is_none());
        setup_model!($rig, $matter, $handler, $im, $kv);
        let $ctx = Context::at(&$im, $endpoint, 6, 0);
        let mut $run = Box::pin($handler.run(&$ctx));
        assert!(poll_once($run.as_mut()).await.is_none());
    };
}

async fn reachable(handler: &impl AsyncHandler, base: &impl HandlerContext, endpoint: u16) -> bool {
    report_value(
        &read_bytes(handler, &Context::at(base, endpoint, 0x39, 17))
            .await
            .unwrap(),
    )
    .bool()
    .unwrap()
}

async fn node_label(
    handler: &impl AsyncHandler,
    base: &impl HandlerContext,
    endpoint: u16,
) -> String {
    report_value(
        &read_bytes(handler, &Context::at(base, endpoint, 0x39, 5))
            .await
            .unwrap(),
    )
    .utf8()
    .unwrap()
    .to_owned()
}

fn mark_available(rig: &mut GroupRig) {
    for member in &mut rig.members {
        member.device.set_availability(Availability::Available);
        member.device.functions()[0].observe(false);
    }
}

#[test]
fn group_publishes_one_light_endpoint_with_configuration_version() {
    let rig = GroupRig::new(&[
        ("ct-one", "yeelink.light.light3", "Ceiling One"),
        ("rgb-one", "devcea.light.ls2307", "Bedside One"),
    ]);
    let parent = rig.parent;
    let ct = rig.member("ct-one").endpoint;
    let rgb = rig.member("rgb-one").endpoint;
    setup_model!(rig, handler, im);
    handler.access(|node| {
        assert_eq!(
            node.endpoints.iter().map(|e| e.id).collect::<Vec<_>>(),
            [0, 1, parent],
            "a mixed group publishes one light endpoint and hides both members"
        );
        assert!(node.endpoint(ct).is_none(), "grouped member stays hidden");
        assert!(node.endpoint(rgb).is_none(), "grouped member stays hidden");
        let group = node.endpoint(parent).unwrap();
        for id in [0x1d, 3, 4, 6, 8, 0x300, 0x39] {
            assert!(group.cluster(id).is_some(), "group is missing cluster {id:#x}");
        }
        assert!(
            group
                .cluster(0x39)
                .unwrap()
                .attribute(
                    rs_matter::dm::clusters::decl::bridged_device_basic_information::AttributeId::ConfigurationVersion
                        as _
                )
                .is_some(),
            "group BDBI must advertise ConfigurationVersion"
        );
    });
    block_on(async {
        let version = report_value(
            &read_bytes(
                &handler,
                &Context::at(
                    &im,
                    parent,
                    0x39,
                    rs_matter::dm::clusters::decl::bridged_device_basic_information::AttributeId::ConfigurationVersion
                        as _,
                ),
            )
            .await
            .unwrap(),
        )
        .u32()
        .unwrap();
        assert!(
            version >= 1,
            "group ConfigurationVersion must be at least 1"
        );
    });
}

#[test]
fn relation_rebuild_cancels_unfinished_group_actions_without_replay() {
    use crate::device::{CommandError, LightDimension, LightPatch};

    let mut rig = GroupRig::standalone(&[
        ("ct-a", "yeelink.light.light3", "A"),
        ("ct-b", "yeelink.light.light3", "B"),
        ("ct-c", "yeelink.light.light3", "C"),
    ]);
    let parents = rig.commit_compositions(&GroupRig::group_compositions(&[
        ("One", &["ct-a", "ct-b"]),
        ("Two", &["ct-c"]),
    ]));
    let parent = parents[0];
    mark_available(&mut rig);
    for member in &rig.members {
        member.device.functions()[0]
            .observe_light(LightPatch {
                power: Some(true),
                brightness_percent: Some(20.0),
                ..Default::default()
            })
            .unwrap();
    }
    let executor = test_executor();
    block_on(async {
        setup_model!(
            rig,
            matter,
            handler,
            im,
            kv,
            model::handler(
                &rig.registry,
                &rig.store,
                StoreAdapter::new(rig.store.matter()),
                executor.clone(),
                rand::rng(),
                None,
            )
            .unwrap()
        );
        let ctx = Context::at(&im, parent, 6, 0);
        let mut run = Box::pin(handler.run(&ctx));
        assert!(poll_once(run.as_mut()).await.is_none());
        let mut tokens = Vec::new();
        {
            let mut command = Context::at(&im, parent, 8, 0);
            command.command.cmd_id = 0;
            command.data = TLVElement::new(&[
                0x15, 0x24, 0, 200, 0x25, 1, 0x58, 0x02, 0x24, 2, 0, 0x24, 3, 0, 0x18,
            ]);
            let mut out = [0; 128];
            let mut invoke = pin!(handler.invoke(
                &command,
                InvokeReplyInstance::new(&command.command, WriteBuf::new(&mut out))
            ));
            assert!(poll_once(invoke.as_mut()).await.is_none());
            for did in ["ct-a", "ct-b"] {
                let DeviceRequest::Light(frame) =
                    rig.member_mut(did).receiver.next_operation().await.unwrap()
                else {
                    panic!("expected the initial group light frame");
                };
                tokens.push((did, frame.token));
                frame.mark_sent().unwrap();
                frame.accept().unwrap();
            }
            invoke.await.unwrap();
        }
        // State refreshes and a repeated relation must leave the action active.
        rig.member("ct-a").device.functions()[0].observe(true);
        rig.stage_groups(&[("One", &["ct-b", "ct-a"]), ("Two", &["ct-c"])]);
        assert!(poll_once(run.as_mut()).await.is_none());
        for (did, token) in &tokens {
            assert!(rig.member(did).device.functions()[0].light_action_current(*token));
        }

        rig.stage_groups(&[("One", &["ct-a", "ct-c"]), ("Two", &["ct-b"])]);
        rebuild_model!(rig, matter, handler, im, kv, ctx, run, parent);
        for (did, token) in tokens {
            let function = &rig.member(did).device.functions()[0];
            assert!(!function.light_action_current(token));
            assert_eq!(
                function
                    .light_action_diagnostics()
                    .unwrap()
                    .last
                    .unwrap()
                    .result,
                Err(CommandError::Cancelled)
            );
            assert_eq!(
                executor.remaining_ms(function.function_key(), LightDimension::Brightness),
                None
            );
        }
        executor.advance();
        for member in &mut rig.members {
            assert!(
                poll_once(member.receiver.next_operation()).await.is_none(),
                "a rebuild must neither resume an old action nor replay it to a new member"
            );
        }
        assert_eq!(
            report_value(
                &read_bytes(&handler, &Context::at(&im, parent, 8, 1))
                    .await
                    .unwrap()
            )
            .u16()
            .unwrap(),
            0
        );

        // The restored group observes the newly applied member set.
        rig.member("ct-a").device.functions()[0].observe(false);
        rig.member("ct-c").device.functions()[0].observe(false);
        assert!(poll_once(run.as_mut()).await.is_none());
        assert!(
            !report_value(&read_bytes(&handler, &ctx).await.unwrap())
                .bool()
                .unwrap()
        );
    });
}

#[test]
fn rebuilt_group_fans_out_to_current_member_instances() {
    let mut rig = GroupRig::new(&[
        ("ct-one", "yeelink.light.light3", "Ceiling One"),
        ("rgb-one", "devcea.light.ls2307", "Bedside One"),
    ]);
    let parent = rig.parent;
    mark_available(&mut rig);
    block_on(async {
        setup_model!(rig, matter, handler, im, kv);
        let ctx = Context::at(&im, parent, 6, 0);
        let mut run = Box::pin(handler.run(&ctx));
        assert!(poll_once(run.as_mut()).await.is_none());
        let version = rig.store.devices().configuration_version(parent).unwrap();
        let (replacement, mut receivers) =
            PhysicalDevice::new(rig.member("rgb-one").device.description());
        replacement.set_availability(Availability::Available);
        rig.registry.revoke(replacement.key());
        rig.registry.insert(replacement.clone());
        rig.stage(&["ct-one", "rgb-one"]);
        rebuild_model!(rig, matter, handler, im, kv, ctx, run, parent);
        assert_eq!(
            rig.store.devices().configuration_version(parent).unwrap(),
            version
        );
        rig.member_mut("rgb-one").receiver = receivers.remove(0);
        replacement.functions()[0].observe(true);
        assert!(poll_once(run.as_mut()).await.is_none());
        assert!(
            report_value(&read_bytes(&handler, &ctx).await.unwrap())
                .bool()
                .unwrap()
        );
        replacement.functions()[0].observe(false);
        assert!(poll_once(run.as_mut()).await.is_none());
        let mut command = Context::at(&im, parent, 6, 0);
        command.command.cmd_id = 1;
        command.data = TLVElement::new(&[0x15, 0x18]);
        let mut out = [0; 128];
        let mut invoke = pin!(handler.invoke(
            &command,
            InvokeReplyInstance::new(&command.command, WriteBuf::new(&mut out))
        ));
        assert!(poll_once(invoke.as_mut()).await.is_none());
        // Every member is dispatched independently, not through the hidden
        // member endpoints and not through the group DID.
        for did in ["ct-one", "rgb-one"] {
            let DeviceRequest::Power(request) =
                rig.member_mut(did).receiver.next_operation().await.unwrap()
            else {
                panic!("{did} did not receive the group power write")
            };
            request.accept().unwrap();
        }
        invoke.await.unwrap();
    });
}

#[test]
fn group_reachable_follows_the_shared_member_snapshot() {
    let mut rig = GroupRig::new(&[
        ("ct-one", "yeelink.light.light3", "Ceiling One"),
        ("rgb-one", "devcea.light.ls2307", "Bedside One"),
    ]);
    let parent = rig.parent;
    block_on(async {
        setup_model!(rig, matter, handler, im, kv);
        let ctx = Context::at(&im, parent, 0x39, 17);
        let mut run = Box::pin(handler.run(&ctx));
        assert!(poll_once(run.as_mut()).await.is_none());
        assert!(!reachable(&handler, &im, parent).await);
        rig.member_mut("ct-one")
            .device
            .set_availability(Availability::Available);
        assert!(poll_once(run.as_mut()).await.is_none());
        assert!(reachable(&handler, &im, parent).await);
        rig.member_mut("ct-one")
            .device
            .set_availability(Availability::Unavailable("offline".into()));
        rig.member_mut("rgb-one")
            .device
            .set_availability(Availability::Available);
        assert!(poll_once(run.as_mut()).await.is_none());
        assert!(reachable(&handler, &im, parent).await);
        rig.member_mut("rgb-one")
            .device
            .set_availability(Availability::Unavailable("offline".into()));
        assert!(poll_once(run.as_mut()).await.is_none());
        assert!(!reachable(&handler, &im, parent).await);
    });
}

#[test]
fn group_join_and_leave_keep_member_identity_and_saved_name() {
    let rig = GroupRig::standalone(&[("ct-one", "yeelink.light.light3", "Ceiling One")]);
    let ct_endpoint = rig.member("ct-one").endpoint;
    block_on(async {
        setup_model!(rig, matter, handler, im, kv);
        let ctx = Context::at(&im, ct_endpoint, 6, 0);
        let mut run = Box::pin(handler.run(&ctx));
        assert!(poll_once(run.as_mut()).await.is_none());
        assert_eq!(node_label(&handler, &im, ct_endpoint).await, "Ceiling One");
        rig.stage(&["ct-one"]);
        rebuild_model!(rig, matter, handler, im, kv, ctx, run, 0);
        let parent = rig.group_endpoint(1);
        handler.access(|node| {
            assert!(node.endpoint(parent).is_some());
            assert!(node.endpoint(ct_endpoint).is_none());
        });
        assert_eq!(node_label(&handler, &im, parent).await, "Living room");
        // Dissolving the group restores the member's own reserved endpoint
        // and its saved name.
        rig.stage(&[]);
        rebuild_model!(rig, matter, handler, im, kv, ctx, run, ct_endpoint);
        handler.access(|node| {
            assert!(node.endpoint(parent).is_none());
            assert!(node.endpoint(ct_endpoint).is_some());
        });
        assert_eq!(node_label(&handler, &im, ct_endpoint).await, "Ceiling One");
    });
}

#[test]
fn group_metadata_matches_every_real_on_off_combination() {
    let range = NumericRange {
        min: 2700,
        max: 6500,
        step: 1,
    };
    for (brightness, temperature, rgb) in [
        (false, false, false),
        (false, true, false),
        (false, false, true),
        (false, true, true),
        (true, false, false),
        (true, false, true),
        (true, true, false),
        (true, true, true),
    ] {
        let mut capabilities = vec![Capability::OnOff];
        if brightness {
            capabilities.push(Capability::Brightness);
        }
        if temperature {
            capabilities.push(Capability::ColorTemperature(range));
        }
        if rgb {
            capabilities.push(Capability::ColorRgb);
        }
        let rig = GroupRig::build(
            &[("member-a", "yeelink.light.light3", "Member")],
            Some(&["member-a"]),
            &[("member-a", capabilities)],
        );
        let parent = rig.parent;
        let model = rig.model();
        let combo = (true, brightness, temperature, rgb);
        model.access(|node| {
            let group = node
                .endpoints
                .iter()
                .find(|candidate| candidate.id == parent)
                .unwrap_or_else(|| panic!("{combo:?} group endpoint is missing"));
            assert_light_surface(group, brightness, temperature, rgb, true, combo);
        });
    }
}

#[test]
fn unrepresentable_group_candidate_fails_before_commit_and_keeps_old_shape() {
    // Each member is publishable, but their common 3699..3700 K interval has
    // no valid integer mired range. The model must reject the candidate
    // before the transaction, keep the old published surface and expose a
    // diagnostic instead of publishing a made-up range.
    let rig = GroupRig::build(
        &[
            ("ct-small", "yeelink.light.light3", "Small"),
            ("ct-large", "yeelink.light.light3", "Large"),
        ],
        None,
        &[
            (
                "ct-small",
                vec![
                    Capability::OnOff,
                    Capability::Brightness,
                    Capability::ColorTemperature(NumericRange {
                        min: 2700,
                        max: 3700,
                        step: 1,
                    }),
                ],
            ),
            (
                "ct-large",
                vec![
                    Capability::OnOff,
                    Capability::Brightness,
                    Capability::ColorTemperature(NumericRange {
                        min: 3699,
                        max: 6500,
                        step: 1,
                    }),
                ],
            ),
        ],
    );
    let small = rig.member("ct-small").endpoint;
    let large = rig.member("ct-large").endpoint;
    block_on(async {
        setup_model!(rig, matter, handler, im, kv);
        let ctx = Context::at(&im, small, 6, 0);
        let mut run = Box::pin(handler.run(&ctx));
        assert!(poll_once(run.as_mut()).await.is_none());
        rig.stage(&["ct-small", "ct-large"]);
        poll_once(run.as_mut()).await.unwrap().unwrap();
        let rebuild = handler.take_rebuild().unwrap();
        drop(run);
        drop(ctx);
        drop(im);
        drop(handler);
        drop(kv);
        drop(matter);
        let retry = rebuild.apply().expect("unrepresentable mapping must fail");
        setup_model!(
            rig,
            matter,
            handler,
            im,
            kv,
            rig.model_with_retry(Some(retry))
        );
        let ctx = Context::at(&im, small, 6, 0);
        let mut run = Box::pin(handler.run(&ctx));
        assert!(poll_once(run.as_mut()).await.is_none());
        handler.access(|node| {
            assert_eq!(node.endpoints.len(), 4, "no fabricated group endpoint");
            assert!(node.endpoint(small).is_some());
            assert!(node.endpoint(large).is_some());
        });
        let error = rig
            .registry
            .apply_error()
            .expect("the rejected candidate is observable");
        assert!(
            error.contains("mireds") || error.contains("represent"),
            "{error}"
        );
        // A later representable candidate still applies: the failure does not
        // poison the pending queue.
        rig.stage(&["ct-small"]);
        rebuild_model!(rig, matter, handler, im, kv, ctx, run, 0);
        let valid = rig.group_endpoint(1);
        handler.access(|node| {
            assert!(node.endpoint(valid).is_some());
            assert!(node.endpoint(small).is_none());
            assert!(node.endpoint(large).is_some());
        });
        assert!(rig.registry.apply_error().is_none());
    });
}
