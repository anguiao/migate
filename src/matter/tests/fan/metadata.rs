use super::*;

#[test]
fn global_lists_and_access_match_each_fan_shape() {
    for swing in [false, true] {
        let (device, mut receiver) = fan_device("fan", swing);
        fan_model!(&device, handler, im);
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
            assert_unpublished_surface(
                &handler,
                &im,
                0x202,
                if swing {
                    &[9, 10, 11]
                } else {
                    &[7, 8, 9, 10, 11]
                },
                &[0, 1, 255],
            )
            .await;
            observe_fan_state(&device.functions()[0], true, 40, 1, false);
            assert_fan_mode_constraints(
                &handler,
                &im,
                &mut receiver,
                &device.functions()[0],
                &[5, 7, 255],
            )
            .await;
        });
    }
}

#[test]
fn confirmed_on_without_speed_still_fails_until_observed() {
    let device = fan_device("fan", true).0;
    fan_model!(&device, handler, im);
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
