use super::*;

#[test]
fn subscription_and_read_retries_are_bounded_and_later_events_restore_them() {
    for timeout in [false, true] {
        block_on(Rig::new(true, &["one", "multi"]).run(|c| async move {
            c.announce();
            let peer = Rc::new(Peer::default());
            let sub = ("multi".into(), 9, 1);
            let read = ("one".into(), 2, 1);
            if timeout {
                peer.state.borrow_mut().sub_timeout.insert(sub.clone());
                peer.state.borrow_mut().read_timeout.insert(read.clone());
            } else {
                peer.state.borrow_mut().sub_reject.insert(sub.clone());
                peer.state.borrow_mut().read_fail.insert(read.clone());
            }
            let control = &c;
            c.peer(peer, |peer| async move {
                until(|| {
                    control
                        .context
                        .status
                        .borrow()
                        .pending
                        .iter()
                        .any(|pending| pending.did == "one" && pending.reason.contains("read"))
                        && control
                            .context
                            .status
                            .borrow()
                            .pending
                            .iter()
                            .any(|pending| {
                                pending.did == "multi" && pending.reason.contains("subscribe")
                            })
                })
                .await;
                assert!(
                    control
                        .context
                        .status
                        .borrow()
                        .pending
                        .iter()
                        .any(|pending| pending.did == "one" && pending.reason.contains("read"))
                );
                assert!(
                    control
                        .context
                        .status
                        .borrow()
                        .pending
                        .iter()
                        .any(|pending| pending.did == "multi"
                            && pending.reason.contains("subscribe"))
                );
                assert_eq!(
                    function(&control.context.registry, "one", "main")
                        .snapshot()
                        .power,
                    Power::Unknown
                );
                assert_eq!(
                    function(&control.context.registry, "one", "main")
                        .snapshot()
                        .availability,
                    Availability::Available
                );
                control.clock.advance(Duration::from_secs(10));
                for _ in 0..10 {
                    future::yield_now().await;
                }
                let failed_reads = if timeout { 1 } else { 4 };
                assert_eq!(peer.count("get", "one", 2), failed_reads);
                assert_eq!(peer.count("subscribe", "multi", 9), 4);
                assert_eq!(peer.count("subscribe", "multi", 3), 1);
                {
                    let mut state = peer.state.borrow_mut();
                    state.sub_reject.clear();
                    state.sub_timeout.clear();
                    state.read_fail.clear();
                    state.read_timeout.clear();
                    state.values.insert(read.clone(), true);
                }
                peer.list_changed(&["multi"]);
                until(|| {
                    control
                        .context
                        .live
                        .borrow()
                        .session
                        .as_ref()
                        .unwrap()
                        .devices["multi"]
                        .subscribed
                        .borrow()
                        .contains(&(9, 1))
                })
                .await;
                if timeout {
                    until(|| {
                        function(&control.context.registry, "one", "main")
                            .snapshot()
                            .power
                            == Power::On
                    })
                    .await;
                    assert_eq!(peer.count("get", "one", 2), 2);
                } else {
                    assert_eq!(peer.count("get", "one", 2), 4);
                }
                peer.state.borrow_mut().online.insert("one".into(), false);
                peer.list_changed(&["one"]);
                until(|| {
                    matches!(
                        function(&control.context.registry, "one", "main")
                            .snapshot()
                            .availability,
                        Availability::Unavailable(_)
                    )
                })
                .await;
                peer.state.borrow_mut().online.insert("one".into(), true);
                peer.list_changed(&["one"]);
                until(|| {
                    function(&control.context.registry, "one", "main")
                        .snapshot()
                        .power
                        == Power::On
                })
                .await;
                let expected = failed_reads + if timeout { 2 } else { 1 };
                until(|| peer.count("get", "one", 2) == expected).await;
                assert_eq!(peer.count("get", "one", 2), expected);
            })
            .await;
        }));
    }
}
