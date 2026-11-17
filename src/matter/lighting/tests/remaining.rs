use super::*;
use l::ClusterAsyncHandler as _;

#[test]
fn remaining_reads_follow_elapsed_time_and_takeover_without_a_frame_tick() {
    setup!(false, function, receiver, light, clock, ctx);
    function
        .observe_light(LightPatch {
            power: Some(true),
            brightness_percent: Some(50.0),
            ..Default::default()
        })
        .unwrap();
    block_on(async {
        let handler = LevelHandler(&light);
        let bytes = [
            0x15, 0x24, 0, 200, 0x25, 1, 20, 0, 0x24, 2, 0, 0x24, 3, 0, 0x18,
        ];
        let mut invoke = pin!(
            handler.handle_move_to_level(&ctx, l::MoveToLevelRequest::new(TLVElement::new(&bytes)))
        );
        assert!(poll_once(invoke.as_mut()).await.is_none());
        accept(&function, frame(&mut receiver).await);
        invoke.await.unwrap();
        let started_version = handler.dataver();
        assert_eq!(handler.remaining_time(&ctx).await.unwrap(), 20);

        // Attribute reads use the action's clock even while the frame runner
        // has not been polled. Reading progress never submits a device frame.
        clock.set(501);
        assert_eq!(handler.remaining_time(&ctx).await.unwrap(), 15);
        assert_ne!(handler.dataver(), started_version);
        assert!(poll_once(receiver.next_operation()).await.is_none());

        function
            .take_over_light(LightDimension::Brightness)
            .unwrap();
        assert_eq!(handler.remaining_time(&ctx).await.unwrap(), 0);
        assert!(collect_reports(&light).contains(&(8, 1)));
        assert!(poll_once(receiver.next_operation()).await.is_none());
    });
}
