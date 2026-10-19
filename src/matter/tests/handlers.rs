use super::super::{bridged_info::BridgedHandler, model::basic_info, storage::StoreAdapter};
use super::support::{
    Context, entry, model_handler, next_power, read_bytes, report_dataver, report_value,
    single_entry,
};
use crate::{
    app::App,
    device::{Availability, CommandError, Power},
    storage::Store,
    xiaomi::adapter::tests::app_source::AppSource,
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
use std::{pin::pin, rc::Rc};

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
    let handler = crate::matter::tests::support::model_handler(vec![
        crate::matter::tests::support::single_entry(&device),
    ]);
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
            device.functions()[0].observe(power);
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
    let source = AppSource::new(&store, Some(super::test_device().0.description()));
    let app = Rc::new(App::open(source.handle.clone(), store.clone()).unwrap());
    let device = source.physical.as_ref().unwrap();
    let info = basic_info(&identity);
    let matter = Matter::new(&info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
    let buffers: MatterBuffers = MatterBuffers::new();
    let state: EthInteractionModelState = EthInteractionModelState::new(EthNetwork::new_default());
    let crypto = default_crypto(rand::rng(), DAC_PRIVKEY);
    let kv = matter.kv(StoreAdapter::new(store.matter()));
    let entry = app.frozen().remove(0);
    let stored = app
        .current()
        .configuration
        .device(device.key())
        .unwrap()
        .clone();
    let bridged = BridgedHandler::new(
        device.clone(),
        Dataver::new(1),
        &entry.identity,
        app.clone(),
    );
    let handler = crate::matter::model::handler_from(vec![entry], rand::rng(), app.clone());
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
            .load_configuration()
            .unwrap()
            .device(device.key())
            .cloned()
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
        .load_configuration()
        .unwrap()
        .device(device.key())
        .cloned()
        .unwrap();
    assert_eq!(renamed.unique_id, stored.unique_id);
    assert_eq!(renamed.endpoint, stored.endpoint);
}

#[test]
fn frozen_topology_and_replacement_instances_keep_independent_channels() {
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

        // An empty publication serves only the root and aggregator.
        {
            let handler = model_handler(vec![]);
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

        let (single, mut single_receivers) = super::physical("single", false);
        let (multi, mut receivers) = super::physical("multi", true);
        let multi_entry = entry(&multi, 3, &[("outlet_1", 4), ("outlet_2", 5), ("usb", 6)]);
        let stored = multi_entry.identity.clone();
        {
            let handler = model_handler(vec![single_entry(&single), multi_entry.clone()]);
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
                assert_eq!(
                    tag.find_ctx(3).unwrap().utf8().unwrap(),
                    multi.function(&function.id).unwrap().description().label
                );
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
                        Some(
                            multi
                                .function(&function.id)
                                .unwrap()
                                .description()
                                .label
                                .as_str()
                        )
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
                multi.functions()[index].observe(true);
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
        }

        // A replacement uses the same explicit identity with a new command channel.
        single.close();
        let (rejoined, mut replacement_receivers) =
            crate::device::PhysicalDevice::new(single.description());
        rejoined.set_availability(Availability::Available);
        rejoined.functions()[0].observe(false);
        {
            let handler = model_handler(vec![single_entry(&rejoined), multi_entry]);
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
            rejoined.functions()[0].observe(true);
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

        // A new publication omits the multi-function endpoint.
        {
            let handler = model_handler(vec![single_entry(&rejoined)]);
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

        let mut entries = vec![single_entry(&rejoined)];
        for endpoint in 7..27 {
            let (device, _receivers) = super::physical(&format!("extra-{endpoint}"), false);
            entries.push(entry(&device, endpoint, &[("main", endpoint)]));
        }
        {
            let handler = model_handler(entries);
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
fn dynamic_device_names_are_isolated_and_write_failure_preserves_names() {
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
        let source = AppSource::new(
            &store,
            Some(super::physical("first-name", false).0.description()),
        );
        let app = Rc::new(App::open(source.handle.clone(), store.clone()).unwrap());
        let second_key = crate::device::DeviceKey::new("xiaomi", "second-name");
        source.describe(super::physical("second-name", true).0.description());
        app.apply(app.prepare(app.plan().unwrap().unwrap()))
            .unwrap();
        let first = source.physical.as_ref().unwrap();
        let second = app.current().published.get(&second_key).unwrap();
        let first_stored = app
            .current()
            .configuration
            .device(first.key())
            .unwrap()
            .clone();
        let second_stored = app
            .current()
            .configuration
            .device(&second_key)
            .unwrap()
            .clone();
        let handler = crate::matter::model::handler_from(app.frozen(), rand::rng(), app.clone());
        let im = InteractionModel::new(&matter, &crypto, &buffers, &handler, &kv, &state);
        write_label(&handler, &im, first_stored.endpoint, "Renamed first")
            .await
            .unwrap();
        assert_eq!(
            store
                .devices()
                .load_configuration()
                .unwrap()
                .device(&first_stored.key)
                .cloned()
                .unwrap()
                .label,
            "Renamed first"
        );
        assert_eq!(
            store
                .devices()
                .load_configuration()
                .unwrap()
                .device(&second_stored.key)
                .cloned()
                .unwrap()
                .label,
            second_stored.label
        );
        let bytes = read_bytes(&handler, &Context::at(&im, first_stored.endpoint, 0x39, 5))
            .await
            .unwrap();
        assert_eq!(report_value(&bytes).utf8().unwrap(), "Renamed first");
        assert_eq!(first.label(), "Renamed first");
        assert_eq!(first.description().label(), "Renamed first");
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
                .load_configuration()
                .unwrap()
                .device(&first_stored.key)
                .cloned()
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
                .load_configuration()
                .unwrap()
                .device(&second_stored.key)
                .cloned()
                .unwrap()
                .label,
            "Renamed multi"
        );
        assert_eq!(
            store
                .devices()
                .load_configuration()
                .unwrap()
                .device(&first_stored.key)
                .cloned()
                .unwrap()
                .label,
            "Renamed first"
        );
        let db = rusqlite::Connection::open(store.path()).unwrap();
        db.execute_batch("CREATE TRIGGER fail_name_write BEFORE UPDATE ON bridge_configuration BEGIN SELECT RAISE(ABORT, 'name write rejected'); END;").unwrap();
        assert_eq!(
            write_label(&handler, &im, first_stored.endpoint, "Failed name")
                .await
                .unwrap_err()
                .code(),
            ErrorCode::StdIoError
        );
        assert_eq!(first.label(), "Renamed first");
        assert_eq!(first.description().label(), "Renamed first");
        assert_eq!(second.label(), "Renamed multi");
        // A failed configuration transaction keeps the last complete names and
        // does not poison the protocol store watcher.
        protocol.check_failure().unwrap();
        db.execute_batch("DROP TRIGGER fail_name_write;").unwrap();
        write_label(
            &handler,
            &im,
            second_stored.endpoint,
            "Blocked after failure",
        )
        .await
        .unwrap();
        let bytes = read_bytes(&handler, &Context::at(&im, first_stored.endpoint, 0x39, 5))
            .await
            .unwrap();
        assert_eq!(report_value(&bytes).utf8().unwrap(), "Renamed first");
        assert_eq!(first.label(), "Renamed first");
        assert_eq!(first.description().label(), "Renamed first");
        assert_eq!(second.label(), "Blocked after failure");
        assert_eq!(
            store
                .devices()
                .load_configuration()
                .unwrap()
                .device(&first_stored.key)
                .cloned()
                .unwrap()
                .label,
            "Renamed first"
        );
        assert_eq!(
            store
                .devices()
                .load_configuration()
                .unwrap()
                .device(&second_stored.key)
                .cloned()
                .unwrap()
                .label,
            "Blocked after failure"
        );
    });
}
