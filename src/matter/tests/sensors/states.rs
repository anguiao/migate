//! Binary-state metadata, unknown handling, polarity and semantic reports.
use super::*;
use crate::device::StateInput;

fn state_device(kinds: &[(&str, FunctionKind)], battery: bool) -> PhysicalDevice {
    let description = PhysicalDeviceDescription::new(
        DeviceKey::new("xiaomi", "state"),
        "State sensor",
        kinds
            .iter()
            .map(|(id, kind)| FunctionDescription {
                id: (*id).into(),
                label: (*id).into(),
                kind: *kind,
            })
            .collect(),
    )
    .unwrap();
    let (physical, receivers) = PhysicalDevice::new(if battery {
        description.with_battery()
    } else {
        description
    });
    assert!(receivers.is_empty());
    physical
}

#[test]
fn state_clusters_publish_exact_types_features_and_read_only_attribute_surface() {
    setup!(@device device, handler, im, state_device(&[("contact", FunctionKind::Contact),("motion", FunctionKind::Motion),("occupancy", FunctionKind::Occupancy)],true));
    handler.access(|node| {
        assert_eq!(node.endpoints.len(), 6);
        for (id, dtype, revision, cluster, cluster_revision, features, attributes) in [
            (
                3,
                0x15,
                2,
                0x45,
                3,
                0,
                vec![0, 0xfff8, 0xfff9, 0xfffb, 0xfffc, 0xfffd],
            ),
            (
                4,
                0x107,
                4,
                0x406,
                7,
                1,
                vec![0, 1, 2, 0xfff8, 0xfff9, 0xfffb, 0xfffc, 0xfffd],
            ),
            (
                5,
                0x107,
                4,
                0x406,
                7,
                1,
                vec![0, 1, 2, 0xfff8, 0xfff9, 0xfffb, 0xfffc, 0xfffd],
            ),
        ] {
            let endpoint = node
                .endpoints
                .iter()
                .find(|endpoint| endpoint.id == id)
                .unwrap();
            assert_eq!(endpoint.device_types[0].dtype, dtype);
            assert_eq!(endpoint.device_types[0].drev, revision);
            assert!(endpoint.clusters.iter().any(|cluster| cluster.id == 3));
            assert!(!endpoint.clusters.iter().any(|cluster| cluster.id == 6));
            let cluster = endpoint
                .clusters
                .iter()
                .find(|value| value.id == cluster)
                .unwrap();
            assert_eq!(cluster.revision, cluster_revision);
            assert_eq!(cluster.feature_map, features);
            assert_eq!(
                cluster
                    .attributes
                    .iter()
                    .filter(|attribute| cluster.attribute(attribute.id).is_some())
                    .map(|attribute| attribute.id)
                    .collect::<Vec<_>>(),
                attributes
            );
            assert_eq!(
                cluster
                    .commands
                    .iter()
                    .filter(|command| cluster.command(command.id).is_some())
                    .count(),
                0
            );
            assert_eq!(
                cluster
                    .events
                    .iter()
                    .filter(|event| cluster.event(event.id).is_some())
                    .count(),
                0
            );
        }
    });
    block_on(async {
        for (cluster, attribute, expected) in
            [(0x1d, 3, vec![3, 4, 5]), (0x2f, 31, vec![2, 3, 4, 5])]
        {
            let ctx = Context::at(&im, 2, cluster, attribute);
            assert_eq!(
                report_value(&read_bytes(&handler, &ctx).await.unwrap())
                    .array()
                    .unwrap()
                    .iter()
                    .map(|value| value.unwrap().u16().unwrap())
                    .collect::<Vec<_>>(),
                expected
            );
        }
        for (endpoint, cluster) in [(3, 0x45), (4, 0x406), (5, 0x406)] {
            let mut ctx = Context::at(&im, endpoint, cluster, 0);
            assert_eq!(
                read_bytes(&handler, &ctx).await.unwrap_err().code(),
                ErrorCode::Failure
            );
            ctx.data = TLVElement::new(&[0x15, 0x18]);
            assert_eq!(
                handler.write(&ctx).await.unwrap_err().code(),
                ErrorCode::AttributeNotFound
            );
        }
        for endpoint in [4, 5] {
            for (attribute, expected) in [(1, 0), (2, 1)] {
                let ctx = Context::at(&im, endpoint, 0x406, attribute);
                assert_eq!(
                    report_value(&read_bytes(&handler, &ctx).await.unwrap())
                        .u8()
                        .unwrap(),
                    expected
                );
            }
        }
        device.functions()[0]
            .observe_state(
                Some(false),
                StateInput::Property(serde_json::json!(true)),
                Some(1),
                2,
            )
            .unwrap();
        let ctx = Context::at(&im, 3, 0x45, 0);
        assert!(
            report_value(&read_bytes(&handler, &ctx).await.unwrap())
                .bool()
                .unwrap()
        );
        device.functions()[0]
            .observe_state(
                Some(true),
                StateInput::Property(serde_json::json!(false)),
                Some(3),
                4,
            )
            .unwrap();
        assert!(
            !report_value(&read_bytes(&handler, &ctx).await.unwrap())
                .bool()
                .unwrap()
        );
    });
}

#[test]
fn state_semantic_edges_report_unknown_and_known_without_native_or_timestamp_false_edges() {
    setup!(@device device, handler, im, state_device(&[("motion",FunctionKind::Motion)],false));
    block_on(async {
        let function = &device.functions()[0];
        let ctx = Context::at(&im, 3, 0x406, 0);
        let mut run = pin!(handler.run(&ctx));
        assert!(poll_once(run.as_mut()).await.is_none());
        function
            .observe_state(
                Some(true),
                StateInput::Event {
                    siid: 2,
                    eiid: 1008,
                },
                None,
                1,
            )
            .unwrap();
        assert!(poll_once(run.as_mut()).await.is_none());
        assert_eq!(&*ctx.changes.borrow(), &[(3, 0x406, 0)]);
        let version = report_dataver(&read_bytes(&handler, &ctx).await.unwrap());
        for (input, time) in [
            (
                StateInput::Event {
                    siid: 2,
                    eiid: 1008,
                },
                2,
            ),
            (StateInput::Property(serde_json::json!(0)), 3),
        ] {
            function
                .observe_state(Some(true), input, None, time)
                .unwrap();
            assert!(poll_once(run.as_mut()).await.is_none());
            assert_eq!(ctx.changes.borrow().len(), 1);
            assert_eq!(
                report_dataver(&read_bytes(&handler, &ctx).await.unwrap()),
                version
            );
        }
        function
            .observe_state(
                Some(false),
                StateInput::Property(serde_json::json!(120)),
                Some(4),
                4,
            )
            .unwrap();
        assert!(poll_once(run.as_mut()).await.is_none());
        assert_eq!(ctx.changes.borrow().len(), 2);
        let version = report_dataver(&read_bytes(&handler, &ctx).await.unwrap());
        function
            .observe_state(
                Some(false),
                StateInput::Property(serde_json::json!(300)),
                Some(5),
                5,
            )
            .unwrap();
        assert!(poll_once(run.as_mut()).await.is_none());
        assert_eq!(ctx.changes.borrow().len(), 2);
        assert_eq!(
            report_dataver(&read_bytes(&handler, &ctx).await.unwrap()),
            version
        );
        function
            .observe_state(None, StateInput::Property(serde_json::json!(2)), None, 6)
            .unwrap();
        assert!(poll_once(run.as_mut()).await.is_none());
        assert_eq!(ctx.changes.borrow().len(), 3);
        assert_eq!(
            read_bytes(&handler, &ctx).await.unwrap_err().code(),
            ErrorCode::Failure
        );
        device.set_availability(Availability::Unavailable("lost source".into()));
        assert!(poll_once(run.as_mut()).await.is_none());
        assert_eq!(
            ctx.changes
                .borrow()
                .iter()
                .filter(|(_, cluster, _)| *cluster == 0x406)
                .count(),
            3
        );
        assert!(
            ctx.events
                .borrow()
                .iter()
                .all(|(_, cluster, _)| *cluster != 0x406)
        );
    });
}
