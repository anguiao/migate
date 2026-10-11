use super::*;

use crate::{
    device::{
        Availability, CommandReceiver, Composition, DeviceKey, DeviceRegistry, DeviceRequest,
        LightMode, LightPatch, PhysicalDevice,
    },
    storage::{CachedPhysicalDevice, CachedScope, Store},
    xiaomi::{capability::parse_device_for_model, cloud::group_id},
};
use futures_lite::future::poll_once;
use rs_matter::{
    MATTER_PORT, Matter,
    crypto::default_crypto,
    dm::{
        AsyncHandler, HandlerContext, InvokeReplyInstance,
        devices::{
            DEV_TYPE_BRIDGED_NODE,
            test::{DAC_PRIVKEY, TEST_DEV_ATT, TEST_DEV_COMM, TEST_VID},
        },
        networks::eth::EthNetwork,
    },
    error::{Error, ErrorCode},
    im::{EthInteractionModelState, InteractionModel},
    tlv::{TLVElement, TLVTag, TLVWrite},
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

pub(in crate::matter) struct Member {
    pub(in crate::matter) key: DeviceKey,
    pub(in crate::matter) name: String,
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

    /// Published members without any composition relation yet.
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
            let cached = cached_light("uid", did, model);
            let stored = store.devices().publish_cached(&cached, name).unwrap();
            let mut description = stored.description();
            if let Some((_, capabilities)) = capabilities.iter().find(|(id, _)| id == did) {
                description.functions[0]
                    .capabilities
                    .clone_from(capabilities);
            }
            let (device, mut receivers) = PhysicalDevice::new(description);
            registry.insert(device.clone());
            collected.push(Member {
                key: device.key().clone(),
                name: (*name).into(),
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

    /// Publish one complete relation set through storage and the registry, the
    /// same two-step path the adapter uses. An empty slice dissolves the
    /// composition while keeping the parent identity and endpoint reserved.
    pub(in crate::matter) fn recompose(&self, members: &[&str]) -> u16 {
        self.recompose_named(members, &self.label)
    }

    pub(in crate::matter) fn recompose_named(&self, members: &[&str], label: &str) -> u16 {
        let compositions = if members.is_empty() {
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
        };
        let confirmed = self
            .store
            .devices()
            .commit_composition(&self.scope(), &compositions, &[])
            .unwrap();
        let groups = confirmed
            .groups
            .iter()
            .map(|group| Composition {
                key: group.key.clone(),
                label: group.label.clone(),
                members: group.members.clone(),
            })
            .collect::<Vec<_>>();
        self.registry
            .update_composition_versioned(confirmed.version, groups, &[])
            .unwrap();
        confirmed
            .groups
            .first()
            .map_or(self.parent, |group| group.endpoint)
    }

    pub(in crate::matter) fn model(&self) -> model::Model {
        model::handler(
            &self.registry,
            &self.store,
            StoreAdapter::new(self.store.matter()),
            rand::rng(),
        )
        .unwrap()
    }
}

fn cluster_ids(endpoint: &rs_matter::dm::Endpoint<'_>) -> Vec<u32> {
    let mut ids = endpoint
        .clusters
        .iter()
        .map(|cluster| cluster.id)
        .collect::<Vec<_>>();
    ids.sort_unstable();
    ids
}

macro_rules! setup_model {
    ($rig:expr, $handler:ident, $im:ident) => {
        let identity = $rig.store.load_identity().unwrap();
        let info = basic_info(&identity);
        let matter = Matter::new(&info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
        let buffers: MatterBuffers = MatterBuffers::new();
        let state: EthInteractionModelState =
            EthInteractionModelState::new(EthNetwork::new_default());
        let crypto = default_crypto(rand::rng(), DAC_PRIVKEY);
        let kv = matter.kv(StoreAdapter::new($rig.store.matter()));
        matter.startup(&kv).unwrap();
        let $handler = $rig.model();
        let $im = InteractionModel::new(&matter, &crypto, &buffers, &$handler, &kv, &state);
    };
}

// Handler-only tests stop the old runner before constructing the new surface.
macro_rules! rebuild_model {
    ($rig:expr, $handler:ident, $im:ident, $ctx:ident, $run:ident, $endpoint:expr) => {
        poll_once($run.as_mut()).await.unwrap().unwrap();
        assert!($handler.needs_rebuild());
        drop($run);
        drop($im);
        drop($handler);
        setup_model!($rig, $handler, $im);
        let $ctx = Context::at(&$im, $endpoint, 0x39, 17);
        let mut $run = Box::pin($handler.run(&$ctx));
        assert!(poll_once($run.as_mut()).await.is_none());
    };
}

macro_rules! setup_group {
    ($members:expr, $rig:ident, $handler:ident, $im:ident) => {
        #[allow(unused_mut)]
        let mut $rig = GroupRig::new($members);
        setup_model!($rig, $handler, $im);
    };
}

fn report_parts(bytes: &[u8]) -> Vec<u16> {
    report_value(bytes)
        .array()
        .unwrap()
        .iter()
        .map(|part| part.unwrap().u16().unwrap())
        .collect()
}

#[test]
fn mixed_group_publishes_pure_parent_with_full_light_children() {
    let rig = GroupRig::new(&[
        ("ct-one", "yeelink.light.light3", "Ceiling One"),
        ("rgb-one", "devcea.light.ls2307", "Bedside One"),
    ]);
    let ct = rig.member("ct-one");
    let rgb = rig.member("rgb-one");
    let model = rig.model();
    model.access(|node| {
        assert_eq!(
            node.endpoints.iter().map(|e| e.id).collect::<Vec<_>>(),
            [0, 1, ct.endpoint, rgb.endpoint, rig.parent]
        );
        let parent = node.endpoint(rig.parent).unwrap();
        assert_eq!(
            parent
                .device_types
                .iter()
                .map(|kind| kind.dtype)
                .collect::<Vec<_>>(),
            [DEV_TYPE_BRIDGED_NODE.dtype]
        );
        assert_eq!(cluster_ids(parent), [0x1d, 0x39]);
        assert!(parent.semantic_tags.is_empty());
        assert!(
            parent
                .cluster(0x39)
                .unwrap()
                .attribute(rs_matter::dm::clusters::decl::bridged_device_basic_information::AttributeId::NodeLabel as _)
                .is_some()
        );
        for (member, dtype) in [(ct, 0x010c), (rgb, 0x010d)] {
            let endpoint = node.endpoint(member.endpoint).unwrap();
            assert_eq!(
                endpoint
                    .device_types
                    .iter()
                    .map(|kind| kind.dtype)
                    .collect::<Vec<_>>(),
                [dtype]
            );
            assert!(endpoint.cluster(0x39).is_none());
            for id in [0x1d, 3, 4, 6, 8, 0x300] {
                assert!(endpoint.cluster(id).is_some(), "{id} on {}", member.name);
            }
        }
    });
}

#[test]
fn group_parts_and_commands_route_to_mixed_children_only() {
    setup_group!(
        &[
            ("ct-one", "yeelink.light.light3", "Ceiling One"),
            ("rgb-one", "devcea.light.ls2307", "Bedside One"),
        ],
        rig,
        handler,
        im
    );
    let ct_endpoint = rig.member("ct-one").endpoint;
    let rgb_endpoint = rig.member("rgb-one").endpoint;
    let parent = rig.parent;
    for did in ["ct-one", "rgb-one"] {
        let member = rig.member_mut(did);
        member.device.set_availability(Availability::Available);
        member.device.functions()[0]
            .observe_light(LightPatch {
                power: Some(true),
                brightness_percent: Some(50.0),
                temperature_kelvin: Some(4000),
                mode: Some(LightMode::Temperature),
                ..Default::default()
            })
            .unwrap();
    }
    handler.access(|node| {
        let ct = node.endpoint(ct_endpoint).unwrap();
        let rgb = node.endpoint(rgb_endpoint).unwrap();
        assert!(ct.cluster(0x300).unwrap().command(0x0a).is_some());
        assert!(ct.cluster(0x300).unwrap().command(0x00).is_none());
        for id in [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 0x47, 0x4b, 0x4c] {
            assert!(rgb.cluster(0x300).unwrap().command(id).is_some(), "{id}");
        }
    });
    block_on(async {
        assert_eq!(
            report_parts(
                &read_bytes(&handler, &Context::at(&im, parent, 0x1d, 3))
                    .await
                    .unwrap()
            ),
            [ct_endpoint, rgb_endpoint]
        );
        assert_eq!(
            report_parts(
                &read_bytes(&handler, &Context::at(&im, 0, 0x1d, 3))
                    .await
                    .unwrap()
            ),
            [1, ct_endpoint, rgb_endpoint, parent]
        );
        assert_eq!(
            report_parts(
                &read_bytes(&handler, &Context::at(&im, 1, 0x1d, 3))
                    .await
                    .unwrap()
            ),
            [ct_endpoint, rgb_endpoint, parent]
        );
        for (endpoint, name) in [(ct_endpoint, "Ceiling One"), (rgb_endpoint, "Bedside One")] {
            for (attribute, expected) in [(0x400b, 154), (0x400c, 370)] {
                assert_eq!(
                    report_value(
                        &read_bytes(&handler, &Context::at(&im, endpoint, 0x300, attribute))
                            .await
                            .unwrap()
                    )
                    .u16()
                    .unwrap(),
                    expected
                );
            }
            let bytes = read_bytes(&handler, &Context::at(&im, endpoint, 0x1d, 4))
                .await
                .unwrap();
            let list = report_value(&bytes);
            let entries = list.array().unwrap().iter().collect::<Vec<_>>();
            assert_eq!(entries.len(), 2);
            let mfg = entries[0].as_ref().unwrap().structure().unwrap();
            assert_eq!(mfg.find_ctx(0).unwrap().u16().unwrap(), TEST_VID);
            assert_eq!(
                mfg.find_ctx(1).unwrap().u8().unwrap(),
                (endpoint >> 8) as u8
            );
            assert_eq!(mfg.find_ctx(2).unwrap().u8().unwrap(), endpoint as u8);
            assert_eq!(mfg.find_ctx(3).unwrap().utf8().unwrap(), name);
            let standard = entries[1].as_ref().unwrap().structure().unwrap();
            standard.find_ctx(0).unwrap().null().unwrap();
            assert_eq!(standard.find_ctx(1).unwrap().u8().unwrap(), 7);
            assert_eq!(standard.find_ctx(2).unwrap().u8().unwrap(), 0);
        }
        assert_eq!(
            read_bytes(&handler, &Context::at(&im, parent, 6, 0))
                .await
                .unwrap_err()
                .code(),
            ErrorCode::EndpointNotFound
        );
        // CT accepts On/Off through the member power gate.
        let mut command = Context::at(&im, ct_endpoint, 6, 0);
        command.command.cmd_id = 1;
        command.data = TLVElement::new(&[0x15, 0x18]);
        let mut out = [0; 128];
        let mut invoke = pin!(handler.invoke(
            &command,
            InvokeReplyInstance::new(&command.command, WriteBuf::new(&mut out))
        ));
        assert!(poll_once(invoke.as_mut()).await.is_none());
        let DeviceRequest::Power(request) = rig
            .member_mut("ct-one")
            .receiver
            .next_operation()
            .await
            .unwrap()
        else {
            panic!("on/off is not a light frame")
        };
        request.mark_sent().unwrap();
        request.accept().unwrap();
        invoke.await.unwrap();
        assert_eq!(
            rig.member("ct-one").device.functions()[0]
                .snapshot()
                .power
                .value(),
            Some(true)
        );
        // The CT child rejects RGB-only hue commands.
        let mut hue = Context::at(&im, ct_endpoint, 0x300, 0);
        hue.command.cmd_id = 0;
        hue.data = TLVElement::new(&[
            0x15, 0x24, 0, 254, 0x24, 1, 2, 0x25, 2, 0, 0, 0x24, 3, 0, 0x24, 4, 0, 0x18,
        ]);
        assert_eq!(
            handler
                .invoke(
                    &hue,
                    InvokeReplyInstance::new(&hue.command, WriteBuf::new(&mut [0; 128]))
                )
                .await
                .unwrap_err()
                .code(),
            ErrorCode::CommandNotFound
        );
        assert!(
            poll_once(rig.member_mut("ct-one").receiver.next_operation())
                .await
                .is_none()
        );
        // The RGB child executes the same command end to end.
        let mut hue = Context::at(&im, rgb_endpoint, 0x300, 0);
        hue.command.cmd_id = 0;
        hue.data = TLVElement::new(&[
            0x15, 0x24, 0, 254, 0x24, 1, 2, 0x25, 2, 0, 0, 0x24, 3, 0, 0x24, 4, 0, 0x18,
        ]);
        let mut out = [0; 128];
        let mut invoke = pin!(handler.invoke(
            &hue,
            InvokeReplyInstance::new(&hue.command, WriteBuf::new(&mut out))
        ));
        assert!(poll_once(invoke.as_mut()).await.is_none());
        let DeviceRequest::Light(hue) = rig
            .member_mut("rgb-one")
            .receiver
            .next_operation()
            .await
            .unwrap()
        else {
            panic!("hue is not a light frame")
        };
        assert!(hue.final_frame);
        hue.mark_sent().unwrap();
        hue.accept().unwrap();
        invoke.await.unwrap();
        // The RGB child also accepts color temperature.
        let mut ct = Context::at(&im, rgb_endpoint, 0x300, 0);
        ct.command.cmd_id = 0x0a;
        ct.data = TLVElement::new(&[
            0x15, 0x25, 0, 200, 0, 0x25, 1, 0, 0, 0x24, 2, 0, 0x24, 3, 0, 0x18,
        ]);
        let mut out = [0; 128];
        let mut invoke = pin!(handler.invoke(
            &ct,
            InvokeReplyInstance::new(&ct.command, WriteBuf::new(&mut out))
        ));
        assert!(poll_once(invoke.as_mut()).await.is_none());
        let DeviceRequest::Light(frame) = rig
            .member_mut("rgb-one")
            .receiver
            .next_operation()
            .await
            .unwrap()
        else {
            panic!("temperature is not a light frame")
        };
        frame.mark_sent().unwrap();
        frame.accept().unwrap();
        invoke.await.unwrap();
    });
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

#[test]
fn group_parent_reachable_tracks_any_member_and_starts_jobs_on_first_run() {
    let mut rig = GroupRig::new(&[
        ("ct-one", "yeelink.light.light3", "Ceiling One"),
        ("rgb-one", "devcea.light.ls2307", "Bedside One"),
    ]);
    let parent = rig.parent;
    let ct_endpoint = rig.member("ct-one").endpoint;
    let rgb_endpoint = rig.member("rgb-one").endpoint;
    block_on(async {
        setup_model!(rig, handler, im);
        let ctx = Context::at(&im, parent, 0x39, 17);
        let mut run = Box::pin(handler.run(&ctx));
        assert!(poll_once(run.as_mut()).await.is_none());
        assert!(!reachable(&handler, &im, parent).await);
        // Establishing the shape boundary reports exactly the root
        // configuration version; nothing else may notify at startup.
        assert_eq!(
            ctx.changes.borrow().clone(),
            vec![(0, 0x28, 0x18)],
            "startup may only report the baseline configuration version"
        );
        ctx.changes.borrow_mut().clear();
        assert!(ctx.changes.borrow().is_empty());
        // The first loop seeds jobs from the already reconciled topology: a
        // member state report must be visible without any registry change.
        rig.member("rgb-one").device.functions()[0].observe(true);
        assert!(poll_once(run.as_mut()).await.is_none());
        assert!(ctx.changes.borrow().contains(&(rgb_endpoint, 6, 0)));
        ctx.changes.borrow_mut().clear();

        rig.member_mut("ct-one")
            .device
            .set_availability(Availability::Unavailable("offline".into()));
        rig.member_mut("rgb-one")
            .device
            .set_availability(Availability::Unavailable("offline".into()));
        assert!(poll_once(run.as_mut()).await.is_none());
        assert!(!reachable(&handler, &im, parent).await);
        assert!(ctx.changes.borrow().is_empty());

        rig.member_mut("ct-one")
            .device
            .set_availability(Availability::Available);
        assert!(poll_once(run.as_mut()).await.is_none());
        assert!(reachable(&handler, &im, parent).await);
        assert!(ctx.changes.borrow().contains(&(parent, 0x39, 17)));
        assert!(ctx.events.borrow().contains(&(parent, 0x39, 3)));
        ctx.changes.borrow_mut().clear();
        ctx.events.borrow_mut().clear();

        rig.member_mut("ct-one")
            .device
            .set_availability(Availability::Unavailable("offline".into()));
        rig.member_mut("rgb-one")
            .device
            .set_availability(Availability::Available);
        assert!(poll_once(run.as_mut()).await.is_none());
        // Another member became reachable, so the aggregate stays true and
        // reports nothing.
        assert!(reachable(&handler, &im, parent).await);
        assert!(ctx.changes.borrow().is_empty());
        ctx.events.borrow_mut().clear();

        // Light state, power and first-read success never decide Reachable.
        rig.member_mut("rgb-one").device.functions()[0].observe(true);
        assert!(poll_once(run.as_mut()).await.is_none());
        assert!(reachable(&handler, &im, parent).await);
        assert!(
            !ctx.changes
                .borrow()
                .iter()
                .any(|change| change.0 != rgb_endpoint)
        );

        // Rebuilding after removal seeds the new group from its remaining member.
        ctx.changes.borrow_mut().clear();
        rig.recompose(&["ct-one"]);
        rebuild_model!(rig, handler, im, ctx, run, parent);
        assert!(!reachable(&handler, &im, parent).await);
        rig.member_mut("ct-one")
            .device
            .set_availability(Availability::Available);
        assert!(poll_once(run.as_mut()).await.is_none());
        assert!(reachable(&handler, &im, parent).await);
        assert!(ctx.changes.borrow().contains(&(parent, 0x39, 17)));
        ctx.changes.borrow_mut().clear();
        ctx.events.borrow_mut().clear();

        rig.member_mut("ct-one")
            .device
            .set_availability(Availability::Unavailable("offline".into()));
        assert!(poll_once(run.as_mut()).await.is_none());
        assert!(!reachable(&handler, &im, parent).await);
        assert!(ctx.changes.borrow().contains(&(parent, 0x39, 17)));
        assert!(ctx.events.borrow().contains(&(parent, 0x39, 3)));
        assert_eq!(ct_endpoint, rig.member("ct-one").endpoint);
    });
}

async fn member_tags(
    handler: &impl AsyncHandler,
    base: &impl HandlerContext,
    endpoint: u16,
) -> Vec<(Option<u16>, u8, u8, Option<String>)> {
    let bytes = read_bytes(handler, &Context::at(base, endpoint, 0x1d, 4))
        .await
        .unwrap();
    report_value(&bytes)
        .array()
        .unwrap()
        .iter()
        .map(|entry| {
            let tag = entry.unwrap().structure().unwrap();
            let mfg = tag.find_ctx(0).ok().and_then(|value| value.u16().ok());
            let label = tag
                .find_ctx(3)
                .ok()
                .and_then(|value| value.utf8().ok().map(str::to_owned));
            (
                mfg,
                tag.find_ctx(1).unwrap().u8().unwrap(),
                tag.find_ctx(2).unwrap().u8().unwrap(),
                label,
            )
        })
        .collect()
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

async fn write_label(
    handler: &impl AsyncHandler,
    base: &impl HandlerContext,
    endpoint: u16,
    label: &str,
) -> Result<(), Error> {
    let mut buffer = [0; 128];
    let mut write = WriteBuf::new(&mut buffer);
    write.utf8(&TLVTag::Anonymous, label).unwrap();
    let mut ctx = Context::at(base, endpoint, 0x39, 5);
    ctx.data = TLVElement::new(write.as_slice());
    handler.write(&ctx).await
}

#[test]
fn member_tags_use_saved_names_and_stay_stable_across_reorder_and_removal() {
    let rig = GroupRig::new(&[
        ("ct-a", "yeelink.light.light3", "Shared Name"),
        ("ct-b", "yeelink.light.light3", "Shared Name"),
        ("rgb-c", "devcea.light.ls2307", "Bedside"),
    ]);
    let parent = rig.parent;
    block_on(async {
        setup_model!(rig, handler, im);
        let ctx = Context::at(&im, parent, 0x1d, 3);
        let mut run = Box::pin(handler.run(&ctx));
        assert!(poll_once(run.as_mut()).await.is_none());
        let expected = |did: &str| {
            let member = rig.member(did);
            vec![
                (
                    Some(TEST_VID),
                    (member.endpoint >> 8) as u8,
                    member.endpoint as u8,
                    Some(member.name.clone()),
                ),
                (None, 7, 0, None),
            ]
        };
        let mut initial = Vec::new();
        for did in ["ct-a", "ct-b", "rgb-c"] {
            let tags = member_tags(&handler, &im, rig.member(did).endpoint).await;
            assert_eq!(tags, expected(did));
            initial.push((did, tags));
        }
        assert_ne!(
            initial[0].1, initial[1].1,
            "same-name children must stay distinguishable"
        );
        // Reordering the relation must not renumber or relabel members.
        rig.recompose(&["rgb-c", "ct-b", "ct-a"]);
        assert!(poll_once(run.as_mut()).await.is_none());
        for (did, tags) in &initial {
            assert_eq!(
                member_tags(&handler, &im, rig.member(did).endpoint).await,
                *tags
            );
        }
        // Removing one member must not renumber the others.
        rig.recompose(&["ct-a", "rgb-c"]);
        rebuild_model!(rig, handler, im, ctx, run, parent);
        for (did, tags) in initial.iter().filter(|(did, _)| *did != "ct-b") {
            assert_eq!(
                member_tags(&handler, &im, rig.member(did).endpoint).await,
                *tags
            );
        }
        // Leaving the group keeps the saved member name as its NodeLabel.
        rig.recompose(&[]);
        rebuild_model!(rig, handler, im, ctx, run, parent);
        assert_eq!(
            node_label(&handler, &im, rig.member("ct-a").endpoint).await,
            "Shared Name"
        );
    });
}

#[test]
fn group_and_member_saved_names_survive_rename_join_leave_and_restart() {
    let rig = GroupRig::standalone(&[("ct-one", "yeelink.light.light3", "Ceiling One")]);
    let ct_endpoint = rig.member("ct-one").endpoint;
    let mut parent = 0;
    {
        block_on(async {
            setup_model!(rig, handler, im);
            let ctx = Context::at(&im, ct_endpoint, 0x39, 5);
            let mut run = Box::pin(handler.run(&ctx));
            assert!(poll_once(run.as_mut()).await.is_none());
            write_label(&handler, &im, ct_endpoint, "Renamed Lamp")
                .await
                .unwrap();
            assert_eq!(
                rig.store
                    .devices()
                    .load_published(&DeviceKey::new("xiaomi", "ct-one"))
                    .unwrap()
                    .unwrap()
                    .label,
                "Renamed Lamp"
            );
            parent = rig.recompose(&["ct-one"]);
            rebuild_model!(rig, handler, im, ctx, run, ct_endpoint);
            assert_eq!(
                member_tags(&handler, &im, ct_endpoint).await[0]
                    .3
                    .as_deref(),
                Some("Renamed Lamp")
            );
            write_label(&handler, &im, parent, "Kitchen Group")
                .await
                .unwrap();
            assert_eq!(
                rig.store
                    .devices()
                    .load_group_identity(&DeviceKey::new("xiaomi", "group-1"))
                    .unwrap()
                    .unwrap()
                    .label,
                "Kitchen Group"
            );
            assert_eq!(
                rig.registry.snapshot().composition.groups[0].label,
                "Kitchen Group"
            );
            assert_eq!(node_label(&handler, &im, parent).await, "Kitchen Group");
            // A later cloud proposal never overwrites the saved group name.
            rig.recompose_named(&["ct-one"], "Cloud Name");
            assert!(poll_once(run.as_mut()).await.is_none());
            assert_eq!(
                rig.store
                    .devices()
                    .load_group_identity(&DeviceKey::new("xiaomi", "group-1"))
                    .unwrap()
                    .unwrap()
                    .label,
                "Kitchen Group"
            );
            assert_eq!(
                rig.registry.snapshot().composition.groups[0].label,
                "Kitchen Group"
            );
            assert_eq!(node_label(&handler, &im, parent).await, "Kitchen Group");
            assert_eq!(
                report_value(
                    &read_bytes(&handler, &Context::at(&im, parent, 0x39, 0x12))
                        .await
                        .unwrap()
                )
                .utf8()
                .unwrap(),
                rig.store
                    .devices()
                    .load_group_identity(&DeviceKey::new("xiaomi", "group-1"))
                    .unwrap()
                    .unwrap()
                    .unique_id
            );
            // Dissolving the group restores the member's own saved name; a
            // later rejoin reuses the reserved parent endpoint and name.
            rig.recompose(&[]);
            rebuild_model!(rig, handler, im, ctx, run, ct_endpoint);
            assert_eq!(rig.member("ct-one").endpoint, ct_endpoint);
            assert_eq!(node_label(&handler, &im, ct_endpoint).await, "Renamed Lamp");
            let rejoined = rig.recompose(&["ct-one"]);
            rebuild_model!(rig, handler, im, ctx, run, ct_endpoint);
            assert_eq!(rejoined, parent);
            assert_eq!(node_label(&handler, &im, parent).await, "Kitchen Group");
            assert_eq!(
                member_tags(&handler, &im, ct_endpoint).await[0]
                    .3
                    .as_deref(),
                Some("Renamed Lamp")
            );
            rig.recompose(&[]);
            rebuild_model!(rig, handler, im, ctx, run, ct_endpoint);
        });
    }
    // Restarting the model from the same storage keeps the member name; the
    // group name still wins over a fresh relation proposal.
    {
        block_on(async {
            setup_model!(rig, handler, im);
            assert_eq!(node_label(&handler, &im, ct_endpoint).await, "Renamed Lamp");
            let ctx = Context::at(&im, ct_endpoint, 0x39, 5);
            let mut run = Box::pin(handler.run(&ctx));
            assert!(poll_once(run.as_mut()).await.is_none());
            let rejoined = rig.recompose(&["ct-one"]);
            rebuild_model!(rig, handler, im, ctx, run, ct_endpoint);
            assert_eq!(rejoined, parent);
            assert_eq!(node_label(&handler, &im, parent).await, "Kitchen Group");
            assert_eq!(
                member_tags(&handler, &im, ct_endpoint).await[0]
                    .3
                    .as_deref(),
                Some("Renamed Lamp")
            );
        });
    }
}

#[test]
fn more_than_255_members_keep_collision_free_tags_and_complete_parts() {
    let dids = (0..258)
        .map(|index| format!("bulk-{index}"))
        .collect::<Vec<_>>();
    let names = (0..258)
        .map(|index| format!("Bulk {index}"))
        .collect::<Vec<_>>();
    let members = dids
        .iter()
        .zip(&names)
        .map(|(did, name)| (did.as_str(), "yeelink.light.light3", name.as_str()))
        .collect::<Vec<_>>();
    setup_group!(&members, rig, handler, im);
    let parent = rig.parent;
    let highest = rig
        .members
        .iter()
        .max_by_key(|member| member.endpoint)
        .unwrap();
    assert!(highest.endpoint > 255);
    let mut seen = std::collections::HashSet::new();
    handler.access(|node| {
        assert!(node.endpoint(parent).unwrap().semantic_tags.is_empty());
        for member in &rig.members {
            let endpoint = node.endpoint(member.endpoint).unwrap();
            // The identity key is (manufacturer, namespace, tag); labels are
            // asserted separately so per-member names cannot mask a collision.
            assert!(
                seen.insert((
                    endpoint.semantic_tags[0].mfg_code,
                    endpoint.semantic_tags[0].namespace_id,
                    endpoint.semantic_tags[0].tag,
                )),
                "endpoint {} repeats a manufacturer tag",
                member.endpoint
            );
            assert_eq!(endpoint.semantic_tags[0].mfg_code, Some(TEST_VID));
            assert_eq!(
                u16::from(endpoint.semantic_tags[0].namespace_id),
                member.endpoint >> 8
            );
            assert_eq!(
                u16::from(endpoint.semantic_tags[0].tag),
                member.endpoint & 0xff
            );
            assert_eq!(endpoint.semantic_tags[0].label, Some(member.name.as_str()));
            assert_eq!(
                (
                    endpoint.semantic_tags[1].mfg_code,
                    endpoint.semantic_tags[1].namespace_id,
                    endpoint.semantic_tags[1].tag
                ),
                (None, 7, 0)
            );
        }
    });
    assert_eq!(seen.len(), rig.members.len());
    block_on(async {
        let parts = report_parts(
            &read_bytes(&handler, &Context::at(&im, parent, 0x1d, 3))
                .await
                .unwrap(),
        );
        assert_eq!(parts.len(), rig.members.len());
        let unique = parts
            .iter()
            .copied()
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(unique.len(), parts.len());
        assert!(parts.iter().all(|endpoint| {
            rig.members
                .iter()
                .any(|member| member.endpoint == *endpoint)
        }));
        let bytes = read_bytes(&handler, &Context::at(&im, highest.endpoint, 0x1d, 4))
            .await
            .unwrap();
        let list = report_value(&bytes);
        let mfg = list
            .array()
            .unwrap()
            .iter()
            .next()
            .unwrap()
            .unwrap()
            .structure()
            .unwrap();
        assert_eq!(mfg.find_ctx(0).unwrap().u16().unwrap(), TEST_VID);
        assert_eq!(
            u16::from(mfg.find_ctx(1).unwrap().u8().unwrap()),
            highest.endpoint >> 8
        );
        assert_eq!(
            u16::from(mfg.find_ctx(2).unwrap().u8().unwrap()),
            highest.endpoint & 0xff
        );
    });
}

#[test]
fn member_external_reports_and_command_failures_stay_isolated() {
    setup_group!(
        &[
            ("ct-one", "yeelink.light.light3", "Ceiling One"),
            ("rgb-one", "devcea.light.ls2307", "Bedside One"),
        ],
        rig,
        handler,
        im
    );
    let ct_endpoint = rig.member("ct-one").endpoint;
    let rgb_endpoint = rig.member("rgb-one").endpoint;
    block_on(async {
        let ctx = Context::at(&im, ct_endpoint, 6, 0);
        let mut run = Box::pin(handler.run(&ctx));
        assert!(poll_once(run.as_mut()).await.is_none());
        rig.member_mut("ct-one")
            .device
            .set_availability(Availability::Available);
        rig.member_mut("rgb-one")
            .device
            .set_availability(Availability::Unavailable("offline".into()));
        assert!(poll_once(run.as_mut()).await.is_none());
        ctx.changes.borrow_mut().clear();
        rig.member("ct-one").device.functions()[0]
            .observe_light(LightPatch {
                power: Some(true),
                brightness_percent: Some(40.0),
                temperature_kelvin: Some(3000),
                mode: Some(LightMode::Temperature),
                ..Default::default()
            })
            .unwrap();
        assert!(poll_once(run.as_mut()).await.is_none());
        assert!(ctx.changes.borrow().contains(&(ct_endpoint, 6, 0)));
        assert!(ctx.changes.borrow().contains(&(ct_endpoint, 8, 0)));
        assert!(ctx.changes.borrow().contains(&(ct_endpoint, 0x300, 7)));
        assert!(
            !ctx.changes
                .borrow()
                .iter()
                .any(|(endpoint, _, _)| *endpoint == rgb_endpoint)
        );
        assert!(
            report_value(
                &read_bytes(&handler, &Context::at(&im, ct_endpoint, 6, 0))
                    .await
                    .unwrap()
            )
            .bool()
            .unwrap()
        );
        assert_eq!(
            read_bytes(&handler, &Context::at(&im, rgb_endpoint, 6, 0))
                .await
                .unwrap_err()
                .code(),
            ErrorCode::Failure
        );

        // One unavailable member never blocks another member's command.
        let mut command = Context::at(&im, ct_endpoint, 6, 0);
        command.command.cmd_id = 0;
        command.data = TLVElement::new(&[0x15, 0x18]);
        let mut out = [0; 128];
        let mut invoke = pin!(handler.invoke(
            &command,
            InvokeReplyInstance::new(&command.command, WriteBuf::new(&mut out))
        ));
        assert!(poll_once(invoke.as_mut()).await.is_none());
        let DeviceRequest::Power(request) = rig
            .member_mut("ct-one")
            .receiver
            .next_operation()
            .await
            .unwrap()
        else {
            panic!("on/off is not a light frame")
        };
        request.accept().unwrap();
        invoke.await.unwrap();
        assert_eq!(
            rig.member("ct-one").device.functions()[0]
                .snapshot()
                .power
                .value(),
            Some(false)
        );

        let mut command = Context::at(&im, rgb_endpoint, 6, 0);
        command.command.cmd_id = 1;
        command.data = TLVElement::new(&[0x15, 0x18]);
        assert_eq!(
            handler
                .invoke(
                    &command,
                    InvokeReplyInstance::new(&command.command, WriteBuf::new(&mut [0; 128]))
                )
                .await
                .unwrap_err()
                .code(),
            ErrorCode::Failure
        );
        assert!(
            poll_once(rig.member_mut("rgb-one").receiver.next_operation())
                .await
                .is_none()
        );
    });
}

#[test]
fn group_child_metadata_matches_every_real_on_off_combination() {
    let range = crate::device::NumericRange {
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
        let child_endpoint = rig.member("member-a").endpoint;
        let model = rig.model();
        let combo = (true, brightness, temperature, rgb);
        model.access(|node| {
            let child = node
                .endpoints
                .iter()
                .find(|candidate| candidate.id == child_endpoint)
                .unwrap_or_else(|| panic!("{combo:?} child endpoint is missing"));
            assert_light_surface(child, brightness, temperature, rgb, false, combo);
        });
    }
}

#[test]
fn group_with_unmappable_member_is_not_published_and_others_fall_back_to_standalone() {
    let rig = GroupRig::build(
        &[
            ("member-a", "yeelink.light.light3", "Ceiling"),
            ("member-b", "devcea.light.ls2307", "Bedside"),
        ],
        Some(&["member-a", "member-b"]),
        &[("member-a", vec![Capability::Brightness])],
    );
    let parent = rig.parent;
    let unmappable = rig.member("member-a").endpoint;
    let mappable = rig.member("member-b").endpoint;
    let model = rig.model();
    model.access(|node| {
        let ids = node
            .endpoints
            .iter()
            .map(|endpoint| endpoint.id)
            .collect::<Vec<_>>();
        assert!(
            !ids.contains(&parent),
            "a group with an unmappable member must not publish its parent"
        );
        assert!(
            !ids.contains(&unmappable),
            "the unmappable member has no Matter endpoint"
        );
        let child = node
            .endpoints
            .iter()
            .find(|endpoint| endpoint.id == mappable)
            .expect("the mappable member stays published as a standalone light");
        assert_light_surface(child, true, true, true, true, (true, true, true, true));
    });
    let key = rig.member("member-a").key.clone();
    assert!(
        rig.store.devices().load_published(&key).unwrap().is_some(),
        "the published identity of the unmappable member is not mutated"
    );
}
