//! Configuration versions across Matter rebuilds and unchanged publication.

use super::composition::GroupRig;
use super::handlers::Context;
use super::*;
use futures_lite::future::poll_once;
use rs_matter::{
    MATTER_PORT, Matter,
    crypto::default_crypto,
    dm::{
        AsyncHandler, HandlerContext,
        clusters::decl::bridged_device_basic_information,
        devices::test::{DAC_PRIVKEY, TEST_DEV_ATT, TEST_DEV_COMM},
        networks::eth::EthNetwork,
    },
    im::{EthInteractionModelState, InteractionModel},
    tlv::{TLVElement, TLVTag, TLVWrite},
    transport::exchange::MatterBuffers,
    utils::storage::WriteBuf,
};

fn persisted_version(store: &Store) -> Option<u32> {
    let bytes = store.matter().get(BASIC_INFO_KEY).unwrap()?;
    Some(
        rs_matter::dm::clusters::basic_info::BasicInfoSettings::from_tlv(&TLVElement::new(&bytes))
            .unwrap()
            .configuration_version,
    )
}

async fn endpoint_version(
    handler: &impl AsyncHandler,
    base: &impl HandlerContext,
    endpoint: u16,
) -> u32 {
    report_value(
        &read_bytes(
            handler,
            &Context::at(
                base,
                endpoint,
                0x39,
                bridged_device_basic_information::AttributeId::ConfigurationVersion as _,
            ),
        )
        .await
        .unwrap(),
    )
    .u32()
    .unwrap()
}

macro_rules! setup_rig {
    ($rig:expr, $matter:ident, $handler:ident, $im:ident, $kv:ident) => {
        setup_rig!($rig, $matter, $handler, $im, $kv, None);
    };
    ($rig:expr, $matter:ident, $handler:ident, $im:ident, $kv:ident, $retry:expr) => {
        let identity = $rig.store.load_identity().unwrap();
        let info = basic_info(&identity);
        let $matter = Matter::new(&info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
        let buffers: MatterBuffers = MatterBuffers::new();
        let state: EthInteractionModelState =
            EthInteractionModelState::new(EthNetwork::new_default());
        let crypto = default_crypto(rand::rng(), DAC_PRIVKEY);
        let $kv = $matter.kv(StoreAdapter::new($rig.store.matter()));
        $matter.startup(&$kv).unwrap();
        let $handler = $rig.model_with_retry($retry);
        let $im = InteractionModel::new(&$matter, &crypto, &buffers, &$handler, &$kv, &state);
    };
}

// Handler tests explicitly retire the old model before installing a new shape.
macro_rules! rebuild_rig {
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
        setup_rig!($rig, $matter, $handler, $im, $kv);
        $handler.initialize(&$im).unwrap();
        let $ctx = Context::at(&$im, $endpoint, 6, 0);
        let mut $run = Box::pin($handler.run(&$ctx));
        assert!(poll_once($run.as_mut()).await.is_none());
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
            let mut run = Box::pin(handler.run(&ctx));
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

    {
        setup_rig!(rig, matter, handler, im, kv);
        block_on(async {
            let ctx = Context::at(&im, endpoint, 6, 0);
            let mut run = Box::pin(handler.run(&ctx));
            assert!(poll_once(run.as_mut()).await.is_none());
        });
    }
    assert_eq!(
        persisted_version(&rig.store),
        Some(2),
        "an unchanged restart must not bump again"
    );
}

#[test]
fn structural_changes_rebuild_and_persist_one_version_each() {
    let rig = GroupRig::standalone(&[
        ("ct-one", "yeelink.light.light3", "Ceiling One"),
        ("rgb-one", "devcea.light.ls2307", "Bedside One"),
    ]);
    let ct = rig.member("ct-one").endpoint;
    let rgb = rig.member("rgb-one").endpoint;
    block_on(async {
        setup_rig!(rig, matter, handler, im, kv);
        handler.initialize(&im).unwrap();
        let baseline = persisted_version(&rig.store).unwrap();
        let member_versions = [
            (ct, endpoint_version(&handler, &im, ct).await),
            (rgb, endpoint_version(&handler, &im, rgb).await),
        ];
        let ctx = Context::at(&im, ct, 6, 0);
        let mut run = Box::pin(handler.run(&ctx));
        assert!(poll_once(run.as_mut()).await.is_none());
        ctx.changes.borrow_mut().clear();

        // The old model finishes before the complete group shape is installed.
        rig.stage(&["ct-one"]);
        rebuild_rig!(rig, matter, handler, im, kv, ctx, run, 0);
        let parent = rig.group_endpoint(1);
        assert_eq!(persisted_version(&rig.store), Some(baseline + 1));
        handler.access(|node| {
            assert!(node.endpoint(parent).is_some());
            assert!(node.endpoint(ct).is_none());
            assert!(node.endpoint(rgb).is_some());
        });
        ctx.changes.borrow_mut().clear();

        // Adding a member changes the derived shape and bumps again.
        rig.stage(&["ct-one", "rgb-one"]);
        rebuild_rig!(rig, matter, handler, im, kv, ctx, run, parent);
        assert_eq!(rig.group_endpoint(1), parent);
        assert_eq!(persisted_version(&rig.store), Some(baseline + 2));
        handler.access(|node| {
            assert!(node.endpoint(ct).is_none());
            assert!(node.endpoint(rgb).is_none());
        });
        ctx.changes.borrow_mut().clear();

        // Dissolving the group restores both reserved standalone endpoints.
        rig.stage(&[]);
        rebuild_rig!(rig, matter, handler, im, kv, ctx, run, ct);
        assert_eq!(persisted_version(&rig.store), Some(baseline + 3));
        handler.access(|node| {
            assert!(node.endpoint(parent).is_none());
            assert!(node.endpoint(ct).is_some());
            assert!(node.endpoint(rgb).is_some());
        });
        for (endpoint, previous) in member_versions {
            let expected = previous + 1;
            assert_eq!(
                rig.store.devices().configuration_version(endpoint).unwrap(),
                Some(expected)
            );
            assert_eq!(endpoint_version(&handler, &im, endpoint).await, expected);
        }
        drop(run);
        drop(ctx);
        drop(im);
        drop(handler);
        drop(kv);
        drop(matter);
        setup_rig!(rig, matter, handler, im, kv);
        handler.initialize(&im).unwrap();
        assert_eq!(persisted_version(&rig.store), Some(baseline + 3));
        for (endpoint, previous) in member_versions {
            assert_eq!(
                endpoint_version(&handler, &im, endpoint).await,
                previous + 1
            );
        }
    });
}

#[test]
fn reorder_keeps_versions_and_member_leave_bumps_only_the_endpoint_set() {
    let rig = GroupRig::standalone(&[
        ("ct-a", "yeelink.light.light3", "A"),
        ("ct-b", "yeelink.light.light3", "B"),
    ]);
    let endpoint = rig.member("ct-a").endpoint;
    let removed_endpoint = rig.member("ct-b").endpoint;
    block_on(async {
        setup_rig!(rig, matter, handler, im, kv);
        handler.initialize(&im).unwrap();
        let baseline = persisted_version(&rig.store).unwrap();
        let ctx = Context::at(&im, endpoint, 6, 0);
        let mut run = Box::pin(handler.run(&ctx));
        assert!(poll_once(run.as_mut()).await.is_none());
        rig.stage(&["ct-a", "ct-b"]);
        rebuild_rig!(rig, matter, handler, im, kv, ctx, run, 0);
        let parent = rig.group_endpoint(1);
        assert_eq!(persisted_version(&rig.store), Some(baseline + 1));
        let first = endpoint_version(&handler, &im, parent).await;
        ctx.changes.borrow_mut().clear();

        // Pure reorder keeps the fixed shape, the Control and every version.
        rig.stage(&["ct-b", "ct-a"]);
        assert!(poll_once(run.as_mut()).await.is_none());
        assert_eq!(persisted_version(&rig.store), Some(baseline + 1));
        assert!(!ctx.changes.borrow().contains(&(0, 0x28, 0x18)));

        // The member leaves the group and becomes a published standalone
        // endpoint again: the actual endpoint set changes, so the root version
        // must bump, but the group's own fixed shape does not.
        rig.stage(&["ct-a"]);
        rebuild_rig!(rig, matter, handler, im, kv, ctx, run, parent);
        assert_eq!(persisted_version(&rig.store), Some(baseline + 2));
        assert_eq!(endpoint_version(&handler, &im, parent).await, first);
        handler.access(|node| {
            assert!(node.endpoint(removed_endpoint).is_some());
        });
    });
}

#[test]
fn sharing_members_between_two_groups_rebuilds_without_changing_versions() {
    let rig = GroupRig::standalone(&[
        ("ct-a", "yeelink.light.light3", "A"),
        ("ct-b", "yeelink.light.light3", "B"),
        ("ct-c", "yeelink.light.light3", "C"),
        ("ct-d", "yeelink.light.light3", "D"),
    ]);
    block_on(async {
        setup_rig!(rig, matter, handler, im, kv);
        handler.initialize(&im).unwrap();
        let baseline = persisted_version(&rig.store).unwrap();
        let ctx = Context::at(&im, rig.member("ct-a").endpoint, 6, 0);
        let mut run = Box::pin(handler.run(&ctx));
        assert!(poll_once(run.as_mut()).await.is_none());
        rig.stage_groups(&[("One", &["ct-a", "ct-b"])]);
        rebuild_rig!(rig, matter, handler, im, kv, ctx, run, 0);
        let first = rig.group_endpoint(1);
        assert_eq!(persisted_version(&rig.store), Some(baseline + 1));
        rig.stage_groups(&[("One", &["ct-a", "ct-b"]), ("Two", &["ct-c", "ct-d"])]);
        rebuild_rig!(rig, matter, handler, im, kv, ctx, run, 0);
        let groups = [rig.group_endpoint(1), rig.group_endpoint(2)];
        assert_eq!(persisted_version(&rig.store), Some(baseline + 2));
        let versions = [
            endpoint_version(&handler, &im, groups[0]).await,
            endpoint_version(&handler, &im, groups[1]).await,
        ];
        assert_eq!(groups[0], first);
        ctx.changes.borrow_mut().clear();

        // Both groups keep their endpoints and derived shape while starting
        // to share a member. The relation still requires a rebuild.
        rig.stage_groups(&[
            ("One", &["ct-a", "ct-c"]),
            ("Two", &["ct-b", "ct-c", "ct-d"]),
        ]);
        let applied = rig.registry.snapshot().applied;
        assert_eq!(
            applied.groups[0].members,
            [
                DeviceKey::new("xiaomi", "ct-a"),
                DeviceKey::new("xiaomi", "ct-b")
            ]
        );
        rebuild_rig!(rig, matter, handler, im, kv, ctx, run, groups[0]);
        assert_eq!(
            rig.registry.snapshot().applied.groups[0].members,
            [
                DeviceKey::new("xiaomi", "ct-a"),
                DeviceKey::new("xiaomi", "ct-c")
            ]
        );
        assert_eq!(persisted_version(&rig.store), Some(baseline + 2));
        assert_eq!(
            endpoint_version(&handler, &im, groups[0]).await,
            versions[0]
        );
        assert_eq!(
            endpoint_version(&handler, &im, groups[1]).await,
            versions[1]
        );
        assert!(!ctx.changes.borrow().contains(&(0, 0x28, 0x18)));
        rig.stage_groups(&[
            ("One", &["ct-c", "ct-a"]),
            ("Two", &["ct-d", "ct-c", "ct-b"]),
        ]);
        assert!(poll_once(run.as_mut()).await.is_none());
        assert!(handler.take_rebuild().is_none());
        assert_eq!(persisted_version(&rig.store), Some(baseline + 2));
    });
}

#[test]
fn rebuild_commits_only_the_latest_confirmed_relation() {
    let rig = GroupRig::standalone(&[
        ("ct-one", "yeelink.light.light3", "Ceiling One"),
        ("rgb-one", "devcea.light.ls2307", "Bedside One"),
    ]);
    let ct = rig.member("ct-one").endpoint;
    let rgb = rig.member("rgb-one").endpoint;
    block_on(async {
        setup_rig!(rig, matter, handler, im, kv);
        handler.initialize(&im).unwrap();
        let baseline = persisted_version(&rig.store).unwrap();
        let ctx = Context::at(&im, ct, 6, 0);
        let mut run = Box::pin(handler.run(&ctx));
        assert!(poll_once(run.as_mut()).await.is_none());
        rig.stage(&["ct-one"]);
        poll_once(run.as_mut()).await.unwrap().unwrap();
        let rebuild = handler.take_rebuild().unwrap();
        // The latest confirmation wins even when it arrives during shutdown.
        rig.stage(&["rgb-one"]);
        drop(run);
        drop(ctx);
        drop(im);
        drop(handler);
        drop(kv);
        drop(matter);
        assert!(rebuild.apply().is_none());
        let parent = rig.group_endpoint(1);
        setup_rig!(rig, matter, handler, im, kv);
        handler.initialize(&im).unwrap();
        assert_eq!(persisted_version(&rig.store), Some(baseline + 1));
        assert_eq!(
            rig.registry.snapshot().applied.groups[0].members,
            [DeviceKey::new("xiaomi", "rgb-one")]
        );
        assert!(rig.registry.pending_configuration().is_none());
        handler.access(|node| {
            assert!(node.endpoint(ct).is_some());
            assert!(node.endpoint(rgb).is_none());
            assert_light_surface(
                node.endpoint(parent).unwrap(),
                true,
                true,
                true,
                true,
                (true, true, true, true),
            );
        });
    });
}

#[test]
fn node_label_does_not_bump_configuration_version() {
    let rig = GroupRig::standalone(&[("ct-one", "yeelink.light.light3", "Ceiling One")]);
    let endpoint = rig.member("ct-one").endpoint;
    setup_rig!(rig, matter, handler, im, kv);
    block_on(async {
        let ctx = Context::at(&im, endpoint, 6, 0);
        let mut run = Box::pin(handler.run(&ctx));
        assert!(poll_once(run.as_mut()).await.is_none());
        let baseline = persisted_version(&rig.store).expect("baseline version");
        ctx.changes.borrow_mut().clear();

        // NodeLabel is writable and not a fixed-quality attribute.
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
fn configuration_version_persistence_failure_preserves_stored_and_memory_versions() {
    let rig = GroupRig::new(&[
        ("ct-one", "yeelink.light.light3", "Ceiling One"),
        ("rgb-one", "devcea.light.ls2307", "Bedside One"),
    ]);
    let parent = rig.parent;
    {
        setup_rig!(rig, matter, handler, im, kv);
        handler.initialize(&im).unwrap();
    }
    let baseline = persisted_version(&rig.store).unwrap();
    let stored = rig
        .store
        .devices()
        .configuration_version(parent)
        .unwrap()
        .expect("baseline endpoint version");
    // Leaving only the RGB member changes the group's fixed shape (ColorRgb).
    rig.recompose(&["rgb-one"]);
    let db = rusqlite::Connection::open(rig.store.path()).unwrap();
    db.execute_batch(
        "CREATE TRIGGER block_blob_insert BEFORE INSERT ON blobs BEGIN SELECT RAISE(ABORT, 'blocked'); END;
         CREATE TRIGGER block_blob_update BEFORE UPDATE ON blobs BEGIN SELECT RAISE(ABORT, 'blocked'); END;",
    )
    .unwrap();
    drop(db);
    setup_rig!(rig, matter, handler, im, kv);
    assert!(handler.initialize(&im).is_err());
    assert_eq!(persisted_version(&rig.store), Some(baseline));
    // The failed transaction must not have moved the in-memory BDBI version
    // either; the published attribute still reads the last committed value.
    assert_eq!(block_on(endpoint_version(&handler, &im, parent)), stored);
    assert_eq!(
        rig.store.devices().configuration_version(parent).unwrap(),
        Some(stored)
    );
    // Once storage recovers, the same handler retries the baseline commit.
    let db = rusqlite::Connection::open(rig.store.path()).unwrap();
    db.execute_batch(
        "DROP TRIGGER block_blob_insert;
         DROP TRIGGER block_blob_update;",
    )
    .unwrap();
    drop(db);
    handler.initialize(&im).unwrap();
    assert_eq!(persisted_version(&rig.store), Some(baseline + 1));
    assert_eq!(
        block_on(endpoint_version(&handler, &im, parent)),
        stored + 1
    );
}

#[test]
fn failed_rebuild_commit_restores_old_configuration_and_retries() {
    for removing in [false, true] {
        let rig = GroupRig::standalone(&[("ct-one", "yeelink.light.light3", "Ceiling One")]);
        let endpoint = rig.member("ct-one").endpoint;
        let identity = rig.store.load_identity().unwrap();
        block_on(async {
            setup_rig!(rig, matter, handler, im, kv);
            handler.initialize(&im).unwrap();
            let baseline = persisted_version(&rig.store).unwrap();
            let ctx = Context::at(&im, endpoint, 6, 0);
            let mut run = Box::pin(handler.run(&ctx));
            assert!(poll_once(run.as_mut()).await.is_none());
            if removing {
                let key = rig.member("ct-one").key.clone();
                rig.registry.revoke(&key);
                let scope = rig.scope();
                rig.registry
                    .stage_configuration(
                        crate::device::ConfigurationScope {
                            uid: scope.uid,
                            home_id: scope.home_id,
                            group_id: scope.group_id,
                        },
                        Vec::new(),
                        vec![key],
                    )
                    .unwrap();
            } else {
                rig.stage(&["ct-one"]);
            }
            poll_once(run.as_mut()).await.unwrap().unwrap();
            let rebuild = handler.take_rebuild().unwrap();
            assert_eq!(persisted_version(&rig.store), Some(baseline));
            handler.access(|node| {
                assert!(node.endpoint(endpoint).is_some());
                assert_eq!(node.endpoints.len(), 3);
            });
            drop(run);
            drop(ctx);
            drop(im);
            drop(handler);
            drop(kv);
            drop(matter);

            let db = rusqlite::Connection::open(rig.store.path()).unwrap();
            db.execute_batch(
                "CREATE TRIGGER block_version BEFORE INSERT ON blobs
             BEGIN SELECT RAISE(ABORT, 'blocked'); END;",
            )
            .unwrap();
            let retry = rebuild
                .apply()
                .expect("the failed transaction must be retried");
            assert!(rig.registry.apply_error().is_some());
            assert!(rig.registry.pending_configuration().is_some());
            assert_eq!(persisted_version(&rig.store), Some(baseline));
            assert!(
                rig.store
                    .devices()
                    .load_group_identity(&DeviceKey::new("xiaomi", "group-1"))
                    .unwrap()
                    .is_none()
            );

            setup_rig!(rig, matter, handler, im, kv, Some(retry));
            let ctx = Context::at(&im, endpoint, 6, 0);
            let mut run = Box::pin(handler.run(&ctx));
            assert!(poll_once(run.as_mut()).await.is_none());
            assert!(
                handler.take_rebuild().is_none(),
                "the retry delay avoids restart loops"
            );
            handler.access(|node| {
                assert!(node.endpoint(endpoint).is_some());
                assert_eq!(node.endpoints.len(), 3);
            });
            if removing {
                // Recover only the closed publication handle, never the source or control path.
                assert!(rig.registry.get(&rig.member("ct-one").key).is_none());
                assert!(
                    !report_value(
                        &read_bytes(&handler, &Context::at(&im, endpoint, 0x39, 17))
                            .await
                            .unwrap()
                    )
                    .bool()
                    .unwrap()
                );
                let mut command = Context::at(&im, endpoint, 6, 0);
                command.command.cmd_id = 1;
                command.data = TLVElement::new(&[0x15, 0x18]);
                let mut buffer = [0; 128];
                let error = handler
                    .invoke(
                        &command,
                        rs_matter::dm::InvokeReplyInstance::new(
                            &command.command,
                            WriteBuf::new(&mut buffer),
                        ),
                    )
                    .await
                    .unwrap_err();
                assert_eq!(error.code(), rs_matter::error::ErrorCode::Failure);
            }
            db.execute_batch("DROP TRIGGER block_version;").unwrap();
            async_io::Timer::after(std::time::Duration::from_millis(1100)).await;
            rebuild_rig!(rig, matter, handler, im, kv, ctx, run, endpoint);
            assert_eq!(persisted_version(&rig.store), Some(baseline + 1));
            assert!(rig.registry.apply_error().is_none());
            assert!(rig.registry.pending_configuration().is_none());
            assert_eq!(rig.store.load_identity().unwrap(), identity);
            handler.access(|node| {
                assert!(node.endpoint(endpoint).is_none());
                if removing {
                    assert_eq!(node.endpoints.len(), 2);
                } else {
                    assert!(node.endpoint(rig.group_endpoint(1)).is_some());
                }
            });
        });
    }
}

#[test]
fn applied_group_without_a_stored_identity_fails_the_restore_boundary() {
    let rig = GroupRig::standalone(&[("ct-one", "yeelink.light.light3", "Ceiling One")]);
    let key = rig.member("ct-one").key.clone();
    rig.registry
        .apply_configuration(
            0,
            vec![crate::device::GroupConfiguration {
                key: DeviceKey::new("xiaomi", "group-missing"),
                label: "Missing".into(),
                members: vec![key],
                capabilities: vec![Capability::OnOff],
            }],
            &[],
            &[],
        )
        .unwrap();
    let error = match model::handler(
        &rig.registry,
        &rig.store,
        StoreAdapter::new(rig.store.matter()),
        crate::matter::tests::support::test_executor(),
        rand::rng(),
        None,
    ) {
        Ok(_) => panic!("an applied group without identity must fail the restore boundary"),
        Err(error) => error,
    };
    assert_eq!(error.operation(), "build Matter topology");
}

#[test]
fn reappearing_group_identity_bumps_its_endpoint_configuration_version() {
    let rig = GroupRig::standalone(&[
        ("ct-one", "yeelink.light.light3", "Ceiling One"),
        ("rgb-one", "devcea.light.ls2307", "Bedside One"),
    ]);
    let key = DeviceKey::new("xiaomi", "group-1");
    let ct_endpoint = rig.member("ct-one").endpoint;
    block_on(async {
        setup_rig!(rig, matter, handler, im, kv);
        handler.initialize(&im).unwrap();
        let baseline = persisted_version(&rig.store).unwrap();
        let ctx = Context::at(&im, ct_endpoint, 6, 0);
        let mut run = Box::pin(handler.run(&ctx));
        assert!(poll_once(run.as_mut()).await.is_none());

        // 1. The group is published as a CT-only endpoint.
        rig.stage(&["ct-one"]);
        rebuild_rig!(rig, matter, handler, im, kv, ctx, run, 0);
        let parent = rig.group_endpoint(1);
        assert_eq!(persisted_version(&rig.store), Some(baseline + 1));
        let ct_version = endpoint_version(&handler, &im, parent).await;
        assert!(ct_version >= 1);
        handler.access(|node| {
            assert_light_surface(
                node.endpoint(parent).unwrap(),
                true,
                true,
                false,
                true,
                (true, true, true, false),
            );
        });
        let first_identity = rig
            .store
            .devices()
            .load_group_identity(&key)
            .unwrap()
            .unwrap();

        // 2. The group is revoked. Its identity, endpoint and
        //    ConfigurationVersion stay reserved even though the endpoint
        //    disappears from the published surface.
        rig.stage(&[]);
        rebuild_rig!(rig, matter, handler, im, kv, ctx, run, ct_endpoint);
        assert_eq!(persisted_version(&rig.store), Some(baseline + 2));
        assert_eq!(
            rig.store.devices().configuration_version(parent).unwrap(),
            Some(ct_version)
        );

        // 3. The same DID reappears at the same reserved endpoint with a
        //    different fixed shape (RGB now present). The endpoint version
        //    must increase instead of silently reusing the revoked shape's
        //    version.
        rig.stage(&["rgb-one"]);
        rebuild_rig!(rig, matter, handler, im, kv, ctx, run, parent);
        assert_eq!(rig.group_endpoint(1), parent);
        assert_eq!(persisted_version(&rig.store), Some(baseline + 3));
        let second_identity = rig
            .store
            .devices()
            .load_group_identity(&key)
            .unwrap()
            .unwrap();
        assert_eq!(second_identity.unique_id, first_identity.unique_id);
        assert_eq!(second_identity.endpoint, first_identity.endpoint);
        assert_eq!(
            endpoint_version(&handler, &im, parent).await,
            ct_version + 1
        );
        handler.access(|node| {
            assert_light_surface(
                node.endpoint(parent).unwrap(),
                true,
                true,
                true,
                true,
                (true, true, true, true),
            );
        });

        // 4. Rebuilding from the same final applied configuration does not
        //    bump the stored or in-memory versions again.
        {
            setup_rig!(rig, matter, handler, im, kv);
            handler.initialize(&im).unwrap();
            assert_eq!(persisted_version(&rig.store), Some(baseline + 3));
            assert_eq!(
                endpoint_version(&handler, &im, parent).await,
                ct_version + 1
            );
            let ctx = Context::at(&im, ct_endpoint, 6, 0);
            let mut run = Box::pin(handler.run(&ctx));
            assert!(poll_once(run.as_mut()).await.is_none());
            assert_eq!(persisted_version(&rig.store), Some(baseline + 3));
            assert_eq!(
                endpoint_version(&handler, &im, parent).await,
                ct_version + 1
            );
        }
    });
}
