//! Shared physical battery attributes and reports.

use super::*;

#[test]
fn battery_capability_adds_power_source_to_the_parent_only() {
    setup!(device, handler, _im, true);
    handler.access(|node| {
        let parent = node
            .endpoints
            .iter()
            .find(|endpoint| endpoint.id == 2)
            .unwrap();
        assert_eq!(
            parent
                .device_types
                .iter()
                .map(|kind| (kind.dtype, kind.drev))
                .collect::<Vec<_>>(),
            [(0x0013, 1), (0x0011, 1)]
        );
        let power = parent
            .clusters
            .iter()
            .find(|cluster| cluster.id == 0x002f)
            .unwrap();
        assert_eq!(power.revision, 3);
        assert_eq!(power.feature_map, 0x02);
        for attribute in [
            0, 1, 2, 12, 14, 15, 16, 31, 0xfff8, 0xfff9, 0xfffb, 0xfffc, 0xfffd,
        ] {
            assert!(
                power.attribute(attribute).is_some(),
                "attribute {attribute}"
            );
        }
        for attribute in [
            3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30,
        ] {
            assert!(
                power.attribute(attribute).is_none(),
                "unexpected {attribute}"
            );
        }
        assert!(power.command(0).is_none());
        for event in [0, 1, 2] {
            assert!(power.event(event).is_none(), "unexpected event {event}");
        }
        for endpoint_id in [3, 4] {
            let endpoint = node
                .endpoints
                .iter()
                .find(|endpoint| endpoint.id == endpoint_id)
                .unwrap();
            assert!(!endpoint.clusters.iter().any(|cluster| cluster.id == 0x002f));
            assert!(
                !endpoint
                    .device_types
                    .iter()
                    .any(|kind| kind.dtype == 0x0011)
            );
        }
        assert!(!node.endpoints.iter().any(|endpoint| endpoint.id > 4));
    });
}

#[test]
fn without_battery_the_parent_has_no_power_source_cluster() {
    setup!(device, handler, _im, false);
    handler.access(|node| {
        let parent = node
            .endpoints
            .iter()
            .find(|endpoint| endpoint.id == 2)
            .unwrap();
        assert_eq!(
            parent
                .device_types
                .iter()
                .map(|kind| (kind.dtype, kind.drev))
                .collect::<Vec<_>>(),
            [(0x0013, 1)]
        );
        assert!(!parent.clusters.iter().any(|cluster| cluster.id == 0x002f));
    });
}

#[test]
fn battery_attributes_are_nullable_shared_and_fail_before_the_first_value() {
    setup!(device, handler, im, true);
    block_on(async {
        let ctx = Context::at(&im, 2, 0x002f, 12);
        assert!(
            report_value(&read_bytes(&handler, &ctx).await.unwrap())
                .null()
                .is_ok()
        );
        for attribute in [14, 15] {
            let ctx = Context::at(&im, 2, 0x002f, attribute);
            let mut buffer = [0; 4096];
            let mut write = rs_matter::utils::storage::WriteBuf::new(&mut buffer);
            let error = handler
                .read(
                    &ctx,
                    rs_matter::dm::ReadReplyInstance::new(&ctx.attribute, &mut write),
                )
                .await
                .unwrap_err();
            assert_eq!(error.code(), ErrorCode::Failure, "attribute {attribute}");
        }
        for (attribute, expected) in [(0, 0), (1, 0), (16, 0)] {
            let ctx = Context::at(&im, 2, 0x002f, attribute);
            assert_eq!(
                report_value(&read_bytes(&handler, &ctx).await.unwrap())
                    .u8()
                    .unwrap(),
                expected,
                "attribute {attribute}"
            );
        }
        let ctx = Context::at(&im, 2, 0x002f, 2);
        assert_eq!(
            report_value(&read_bytes(&handler, &ctx).await.unwrap())
                .utf8()
                .unwrap(),
            "Battery"
        );
        device.observe_battery(55.0, Some(1), 2).unwrap();
        let ctx = Context::at(&im, 2, 0x002f, 12);
        assert_eq!(
            report_value(&read_bytes(&handler, &ctx).await.unwrap())
                .u8()
                .unwrap(),
            110
        );
        let ctx = Context::at(&im, 2, 0x002f, 14);
        assert_eq!(
            report_value(&read_bytes(&handler, &ctx).await.unwrap())
                .u8()
                .unwrap(),
            0
        );
        let ctx = Context::at(&im, 2, 0x002f, 15);
        assert!(
            !report_value(&read_bytes(&handler, &ctx).await.unwrap())
                .bool()
                .unwrap()
        );
        let ctx = Context::at(&im, 2, 0x002f, 31);
        assert_eq!(
            report_value(&read_bytes(&handler, &ctx).await.unwrap())
                .array()
                .unwrap()
                .iter()
                .map(|value| value.unwrap().u16().unwrap())
                .collect::<Vec<_>>(),
            [2, 3, 4]
        );
    });
}

#[test]
fn battery_thresholds_and_adjacent_percentages_share_one_value() {
    setup!(device, handler, im, true);
    block_on(async {
        for (percent, doubled, level, replacement) in [
            (0.0, 0, 2, true),
            (1.0, 2, 2, true),
            (4.0, 8, 2, true),
            (5.0, 10, 2, true),
            (6.0, 12, 1, false),
            (9.0, 18, 1, false),
            (10.0, 20, 1, false),
            (11.0, 22, 0, false),
            (100.0, 200, 0, false),
        ] {
            device
                .observe_battery(percent, Some(percent as i64), 1)
                .unwrap();
            for (attribute, expected) in [(12, doubled), (14, level)] {
                let ctx = Context::at(&im, 2, 0x002f, attribute);
                assert_eq!(
                    report_value(&read_bytes(&handler, &ctx).await.unwrap())
                        .u8()
                        .unwrap(),
                    expected,
                    "{percent}% attribute {attribute}"
                );
            }
            let ctx = Context::at(&im, 2, 0x002f, 15);
            assert_eq!(
                report_value(&read_bytes(&handler, &ctx).await.unwrap())
                    .bool()
                    .unwrap(),
                replacement,
                "{percent}%"
            );
        }
    });
}

#[test]
fn battery_attributes_are_read_only() {
    setup!(device, handler, im, true);
    block_on(async {
        device.observe_battery(55.0, Some(1), 2).unwrap();
        for attribute in [0, 1, 2, 12, 14, 15, 16, 31] {
            let mut ctx = Context::at(&im, 2, 0x002f, attribute);
            ctx.data = TLVElement::new(&[0x15, 0x18]);
            let error = handler.write(&ctx).await.unwrap_err();
            assert_eq!(
                error.code(),
                ErrorCode::AttributeNotFound,
                "attribute {attribute}"
            );
        }
    });
}

#[test]
fn battery_changes_bump_versions_and_notify_projected_attribute_edges_only() {
    setup!(device, handler, im, true);
    block_on(async {
        let ctx = Context::at(&im, 2, 0x002f, 12);
        let mut run = pin!(handler.run(&ctx));
        assert!(poll_once(run.as_mut()).await.is_none());
        ctx.changes.borrow_mut().clear();

        device.observe_battery(10.0, Some(1), 2).unwrap();
        assert!(poll_once(run.as_mut()).await.is_none());
        for attribute in [12, 14, 15] {
            assert!(ctx.changes.borrow().contains(&(2, 0x002f, attribute)));
        }
        let version = {
            let bytes = read_bytes(&handler, &ctx).await.unwrap();
            report_dataver(&bytes)
        };
        let count = ctx.changes.borrow().len();
        // A timestamp-only update must not fabricate a report or bump dataver.
        device.observe_battery(10.0, Some(2), 3).unwrap();
        assert!(poll_once(run.as_mut()).await.is_none());
        assert_eq!(ctx.changes.borrow().len(), count);
        assert_eq!(
            report_dataver(&read_bytes(&handler, &ctx).await.unwrap()),
            version
        );

        ctx.changes.borrow_mut().clear();
        device.observe_battery(5.0, Some(3), 4).unwrap();
        assert!(poll_once(run.as_mut()).await.is_none());
        for attribute in [12, 14, 15] {
            assert!(ctx.changes.borrow().contains(&(2, 0x002f, attribute)));
        }
        assert_ne!(
            report_dataver(&read_bytes(&handler, &ctx).await.unwrap()),
            version
        );
    });
}

#[test]
fn multiple_sensor_parents_keep_isolated_battery_state_and_endpoint_lists() {
    let dir = tempfile::tempdir().unwrap();
    let store = crate::storage::Store::open(dir.path()).unwrap();
    let identity = store.load_identity().unwrap();
    let info = basic_info(&identity);
    let matter = Matter::new(&info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
    let buffers: MatterBuffers = MatterBuffers::new();
    let state: EthInteractionModelState = EthInteractionModelState::new(EthNetwork::new_default());
    let crypto = default_crypto(rand::rng(), DAC_PRIVKEY);
    let kv = matter.kv(StoreAdapter::new(store.matter()));
    let first = battery_sensor_named("one", "Sensor One", true);
    let second = battery_sensor_named("two", "Sensor Two", true);
    let handler = model_handler(vec![
        entry(&first, 2, &[("temperature", 3), ("humidity", 4)]),
        entry(&second, 5, &[("temperature", 6), ("humidity", 7)]),
    ]);
    let im = InteractionModel::new(&matter, &crypto, &buffers, &handler, &kv, &state);
    block_on(async {
        first.observe_battery(11.0, Some(1), 2).unwrap();
        for (endpoint, expected) in [(2, [2, 3, 4]), (5, [5, 6, 7])] {
            let ctx = Context::at(&im, endpoint, 0x002f, 31);
            assert_eq!(
                report_value(&read_bytes(&handler, &ctx).await.unwrap())
                    .array()
                    .unwrap()
                    .iter()
                    .map(|value| value.unwrap().u16().unwrap())
                    .collect::<Vec<_>>(),
                expected
            );
            let ctx = Context::at(&im, endpoint, 0x002f, 12);
            let bytes = read_bytes(&handler, &ctx).await.unwrap();
            let value = report_value(&bytes);
            if endpoint == 2 {
                assert_eq!(value.u8().unwrap(), 22);
            } else {
                assert!(value.null().is_ok());
            }
        }
    });
}
