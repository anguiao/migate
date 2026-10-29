use super::*;
use rs_matter::{dm::InvokeReplyInstance, tlv::TLVElement, utils::storage::WriteBuf};

#[test]
fn global_lists_and_access_match_each_fan_shape() {
    for swing in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let identity = store.load_identity().unwrap();
        let device = fan(swing);
        let handler = model_handler(vec![single_entry(&device)]);
        let info = basic_info(&identity);
        let matter = Matter::new(&info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
        let buffers: MatterBuffers = MatterBuffers::new();
        let state: InteractionModelState = InteractionModelState::new();
        let crypto = default_crypto(rand::rng(), DAC_PRIVKEY);
        let kv = matter.kv(StoreAdapter::new(store.matter()));
        let im = InteractionModel::new(&matter, &crypto, &buffers, &handler, &kv, &state);
        let mut ctx = Context::new(&im, 0x202, 0);
        block_on(async {
            let mut attributes = (0..=6).collect::<Vec<_>>();
            if swing {
                attributes.extend([7, 8]);
            }
            attributes.extend([0xfff8, 0xfff9, 0xfffb, 0xfffc, 0xfffd]);
            for (id, expected) in [(0xfff8, vec![]), (0xfff9, vec![]), (0xfffb, attributes)] {
                ctx.attribute = attr(0x202, id);
                let bytes = read_bytes(&handler, &ctx).await.unwrap();
                let actual = report_value(&bytes)
                    .array()
                    .unwrap()
                    .iter()
                    .map(|value| value.unwrap().u32().unwrap())
                    .collect::<Vec<_>>();
                assert_eq!(actual, expected);
            }
            ctx.attribute = attr(0x202, 0xfffc);
            assert_eq!(
                report_value(&read_bytes(&handler, &ctx).await.unwrap())
                    .u32()
                    .unwrap(),
                if swing { 5 } else { 1 }
            );
            ctx.attribute = attr(0x202, 0xfffd);
            assert_eq!(
                report_value(&read_bytes(&handler, &ctx).await.unwrap())
                    .u16()
                    .unwrap(),
                6
            );
            for id in if swing {
                vec![9, 10, 11]
            } else {
                vec![7, 8, 9, 10, 11]
            } {
                ctx.attribute = attr(0x202, id);
                assert_eq!(
                    read_bytes(&handler, &ctx).await.unwrap_err().code(),
                    ErrorCode::AttributeNotFound
                );
                ctx.data = TLVElement::new(&[0x04, 1]);
                assert_eq!(
                    handler.write(&ctx).await.unwrap_err().code(),
                    ErrorCode::AttributeNotFound
                );
            }
            for id in [0, 1, 255] {
                ctx.command.cmd_id = id;
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
}

#[test]
fn confirmed_on_without_speed_still_fails_until_observed() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let identity = store.load_identity().unwrap();
    let device = fan(true);
    let handler = model_handler(vec![single_entry(&device)]);
    let info = basic_info(&identity);
    let matter = Matter::new(&info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
    let buffers: MatterBuffers = MatterBuffers::new();
    let state: InteractionModelState = InteractionModelState::new();
    let crypto = default_crypto(rand::rng(), DAC_PRIVKEY);
    let kv = matter.kv(StoreAdapter::new(store.matter()));
    let im = InteractionModel::new(&matter, &crypto, &buffers, &handler, &kv, &state);
    let function = &device.functions()[0];
    function.observe_fan(FanValue::Power(true), Some(1), 1);
    function.observe_fan(FanValue::Swing(true), Some(1), 1);
    block_on(async {
        for id in [0, 2, 3, 5, 6] {
            let ctx = Context::new(&im, 0x202, id);
            assert_eq!(
                read_bytes(&handler, &ctx).await.unwrap_err().code(),
                ErrorCode::Failure
            );
        }
        let swing = Context::new(&im, 0x202, 8);
        assert_eq!(
            report_value(&read_bytes(&handler, &swing).await.unwrap())
                .u8()
                .unwrap(),
            1
        );
        function.observe_fan(FanValue::Power(false), Some(2), 2);
        for id in [0, 2, 3, 5, 6] {
            let ctx = Context::new(&im, 0x202, id);
            assert_eq!(
                report_value(&read_bytes(&handler, &ctx).await.unwrap())
                    .u8()
                    .unwrap(),
                0
            );
        }
        assert_eq!(function.snapshot().fan().unwrap().speed.value, None);
    });
}
