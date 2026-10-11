use super::super::controllers::hs;
use super::*;
use c::ClusterAsyncHandler as _;

#[derive(Clone, Copy)]
enum FinalOutcome {
    Accepted,
    Rejected,
    Stopped,
}

fn check_hue_254_endpoint(outcome: FinalOutcome) {
    setup!(true, function, receiver, light, clock, ctx);
    function
        .observe_light(LightPatch {
            power: Some(true),
            color: Some(hs::hs_to_rgb(hs::Hs {
                hue: 240.0,
                saturation: 254.0,
            })),
            mode: Some(LightMode::Rgb),
            ..Default::default()
        })
        .unwrap();
    block_on(async {
        let handler = ColorHandler::<true, true>(&light);
        // MoveToHue(254, Up, 1 second). The encoded endpoint 254 and
        // canonical RGB red (hue 0) are physically equivalent.
        let data = [
            0x15, 0x24, 0, 254, 0x24, 1, 2, 0x25, 2, 10, 0, 0x24, 3, 0, 0x24, 4, 0, 0x18,
        ];
        let mut invoke = pin!(
            handler.handle_move_to_hue(&ctx, c::MoveToHueRequest::new(TLVElement::new(&data)))
        );
        assert!(poll_once(invoke.as_mut()).await.is_none());
        let startup = frame(&mut receiver).await;
        let token = startup.token;
        accept(&function, startup);
        invoke.await.unwrap();

        clock.set(500);
        light.advance();
        accept(&function, frame(&mut receiver).await);
        assert_eq!(handler.current_hue(&ctx).await.unwrap(), 247);

        clock.set(1000);
        light.advance();
        let final_frame = frame(&mut receiver).await;
        assert_eq!(final_frame.token, token);
        assert!(final_frame.final_frame);
        let red = Rgb::from_packed(0xff0000).unwrap();
        assert_eq!(final_frame.target.color, Some(LightColorTarget::Rgb(red)));
        final_frame.mark_sent().unwrap();

        // A valid device observation can precede the final receipt. It
        // establishes red, but not successful completion of this HS command.
        function
            .observe_light(LightPatch {
                color: Some(red),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(handler.current_hue(&ctx).await.unwrap(), 0);
        match outcome {
            FinalOutcome::Accepted => {
                final_frame.accept().unwrap();
            }
            FinalOutcome::Rejected => {
                final_frame.reject(CommandError::Rejected("final write rejected".into()));
            }
            FinalOutcome::Stopped => {
                light.stop(LightDimension::Color).unwrap();
                // The transport receipt still completes its original frame.
                final_frame.accept().unwrap();
            }
        }
        assert!(!function.light_action_current(token));
        let expected = if matches!(outcome, FinalOutcome::Accepted) {
            254
        } else {
            0
        };
        assert_eq!(handler.current_hue(&ctx).await.unwrap(), expected);
        light.advance();
        assert_eq!(handler.current_hue(&ctx).await.unwrap(), expected);
        assert_eq!(handler.current_saturation(&ctx).await.unwrap(), 254);
        assert!(poll_once(receiver.next_operation()).await.is_none());
    });
}

#[test]
fn finite_hue_254_endpoint_is_retained_only_after_final_success() {
    check_hue_254_endpoint(FinalOutcome::Accepted);
}

#[test]
fn failed_final_hue_frame_cannot_commit_the_254_endpoint() {
    check_hue_254_endpoint(FinalOutcome::Rejected);
}

#[test]
fn stopped_final_hue_frame_cannot_commit_the_254_endpoint_on_late_success() {
    check_hue_254_endpoint(FinalOutcome::Stopped);
}
