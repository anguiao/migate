use super::*;

#[test]
fn curtain_publishes_only_supported_window_covering_elements() {
    let (device, _) = curtain();
    let model = model_handler(vec![single_entry(&device)]);
    model.access(|node| {
        let endpoint = node.endpoint(2).unwrap();
        assert!(
            endpoint
                .device_types
                .iter()
                .any(|d| d.dtype == 0x0202 && d.drev == 6)
        );
        let parent = node.endpoint(3).unwrap();
        assert!(parent.device_types.iter().any(|d| d.dtype == 0x0013));
        assert!(parent.cluster(0x39).is_some());
        assert!(parent.cluster(0x102).is_none());
        assert_eq!(
            endpoint.clusters.iter().map(|c| c.id).collect::<Vec<_>>(),
            [0x1d, 4, 3, 0x102]
        );
        let cluster = endpoint.cluster(0x102).unwrap();
        assert_eq!(cluster.revision, 8);
        assert_eq!(cluster.feature_map, 5);
        for id in [
            0, 7, 8, 10, 11, 13, 14, 23, 0xfff8, 0xfff9, 0xfffb, 0xfffc, 0xfffd,
        ] {
            assert!(cluster.attribute(id).is_some(), "missing attribute {id}");
        }
        for id in [1, 2, 3, 4, 5, 6, 9, 12, 15, 16, 17, 18, 19, 26] {
            assert!(cluster.attribute(id).is_none(), "unexpected attribute {id}");
        }
        for id in [0, 1, 2, 5] {
            assert!(cluster.command(id).is_some(), "missing command {id}");
        }
        for id in [4, 7, 8] {
            assert!(cluster.command(id).is_none(), "unexpected command {id}");
        }
    });
}

#[test]
fn curtain_protocol_lists_match_published_metadata() {
    setup!(device, receiver, handler, ctx);
    let _ = (&device, &mut receiver);
    block_on(async {
        for (attribute, expected) in [
            (0xfff8, vec![]),
            (0xfff9, vec![0, 1, 2, 5]),
            (
                0xfffb,
                vec![
                    0, 7, 8, 10, 11, 13, 14, 23, 0xfff8, 0xfff9, 0xfffb, 0xfffc, 0xfffd,
                ],
            ),
        ] {
            ctx.attribute = attr(0x102, attribute);
            let bytes = read_bytes(&handler, &ctx).await.unwrap();
            let values = report_value(&bytes)
                .array()
                .unwrap()
                .iter()
                .map(|item| item.unwrap().u32().unwrap())
                .collect::<Vec<_>>();
            assert_eq!(values, expected);
        }
        ctx.attribute = attr(0x102, 0xfffc);
        assert_eq!(
            report_value(&read_bytes(&handler, &ctx).await.unwrap())
                .u32()
                .unwrap(),
            5
        );
        ctx.attribute = attr(0x102, 0xfffd);
        assert_eq!(read_u16(&handler, &ctx).await, 8);
        for command in [4, 7, 8] {
            ctx.command.cmd_id = command;
            assert_eq!(
                handler
                    .invoke(
                        &ctx,
                        InvokeReplyInstance::new(&ctx.command, WriteBuf::new(&mut [0; 128]))
                    )
                    .await
                    .unwrap_err()
                    .code(),
                ErrorCode::CommandNotFound
            );
        }
    });
}
