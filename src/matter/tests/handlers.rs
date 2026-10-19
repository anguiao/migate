use super::super::{
    bridged_info::BridgedHandler,
    model::{self, basic_info},
    storage::StoreAdapter,
};
use super::support::{Context, attr, next_power};
use crate::{
    device::{Availability, CommandError, Power},
    storage::Store,
};
use futures_lite::future::{block_on, poll_once};
use rs_matter::{
    MATTER_PORT, Matter,
    crypto::default_crypto,
    dm::{
        AsyncHandler, Dataver, HandlerContext, InvokeReplyInstance, Metadata, ReadReplyInstance,
        clusters::decl::bridged_device_basic_information::{self as bridged, ClusterHandler as _},
        devices::test::{DAC_PRIVKEY, TEST_DEV_ATT, TEST_DEV_COMM},
        networks::eth::EthNetwork,
    },
    error::{Error, ErrorCode},
    im::{EthInteractionModelState, InteractionModel},
    tlv::{TLVElement, TLVTag, TLVWrite, TLVWriteParent, Utf8StrBuilder},
    transport::exchange::MatterBuffers,
    utils::storage::WriteBuf,
};
use std::pin::pin;

#[test]
fn actual_handler_waits_for_acceptance_reads_unknown_and_reports_shared_values() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let identity = store.load_identity().unwrap();
    let (device, mut receiver) = super::test_device();
    let info = basic_info(&identity);
    let matter = Matter::new(&info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
    let buffers: MatterBuffers = MatterBuffers::new();
    let state: EthInteractionModelState = EthInteractionModelState::new(EthNetwork::new_default());
    let crypto = default_crypto(rand::rng(), DAC_PRIVKEY);
    let kv = matter.kv(StoreAdapter::new(store.matter()));
    let registry = super::test_registry(&store, &device);
    let handler = model::handler(
        &registry,
        &store,
        StoreAdapter::new(store.matter()),
        rand::rng(),
    )
    .unwrap();
    let im = InteractionModel::new(&matter, &crypto, &buffers, &handler, &kv, &state);
    let mut ctx = Context::new(&im, 6, 0);
    block_on(async {
        let mut out = [0; 128];
        let mut write = WriteBuf::new(&mut out);
        assert_eq!(
            handler
                .read(&ctx, ReadReplyInstance::new(&ctx.attribute, &mut write))
                .await
                .unwrap_err()
                .code(),
            ErrorCode::Failure
        );
        device.functions()[0].observe(false);
        device.set_availability(Availability::Available);
        for (command, power) in [(1, true), (0, false), (2, true), (2, false)] {
            ctx.command.cmd_id = command;
            let before = device.functions()[0].snapshot();
            let mut out = [0; 128];
            let mut invoke = pin!(handler.invoke(
                &ctx,
                InvokeReplyInstance::new(&ctx.command, WriteBuf::new(&mut out))
            ));
            assert!(poll_once(invoke.as_mut()).await.is_none());
            assert_eq!(device.functions()[0].snapshot(), before);
            next_power(&mut receiver).await.unwrap().accept().unwrap();
            invoke.await.unwrap();
            assert_eq!(device.functions()[0].snapshot().power.value(), Some(power));
        }
        for bytes in [&[][..], &[0x15], &[0x15, 0x24], &[0x15, 0x35, 0], &[0x09]] {
            ctx.data = TLVElement::new(bytes);
            let error = handler
                .invoke(
                    &ctx,
                    InvokeReplyInstance::new(&ctx.command, WriteBuf::new(&mut [0; 128])),
                )
                .await
                .unwrap_err();
            assert_eq!(error.code(), ErrorCode::InvalidCommand);
            assert!(poll_once(next_power(&mut receiver)).await.is_none());
        }
        ctx.data = TLVElement::new(&[0x15, 0x18]);
        ctx.command.cmd_id = 1;
        let mut out = [0; 128];
        let mut invoke = pin!(handler.invoke(
            &ctx,
            InvokeReplyInstance::new(&ctx.command, WriteBuf::new(&mut out))
        ));
        assert!(poll_once(invoke.as_mut()).await.is_none());
        next_power(&mut receiver)
            .await
            .unwrap()
            .reject(CommandError::Rejected("denied".into()));
        assert_eq!(invoke.await.unwrap_err().code(), ErrorCode::Failure);
        assert_eq!(device.functions()[0].snapshot().power, Power::Off);
        let mut run = pin!(handler.run(&ctx));
        assert!(poll_once(run.as_mut()).await.is_none());
        assert!(ctx.changes.borrow().contains(&(2, 6, 0)));
        assert!(ctx.changes.borrow().contains(&(2, 57, 17)));
        ctx.changes.borrow_mut().clear();
        device.functions()[0].observe(true);
        assert!(poll_once(run.as_mut()).await.is_none());
        assert!(ctx.changes.borrow().contains(&(2, 6, 0)));
        let count = ctx.changes.borrow().len();
        device.functions()[0].observe(true);
        assert!(poll_once(run.as_mut()).await.is_none());
        assert_eq!(ctx.changes.borrow().len(), count);
        device.functions()[0].observe(false);
        device.functions()[0].observe(true);
        assert!(poll_once(run.as_mut()).await.is_none());
        assert_eq!(ctx.changes.borrow().len(), count);
        device.set_availability(Availability::Unavailable("offline".into()));
        assert!(poll_once(run.as_mut()).await.is_none());
        assert!(ctx.changes.borrow().contains(&(2, 57, 17)));
        assert!(ctx.events.borrow().contains(&(2, 57, 3)));
        let count = ctx.changes.borrow().len();
        let events = ctx.events.borrow().len();
        device.set_availability(Availability::Unavailable("another reason".into()));
        assert!(poll_once(run.as_mut()).await.is_none());
        assert_eq!(ctx.changes.borrow().len(), count);
        assert_eq!(ctx.events.borrow().len(), events);
        let mut out = [0; 128];
        let mut write = WriteBuf::new(&mut out);
        handler
            .read(&ctx, ReadReplyInstance::new(&ctx.attribute, &mut write))
            .await
            .unwrap();
        assert!(
            TLVElement::new(write.as_slice())
                .structure()
                .unwrap()
                .find_ctx(1)
                .unwrap()
                .structure()
                .unwrap()
                .find_ctx(2)
                .unwrap()
                .bool()
                .unwrap()
        );
    });
}

#[test]
fn bridged_label_is_persisted_and_invalid_writes_preserve_it() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let identity = store.load_identity().unwrap();
    let (device, _receiver) = super::test_device();
    let info = basic_info(&identity);
    let matter = Matter::new(&info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
    let buffers: MatterBuffers = MatterBuffers::new();
    let state: EthInteractionModelState = EthInteractionModelState::new(EthNetwork::new_default());
    let crypto = default_crypto(rand::rng(), DAC_PRIVKEY);
    let kv = matter.kv(StoreAdapter::new(store.matter()));
    let registry = super::test_registry(&store, &device);
    let physical = registry.get(device.key()).unwrap();
    let stored = store
        .devices()
        .load_published(device.key())
        .unwrap()
        .unwrap();
    let bridged = BridgedHandler::new(
        physical,
        Dataver::new(1),
        &stored,
        store.devices(),
        StoreAdapter::new(store.matter()),
    );
    let handler = model::handler(
        &registry,
        &store,
        StoreAdapter::new(store.matter()),
        rand::rng(),
    )
    .unwrap();
    let im = InteractionModel::new(&matter, &crypto, &buffers, &handler, &kv, &state);
    let ctx = Context::new(&im, 57, bridged::AttributeId::NodeLabel as _);
    assert!(!bridged.reachable(&ctx).unwrap());
    device.set_availability(Availability::Available);
    assert!(bridged.reachable(&ctx).unwrap());
    bridged.set_node_label(&ctx, "Study Outlet").unwrap();
    assert_eq!(
        Store::open(dir.path())
            .unwrap()
            .devices()
            .load_published(device.key())
            .unwrap()
            .unwrap()
            .label,
        "Study Outlet"
    );
    let count = ctx.changes.borrow().len();
    bridged.set_node_label(&ctx, "Study Outlet").unwrap();
    assert_eq!(ctx.changes.borrow().len(), count);
    assert!(bridged.set_node_label(&ctx, &"x".repeat(33)).is_err());
    let mut bytes = [0; 64];
    let mut writer = WriteBuf::new(&mut bytes);
    bridged
        .node_label(
            &ctx,
            Utf8StrBuilder::new(TLVWriteParent::new((), &mut writer), &TLVTag::Anonymous),
        )
        .unwrap();
    assert_eq!(
        TLVElement::new(writer.as_slice()).utf8().unwrap(),
        "Study Outlet"
    );
    let renamed = store
        .devices()
        .load_published(device.key())
        .unwrap()
        .unwrap();
    assert_eq!(renamed.unique_id, stored.unique_id);
    assert_eq!(renamed.endpoint, stored.endpoint);
}

async fn read_bytes(
    handler: &impl AsyncHandler,
    ctx: &Context<'_, impl HandlerContext>,
) -> Result<Vec<u8>, Error> {
    let mut buffer = [0; 4096];
    let mut write = WriteBuf::new(&mut buffer);
    handler
        .read(ctx, ReadReplyInstance::new(&ctx.attribute, &mut write))
        .await?;
    Ok(write.as_slice().to_vec())
}
fn report_value(bytes: &[u8]) -> TLVElement<'_> {
    TLVElement::new(bytes)
        .structure()
        .unwrap()
        .find_ctx(1)
        .unwrap()
        .structure()
        .unwrap()
        .find_ctx(2)
        .unwrap()
}
fn report_dataver(bytes: &[u8]) -> u32 {
    TLVElement::new(bytes)
        .structure()
        .unwrap()
        .find_ctx(1)
        .unwrap()
        .structure()
        .unwrap()
        .find_ctx(0)
        .unwrap()
        .u32()
        .unwrap()
}

#[test]
fn frozen_topology_channels_and_coalesced_rejoin_share_canonical_state() {
    block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let identity = store.load_identity().unwrap();
        let info = basic_info(&identity);
        let matter = Matter::new(&info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
        let buffers: MatterBuffers = MatterBuffers::new();
        let state: EthInteractionModelState =
            EthInteractionModelState::new(EthNetwork::new_default());
        let crypto = default_crypto(rand::rng(), DAC_PRIVKEY);
        let protocol = StoreAdapter::new(store.matter());
        let kv = matter.kv(protocol.clone());
        let registry = crate::device::DeviceRegistry::new();

        // An empty committed set serves only the root and aggregator.
        {
            let handler = model::handler(&registry, &store, protocol.clone(), rand::rng()).unwrap();
            let im = InteractionModel::new(&matter, &crypto, &buffers, &handler, &kv, &state);
            let ctx = Context::new(&im, 6, 0);
            let mut run = pin!(handler.run(&ctx));
            assert!(poll_once(run.as_mut()).await.is_none());
            assert_eq!(
                handler.access(|node| node
                    .endpoints
                    .iter()
                    .map(|endpoint| endpoint.id)
                    .collect::<Vec<_>>()),
                [0, 1]
            );
        }

        // Commit one single-function and one multi-function outlet, then build
        // the next frozen instance from the committed set.
        let (single, mut single_receivers) = super::physical("single", false);
        let single_stored = super::support::add_device(&store, &single.description());
        assert_eq!(single_stored.endpoint, 2);
        let (multi, mut receivers) = super::physical("multi", true);
        let stored = super::support::add_device(&store, &multi.description());
        registry.insert(single.clone());
        registry.insert(multi.clone());
        {
            let handler = model::handler(&registry, &store, protocol.clone(), rand::rng()).unwrap();
            let im = InteractionModel::new(&matter, &crypto, &buffers, &handler, &kv, &state);
            let ctx = Context::new(&im, 6, 0);
            let mut run = pin!(handler.run(&ctx));
            assert!(poll_once(run.as_mut()).await.is_none());
            assert_eq!(
                handler.access(|node| node
                    .endpoints
                    .iter()
                    .map(|endpoint| endpoint.id)
                    .collect::<Vec<_>>()),
                [0, 1, 2, 3, 4, 5, 6]
            );
            for (endpoint, expected) in [
                (0, vec![1, 2, 3, 4, 5, 6]),
                (1, vec![2, 3, 4, 5, 6]),
                (3, vec![4, 5, 6]),
                (4, vec![]),
            ] {
                let bytes = read_bytes(&handler, &Context::at(&im, endpoint, 0x1d, 3))
                    .await
                    .unwrap();
                let parts = report_value(&bytes)
                    .array()
                    .unwrap()
                    .iter()
                    .map(|part| part.unwrap().u16().unwrap())
                    .collect::<Vec<_>>();
                assert_eq!(parts, expected);
            }
            for (index, function) in stored.functions.iter().enumerate() {
                let bytes = read_bytes(&handler, &Context::at(&im, function.endpoint, 0x1d, 4))
                    .await
                    .unwrap();
                let list = report_value(&bytes);
                let tag = list.array().unwrap().iter().next().unwrap().unwrap();
                let tag = tag.structure().unwrap();
                assert_eq!(tag.find_ctx(1).unwrap().u8().unwrap(), 7);
                assert_eq!(tag.find_ctx(2).unwrap().u8().unwrap(), index as u8 + 1);
                assert_eq!(tag.find_ctx(3).unwrap().utf8().unwrap(), function.label);
            }
            handler.access(|node| {
                assert!(node.endpoint(3).unwrap().cluster(6).is_none());
                for (index, function) in stored.functions.iter().enumerate() {
                    let endpoint = node.endpoint(function.endpoint).unwrap();
                    assert_eq!(endpoint.device_types.len(), 1);
                    assert_eq!(endpoint.device_types[0].dtype, 0x010a);
                    assert!(endpoint.cluster(0x39).is_none());
                    assert_eq!(endpoint.semantic_tags[0].namespace_id, 7);
                    assert_eq!(endpoint.semantic_tags[0].tag, index as u8 + 1);
                    assert_eq!(
                        endpoint.semantic_tags[0].label,
                        Some(function.label.as_str())
                    );
                    let descriptor = endpoint.cluster(0x1d).unwrap();
                    assert_ne!(
                        descriptor.feature_map
                            & rs_matter::dm::clusters::desc::Feature::TAG_LIST.bits(),
                        0
                    );
                    assert!(descriptor.attribute(4).is_some());
                }
            });
            multi.set_availability(Availability::Available);
            single.set_availability(Availability::Available);
            assert!(poll_once(run.as_mut()).await.is_none());
            assert!(
                report_value(
                    &read_bytes(&handler, &Context::at(&im, 3, 0x39, 17))
                        .await
                        .unwrap()
                )
                .bool()
                .unwrap()
            );
            for (index, function) in stored.functions.iter().enumerate() {
                let mut command = Context::at(&im, function.endpoint, 6, 0);
                command.command.cmd_id = 1;
                assert_eq!(
                    read_bytes(&handler, &command).await.unwrap_err().code(),
                    ErrorCode::Failure
                );
                let mut out = [0; 128];
                let mut invoke = pin!(handler.invoke(
                    &command,
                    InvokeReplyInstance::new(&command.command, WriteBuf::new(&mut out))
                ));
                assert!(poll_once(invoke.as_mut()).await.is_none());
                next_power(&mut receivers[index])
                    .await
                    .unwrap()
                    .accept()
                    .unwrap();
                invoke.await.unwrap();
                assert!(
                    report_value(&read_bytes(&handler, &command).await.unwrap())
                        .bool()
                        .unwrap()
                );
                for other in index + 1..3 {
                    assert_eq!(multi.functions()[other].snapshot().power, Power::Unknown);
                }
                assert_eq!(single.functions()[0].snapshot().power, Power::Unknown);
            }
            let single_context = Context::at(&im, 2, 6, 0);
            let before = read_bytes(&handler, &single_context).await.unwrap_err();
            assert_eq!(before.code(), ErrorCode::Failure);
            single.functions()[0].observe(false);
            assert!(poll_once(run.as_mut()).await.is_none());
            let before = report_dataver(&read_bytes(&handler, &single_context).await.unwrap());
            single.functions()[0].observe(true);
            assert!(poll_once(run.as_mut()).await.is_none());
            assert_eq!(
                report_dataver(&read_bytes(&handler, &single_context).await.unwrap()),
                before.wrapping_add(1)
            );
            // The frozen instance keeps serving the old single even though the
            // committed set changes; the rejoin is served by the next one.
            store
                .devices()
                .set_device_label(&single_stored.key, "Retained name")
                .unwrap();
            super::support::set_published(&store, &single_stored.key, false);
            registry.remove(&single_stored.key);
            let restored = super::support::set_published(&store, &single_stored.key, true);
            assert_eq!(restored.unique_id, single_stored.unique_id);
            assert_eq!(restored.endpoint, 2);
            assert_eq!(restored.label, "Retained name");
        }

        // The rejoin reuses the retained identity, label and endpoint in a new
        // frozen instance; the old receiver is never used again.
        let (rejoined, mut replacement_receivers) = crate::device::PhysicalDevice::new(
            store
                .devices()
                .load_published(&single_stored.key)
                .unwrap()
                .unwrap()
                .description(),
        );
        rejoined.set_availability(Availability::Available);
        rejoined.functions()[0].observe(false);
        registry.insert(rejoined.clone());
        {
            let handler = model::handler(&registry, &store, protocol.clone(), rand::rng()).unwrap();
            let im = InteractionModel::new(&matter, &crypto, &buffers, &handler, &kv, &state);
            let ctx = Context::new(&im, 6, 0);
            let mut run = pin!(handler.run(&ctx));
            assert!(poll_once(run.as_mut()).await.is_none());
            let single_context = Context::at(&im, 2, 6, 0);
            assert!(
                !report_value(&read_bytes(&handler, &single_context).await.unwrap())
                    .bool()
                    .unwrap()
            );
            assert!(
                poll_once(next_power(&mut single_receivers[0]))
                    .await
                    .flatten()
                    .is_none()
            );
            ctx.changes.borrow_mut().clear();
            let rebound_version =
                report_dataver(&read_bytes(&handler, &single_context).await.unwrap());
            let mut command = Context::at(&im, 2, 6, 0);
            command.command.cmd_id = 1;
            let mut out = [0; 128];
            let mut invoke = pin!(handler.invoke(
                &command,
                InvokeReplyInstance::new(&command.command, WriteBuf::new(&mut out))
            ));
            assert!(poll_once(invoke.as_mut()).await.is_none());
            next_power(&mut replacement_receivers[0])
                .await
                .unwrap()
                .accept()
                .unwrap();
            invoke.await.unwrap();
            assert!(poll_once(run.as_mut()).await.is_none());
            assert!(ctx.changes.borrow().contains(&(2, 6, 0)));
            assert_eq!(
                report_dataver(&read_bytes(&handler, &single_context).await.unwrap()),
                rebound_version.wrapping_add(1)
            );
        }

        // Removing the multi is only visible in the next frozen instance.
        super::support::set_published(&store, &stored.key, false);
        registry.remove(&stored.key);
        {
            let handler = model::handler(&registry, &store, protocol.clone(), rand::rng()).unwrap();
            let im = InteractionModel::new(&matter, &crypto, &buffers, &handler, &kv, &state);
            let ctx = Context::new(&im, 6, 0);
            let mut run = pin!(handler.run(&ctx));
            assert!(poll_once(run.as_mut()).await.is_none());
            assert_eq!(
                read_bytes(&handler, &Context::at(&im, 4, 6, 0))
                    .await
                    .unwrap_err()
                    .code(),
                ErrorCode::EndpointNotFound
            );
            assert_eq!(
                handler.access(|node| node
                    .endpoints
                    .iter()
                    .map(|endpoint| endpoint.id)
                    .collect::<Vec<_>>()),
                [0, 1, 2]
            );
        }

        // Many committed devices build one larger but still frozen instance.
        for index in 0..20 {
            let (device, _receivers) = super::physical(&format!("extra-{index}"), false);
            super::support::add_device(&store, &device.description());
            registry.insert(device);
        }
        {
            let handler = model::handler(&registry, &store, protocol.clone(), rand::rng()).unwrap();
            let im = InteractionModel::new(&matter, &crypto, &buffers, &handler, &kv, &state);
            let ctx = Context::new(&im, 6, 0);
            let mut run = pin!(handler.run(&ctx));
            assert!(poll_once(run.as_mut()).await.is_none());
            assert_eq!(handler.access(|node| node.endpoints.len()), 23);
        }
        protocol.check_failure().unwrap();
    });
}

async fn write_label(
    handler: &impl AsyncHandler,
    base: &impl HandlerContext,
    endpoint: u16,
    label: &str,
) -> Result<(), Error> {
    let mut buffer = [0; 128];
    let mut write = WriteBuf::new(&mut buffer);
    write.utf8(&TLVTag::Anonymous, label)?;
    let mut ctx = Context::at(base, endpoint, 0x39, 5);
    ctx.data = TLVElement::new(write.as_slice());
    handler.write(&ctx).await
}
#[test]
fn dynamic_device_names_are_isolated_and_write_failure_is_fatal() {
    block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let identity = store.load_identity().unwrap();
        let info = basic_info(&identity);
        let matter = Matter::new(&info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
        let buffers: MatterBuffers = MatterBuffers::new();
        let state: EthInteractionModelState =
            EthInteractionModelState::new(EthNetwork::new_default());
        let crypto = default_crypto(rand::rng(), DAC_PRIVKEY);
        let protocol = StoreAdapter::new(store.matter());
        let kv = matter.kv(protocol.clone());
        let registry = crate::device::DeviceRegistry::new();
        let (first, _first) = super::physical("first-name", false);
        let (second, _second) = super::physical("second-name", true);
        let first_stored = super::support::add_device(&store, &first.description());
        let second_stored = super::support::add_device(&store, &second.description());
        registry.insert(first.clone());
        registry.insert(second.clone());
        let handler = model::handler(&registry, &store, protocol.clone(), rand::rng()).unwrap();
        let im = InteractionModel::new(&matter, &crypto, &buffers, &handler, &kv, &state);
        write_label(&handler, &im, first_stored.endpoint, "Renamed first")
            .await
            .unwrap();
        assert_eq!(
            store
                .devices()
                .load_published(&first_stored.key)
                .unwrap()
                .unwrap()
                .label,
            "Renamed first"
        );
        assert_eq!(
            store
                .devices()
                .load_published(&second_stored.key)
                .unwrap()
                .unwrap()
                .label,
            second_stored.label
        );
        let bytes = read_bytes(&handler, &Context::at(&im, first_stored.endpoint, 0x39, 5))
            .await
            .unwrap();
        assert_eq!(report_value(&bytes).utf8().unwrap(), "Renamed first");
        assert_eq!(first.label(), "Renamed first");
        assert_eq!(first.description().label, "Renamed first");
        assert_eq!(second.label(), second_stored.label);
        assert_eq!(
            write_label(&handler, &im, first_stored.endpoint, &"界".repeat(11))
                .await
                .unwrap_err()
                .code(),
            ErrorCode::ConstraintError
        );
        assert_eq!(
            store
                .devices()
                .load_published(&first_stored.key)
                .unwrap()
                .unwrap()
                .label,
            "Renamed first"
        );
        write_label(&handler, &im, second_stored.endpoint, "Renamed multi")
            .await
            .unwrap();
        assert_eq!(
            store
                .devices()
                .load_published(&second_stored.key)
                .unwrap()
                .unwrap()
                .label,
            "Renamed multi"
        );
        assert_eq!(
            store
                .devices()
                .load_published(&first_stored.key)
                .unwrap()
                .unwrap()
                .label,
            "Renamed first"
        );
        let db = rusqlite::Connection::open(store.path()).unwrap();
        db.execute_batch("CREATE TRIGGER fail_name_write BEFORE UPDATE OF label ON device_identity BEGIN SELECT RAISE(ABORT, 'name write rejected'); END;").unwrap();
        assert_eq!(
            write_label(&handler, &im, first_stored.endpoint, "Failed name")
                .await
                .unwrap_err()
                .code(),
            ErrorCode::StdIoError
        );
        assert_eq!(first.label(), "Renamed first");
        assert_eq!(first.description().label, "Renamed first");
        assert_eq!(second.label(), "Renamed multi");
        let failure = protocol.check_failure().unwrap_err();
        assert_eq!(failure.operation(), "write device label");
        assert_eq!(failure.path(), store.path());
        assert!(
            std::error::Error::source(&failure)
                .unwrap()
                .is::<rusqlite::Error>()
        );
        assert_eq!(
            protocol.wait_failure().await.operation(),
            "write device label"
        );
        db.execute_batch("DROP TRIGGER fail_name_write;").unwrap();
        assert_eq!(
            write_label(
                &handler,
                &im,
                second_stored.endpoint,
                "Blocked after failure"
            )
            .await
            .unwrap_err()
            .code(),
            ErrorCode::StdIoError
        );
        let bytes = read_bytes(&handler, &Context::at(&im, first_stored.endpoint, 0x39, 5))
            .await
            .unwrap();
        assert_eq!(report_value(&bytes).utf8().unwrap(), "Renamed first");
        assert_eq!(first.label(), "Renamed first");
        assert_eq!(first.description().label, "Renamed first");
        assert_eq!(second.label(), "Renamed multi");
        assert_eq!(
            store
                .devices()
                .load_published(&first_stored.key)
                .unwrap()
                .unwrap()
                .label,
            "Renamed first"
        );
        assert_eq!(
            store
                .devices()
                .load_published(&second_stored.key)
                .unwrap()
                .unwrap()
                .label,
            "Renamed multi"
        );
    });
}

#[path = "lighting_handlers.rs"]
mod lighting_handlers;
