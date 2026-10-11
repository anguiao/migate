//! Configuration-version and structural-change behavior of the dynamic Matter model.

use super::composition::GroupRig;
use super::handlers::Context;
use super::*;
use crate::device::Availability;
use futures_lite::future::poll_once;
use rs_matter::{
    MATTER_PORT, Matter,
    crypto::default_crypto,
    dm::{
        AsyncHandler, HandlerContext, ReadReplyInstance,
        devices::test::{DAC_PRIVKEY, TEST_DEV_ATT, TEST_DEV_COMM},
        networks::eth::EthNetwork,
    },
    im::{EthInteractionModelState, InteractionModel},
    tlv::{TLVTag, TLVWrite},
    transport::exchange::MatterBuffers,
    utils::storage::WriteBuf,
};
use std::pin::pin;

fn persisted_version(store: &Store) -> Option<u32> {
    let bytes = store.matter().get(BASIC_INFO_KEY).unwrap()?;
    Some(
        rs_matter::dm::clusters::basic_info::BasicInfoSettings::from_tlv(&TLVElement::new(&bytes))
            .unwrap()
            .configuration_version,
    )
}

async fn read_attribute(
    handler: &impl AsyncHandler,
    ctx: &Context<'_, impl HandlerContext>,
) -> Result<Vec<u8>, rs_matter::error::Error> {
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

macro_rules! setup_rig {
    ($rig:expr, $matter:ident, $handler:ident, $im:ident, $kv:ident) => {
        let identity = $rig.store.load_identity().unwrap();
        let info = basic_info(&identity);
        let $matter = Matter::new(&info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
        let buffers: MatterBuffers = MatterBuffers::new();
        let state: EthInteractionModelState =
            EthInteractionModelState::new(EthNetwork::new_default());
        let crypto = default_crypto(rand::rng(), DAC_PRIVKEY);
        let $kv = $matter.kv(StoreAdapter::new($rig.store.matter()));
        $matter.startup(&$kv).unwrap();
        let $handler = $rig.model();
        let $im = InteractionModel::new(&$matter, &crypto, &buffers, &$handler, &$kv, &state);
    };
}

#[test]
fn first_run_establishes_one_persisted_baseline_and_restart_keeps_it() {
    let rig = GroupRig::standalone(&[("ct-one", "yeelink.light.light3", "Ceiling One")]);
    let endpoint = rig.member("ct-one").endpoint;
    {
        setup_rig!(rig, matter, handler, im, kv);
        block_on(async {
            let ctx = Context::at(&im, endpoint, 6, 0);
            let mut run = pin!(handler.run(&ctx));
            assert!(poll_once(run.as_mut()).await.is_none());
            assert_eq!(
                ctx.changes.borrow().clone(),
                vec![(0, 0x28, 0x18)],
                "the only startup notification is the baseline configuration version"
            );
        });
    }
    assert_eq!(
        persisted_version(&rig.store),
        Some(2),
        "a node with no stored shape establishes one baseline version"
    );

    rig.recompose(&["ct-one"]);
    {
        setup_rig!(rig, matter, handler, im, kv);
        block_on(async {
            let ctx = Context::at(&im, endpoint, 6, 0);
            let mut run = pin!(handler.run(&ctx));
            assert!(poll_once(run.as_mut()).await.is_none());
        });
    }
    assert_eq!(persisted_version(&rig.store), Some(3));

    {
        setup_rig!(rig, matter, handler, im, kv);
        block_on(async {
            let ctx = Context::at(&im, endpoint, 6, 0);
            let mut run = pin!(handler.run(&ctx));
            assert!(poll_once(run.as_mut()).await.is_none());
        });
    }
    assert_eq!(
        persisted_version(&rig.store),
        Some(3),
        "an unchanged restart must not bump again"
    );
}

#[test]
fn structural_change_stops_old_model_and_rebuild_uses_latest_registry() {
    let rig = GroupRig::standalone(&[
        ("ct-one", "yeelink.light.light3", "Ceiling One"),
        ("rgb-one", "devcea.light.ls2307", "Bedside One"),
    ]);
    let endpoint = rig.member("ct-one").endpoint;
    let rgb_endpoint = rig.member("rgb-one").endpoint;
    let baseline;
    let parent;
    {
        setup_rig!(rig, matter, handler, im, kv);
        handler.initialize(&im).unwrap();
        baseline = persisted_version(&rig.store).unwrap();
        parent = block_on(async {
            let ctx = Context::at(&im, endpoint, 6, 0);
            let mut run = pin!(handler.run(&ctx));
            assert!(poll_once(run.as_mut()).await.is_none());
            let parent = rig.recompose(&["ct-one"]);
            poll_once(run.as_mut()).await.unwrap().unwrap();
            assert!(handler.needs_rebuild());
            assert!(ctx.changes.borrow().is_empty());
            assert_eq!(persisted_version(&rig.store), Some(baseline));
            handler.access(|node| {
                assert!(node.endpoint(parent).is_none());
                for child in [endpoint, rgb_endpoint] {
                    assert!(node.endpoint(child).unwrap().cluster(0x39).is_some());
                }
            });
            parent
        });
    }
    // Coalesce a second confirmed change before the next service starts.
    rig.recompose(&["ct-one", "rgb-one"]);
    {
        setup_rig!(rig, matter, handler, im, kv);
        handler.initialize(&im).unwrap();
        assert_eq!(persisted_version(&rig.store), Some(baseline + 1));
        handler.access(|node| {
            assert!(node.endpoint(parent).is_some());
            for child in [endpoint, rgb_endpoint] {
                assert!(node.endpoint(child).unwrap().cluster(0x39).is_none());
            }
        });
        handler.initialize(&im).unwrap();
        assert_eq!(persisted_version(&rig.store), Some(baseline + 1));
    }
}

#[test]
fn removed_and_readded_instance_reports_reachability_without_configuration_bump() {
    let mut rig = GroupRig::standalone(&[("ct-one", "yeelink.light.light3", "Ceiling One")]);
    let endpoint = rig.member("ct-one").endpoint;
    rig.member_mut("ct-one")
        .device
        .set_availability(Availability::Available);
    setup_rig!(rig, matter, handler, im, kv);
    block_on(async {
        let ctx = Context::at(&im, endpoint, 0x39, 17);
        let mut run = pin!(handler.run(&ctx));
        assert!(poll_once(run.as_mut()).await.is_none());
        let baseline = persisted_version(&rig.store).expect("baseline version");
        ctx.changes.borrow_mut().clear();

        // Removal and rediscovery before the runner observes them coalesce
        // into a state refresh when the published shape is unchanged.
        let description = rig.member("ct-one").device.description();
        let (replacement, _receiver) = PhysicalDevice::new(description);
        replacement.set_availability(Availability::Unavailable("replaced".into()));
        rig.registry.remove(replacement.key()).unwrap();
        rig.registry.insert(replacement.clone());
        assert!(poll_once(run.as_mut()).await.is_none());
        assert!(!handler.needs_rebuild());

        let changes = ctx.changes.borrow().clone();
        assert!(changes.contains(&(endpoint, 0x39, 17)), "{changes:?}");
        assert!(!changes.contains(&(0, 0x28, 0x18)), "{changes:?}");
        assert!(
            !changes.contains(&(endpoint, 0x1d, u32::MAX)),
            "an unchanged descriptor must not be reported: {changes:?}"
        );
        assert_eq!(persisted_version(&rig.store), Some(baseline));
        assert!(
            !report_value(
                &read_attribute(&handler, &Context::at(&im, endpoint, 0x39, 17))
                    .await
                    .unwrap()
            )
            .bool()
            .unwrap()
        );

        // Coming back reports the new Reachable again, still without a bump.
        ctx.changes.borrow_mut().clear();
        replacement.set_availability(Availability::Available);
        assert!(poll_once(run.as_mut()).await.is_none());
        let changes = ctx.changes.borrow().clone();
        assert!(changes.contains(&(endpoint, 0x39, 17)), "{changes:?}");
        assert_eq!(persisted_version(&rig.store), Some(baseline));
        assert!(
            report_value(
                &read_attribute(&handler, &Context::at(&im, endpoint, 0x39, 17))
                    .await
                    .unwrap()
            )
            .bool()
            .unwrap()
        );
    });
}

#[test]
fn confirmed_role_and_endpoint_changes_bump_once_per_boot_but_reorder_does_not() {
    let rig = GroupRig::new(&[
        ("ct-one", "yeelink.light.light3", "Ceiling One"),
        ("rgb-one", "devcea.light.ls2307", "Bedside One"),
    ]);
    let member = rig.member("rgb-one");
    let mut baseline = 0;
    for boot in 0..5 {
        match boot {
            1 => {
                rig.recompose(&["ct-one"]);
            }
            2 => {
                rig.recompose(&[]);
            }
            3 => {
                rig.registry.remove(&member.key).unwrap();
            }
            4 => {
                rig.registry.insert(member.device.clone());
            }
            _ => {}
        }
        setup_rig!(rig, matter, handler, im, kv);
        handler.initialize(&im).unwrap();
        let version = persisted_version(&rig.store).unwrap();
        if boot == 0 {
            baseline = version;
        }
        assert_eq!(version, baseline + boot);
        block_on(async {
            let ctx = Context::at(&im, member.endpoint, 6, 0);
            let mut run = pin!(handler.run(&ctx));
            assert!(poll_once(run.as_mut()).await.is_none());
            if boot == 0 {
                rig.recompose(&["rgb-one", "ct-one"]);
                assert!(poll_once(run.as_mut()).await.is_none());
                assert!(!handler.needs_rebuild());
                assert_eq!(persisted_version(&rig.store), Some(version));
            }
        });
    }
}

#[test]
fn node_label_does_not_bump_configuration_version() {
    let rig = GroupRig::standalone(&[("ct-one", "yeelink.light.light3", "Ceiling One")]);
    let endpoint = rig.member("ct-one").endpoint;
    setup_rig!(rig, matter, handler, im, kv);
    block_on(async {
        let ctx = Context::at(&im, endpoint, 6, 0);
        let mut run = pin!(handler.run(&ctx));
        assert!(poll_once(run.as_mut()).await.is_none());
        let baseline = persisted_version(&rig.store).expect("baseline version");
        ctx.changes.borrow_mut().clear();

        // NodeLabel is writable and not a fixed-quality attribute.
        ctx.changes.borrow_mut().clear();
        let mut buffer = [0; 128];
        let mut write = WriteBuf::new(&mut buffer);
        write.utf8(&TLVTag::Anonymous, "Renamed").unwrap();
        let mut label = Context::at(&im, endpoint, 0x39, 5);
        label.data = TLVElement::new(write.as_slice());
        handler.write(&label).await.unwrap();
        assert!(poll_once(run.as_mut()).await.is_none());
        assert_eq!(persisted_version(&rig.store), Some(baseline));
        assert!(label.changes.borrow().contains(&(endpoint, 0x39, 5)));
        assert!(!label.changes.borrow().contains(&(0, 0x28, 0x18)));
    });
}

#[test]
fn configuration_version_persistence_failure_prevents_service_start() {
    let rig = GroupRig::standalone(&[("ct-one", "yeelink.light.light3", "Ceiling One")]);
    {
        setup_rig!(rig, matter, handler, im, kv);
        handler.initialize(&im).unwrap();
    }
    let baseline = persisted_version(&rig.store).unwrap();
    rig.recompose(&["ct-one"]);
    let db = rusqlite::Connection::open(rig.store.path()).unwrap();
    db.execute_batch(
        "CREATE TRIGGER block_blob_insert BEFORE INSERT ON blobs BEGIN SELECT RAISE(ABORT, 'blocked'); END;
         CREATE TRIGGER block_blob_update BEFORE UPDATE ON blobs BEGIN SELECT RAISE(ABORT, 'blocked'); END;",
    ).unwrap();
    drop(db);
    setup_rig!(rig, matter, handler, im, kv);
    assert!(handler.initialize(&im).is_err());
    assert_eq!(persisted_version(&rig.store), Some(baseline));
}
