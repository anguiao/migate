use super::*;

#[test]
fn total_control_refresh_reads_mixed_values_without_inference_and_coalesces() {
    block_on(Rig::new(true, &["multi"]).run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        let control = &c;
        c.peer(peer, |peer| async move {
            initialized(control, &peer, &["multi"]).await;
            let context = &control.context;
            peer.notify("multi", 4, true);
            until(|| function(context, "multi", "outlet:4").snapshot().power() == Some(Power::On))
                .await;
            let before = peer.count("get", "multi", 3);
            peer.state.borrow_mut().hold_reads = true;
            peer.notify("multi", 2, true);
            until(|| {
                peer.count("get", "multi", 3) == before + 1
                    && peer
                        .state
                        .borrow()
                        .held
                        .iter()
                        .any(|(p, _)| decode_mips(&p.payload).unwrap().payload["siid"] == 3)
            })
            .await;
            assert_eq!(
                function(context, "multi", "outlet:3").snapshot().power(),
                Some(Power::Off)
            );
            assert_eq!(
                function(context, "multi", "outlet:4").snapshot().power(),
                Some(Power::On)
            );
            assert_eq!(
                function(context, "multi", "outlet:9").snapshot().power(),
                Some(Power::Off)
            );
            peer.notify("multi", 2, false);
            peer.notify("multi", 2, true);
            peer.notify("multi", 2, false);
            peer.release("multi", 3, json!({"value":true})).await;
            until(|| {
                peer.state
                    .borrow()
                    .held
                    .iter()
                    .any(|(p, _)| decode_mips(&p.payload).unwrap().payload["siid"] == 4)
            })
            .await;
            peer.release("multi", 4, json!({"value":false})).await;
            until(|| {
                peer.state
                    .borrow()
                    .held
                    .iter()
                    .any(|(p, _)| decode_mips(&p.payload).unwrap().payload["siid"] == 9)
            })
            .await;
            peer.release("multi", 9, json!({"value":true})).await;
            until(|| {
                peer.count("get", "multi", 3) == before + 2
                    && peer
                        .state
                        .borrow()
                        .held
                        .iter()
                        .any(|(p, _)| decode_mips(&p.payload).unwrap().payload["siid"] == 3)
            })
            .await;
            peer.state.borrow_mut().hold_reads = false;
            peer.release("multi", 3, json!({"value":true})).await;
            until(|| {
                control.context.directory.borrow().devices["multi"]
                    .work
                    .as_ref()
                    .unwrap()
                    .pending_problems()
                    .is_empty()
                    && function(context, "multi", "outlet:3").snapshot().power() == Some(Power::On)
            })
            .await;
            control.clock.advance(Duration::from_secs(1));
            for _ in 0..30 {
                future::yield_now().await;
            }
            // Only the in-flight property needs a follow-up; unsent properties were coalesced.
            assert_eq!(peer.count("get", "multi", 3), before + 2);
            assert_eq!(peer.count("get", "multi", 4), before + 1);
            assert_eq!(peer.count("get", "multi", 9), before + 1);
            assert_eq!(
                function(context, "multi", "outlet:9").snapshot().power(),
                Some(Power::On)
            );
            assert_eq!(
                function(context, "multi", "outlet:3").snapshot().power(),
                Some(Power::On)
            );
            assert_eq!(
                function(context, "multi", "outlet:4").snapshot().power(),
                Some(Power::Off)
            );
            assert_eq!(
                context
                    .physical(&DeviceKey::new("xiaomi", "multi"))
                    .unwrap()
                    .functions()
                    .len(),
                3
            );
        })
        .await;
    }));
}
