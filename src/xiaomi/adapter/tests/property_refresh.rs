use super::*;

#[test]
fn total_control_refresh_reads_mixed_values_without_inference_and_coalesces() {
    block_on(Rig::new(true, &["multi"]).run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        let control = &c;
        c.peer(peer, |peer| async move {
            initialized(control, &peer, &["multi"]).await;
            let registry = &control.context.registry;
            peer.notify("multi", 4, true);
            until(|| function(registry, "multi", "outlet_2").snapshot().power == Power::On).await;
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
                function(registry, "multi", "outlet_1").snapshot().power,
                Power::Off
            );
            assert_eq!(
                function(registry, "multi", "outlet_2").snapshot().power,
                Power::On
            );
            assert_eq!(
                function(registry, "multi", "usb").snapshot().power,
                Power::Off
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
                control
                    .context
                    .live
                    .borrow()
                    .session
                    .as_ref()
                    .unwrap()
                    .devices["multi"]
                    .diagnostic()
                    .is_none()
                    && function(registry, "multi", "outlet_1").snapshot().power == Power::On
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
                function(registry, "multi", "usb").snapshot().power,
                Power::On
            );
            assert_eq!(
                function(registry, "multi", "outlet_1").snapshot().power,
                Power::On
            );
            assert_eq!(
                function(registry, "multi", "outlet_2").snapshot().power,
                Power::Off
            );
            assert_eq!(
                registry
                    .get(&DeviceKey::new("xiaomi", "multi"))
                    .unwrap()
                    .functions()
                    .len(),
                3
            );
        })
        .await;
    }));
}
