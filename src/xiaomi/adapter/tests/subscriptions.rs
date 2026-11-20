use super::*;
use crate::xiaomi::capability::PropertyBinding;

#[test]
fn subscription_and_read_retries_are_bounded_and_later_events_restore_them() {
    for timeout in [false, true] {
        let rig = Rig::new(true, &["one", "multi"]);
        if timeout {
            rig.network.use_short_gateway_request_timeout();
        }
        block_on(rig.run(|c| async move {
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
            let _diagnostics = OnTestPanic(|| {
                let status = control.context.diagnostics();
                let mode = if timeout { "no reply" } else { "rejected" };
                eprintln!(
                    "Retry scenario ({mode}) at {:?}: reads(one/2.1)={}, subscriptions(multi/9.1)={}, gateway={:?}, pending={:?}",
                    control.clock.now(),
                    peer.count("get", "one", 2),
                    peer.count("subscribe", "multi", 9),
                    status.gateway,
                    status.pending
                );
            });
            c.peer(peer.clone(), |peer| async move {
                until(|| {
                    control
                        .context
                        .diagnostics()
                        .pending
                        .iter()
                        .any(|pending| pending.did == "one" && pending.reason.contains("read"))
                        && control.context.diagnostics().pending.iter().any(|pending| {
                            pending.did == "multi" && pending.reason.contains("subscribe")
                        })
                })
                .await;
                let (read_work, subscription_work) = {
                    let directory = control.context.directory.borrow();
                    (
                        directory.devices["one"].work.clone().unwrap(),
                        directory.devices["multi"].work.clone().unwrap(),
                    )
                };
                let read_binding = PropertyBinding { siid: 2, piid: 1 };
                exhaust_read_retries(&control.clock, &read_work, "outlet:2", read_binding).await;
                until(|| subscription_work.test_local_exhausted()).await;
                assert!(
                    control
                        .context
                        .diagnostics()
                        .pending
                        .iter()
                        .any(|pending| pending.did == "one" && pending.reason.contains("read"))
                );
                assert!(
                    control
                        .context
                        .diagnostics()
                        .pending
                        .iter()
                        .any(|pending| pending.did == "multi"
                            && pending.reason.contains("subscribe"))
                );
                assert_eq!(
                    function(&control.context, "one", "outlet:2")
                        .snapshot()
                        .power(),
                    Some(Power::Unknown)
                );
                assert_eq!(
                    function(&control.context, "one", "outlet:2")
                        .snapshot()
                        .availability,
                    Availability::Available
                );
                control.clock.advance(Duration::from_secs(10));
                // Complete a gateway round trip after advancing beyond the retry window.
                let gateway = control.context.local_candidate("one", false).unwrap();
                gateway.get_devices(|_| {}).await.result.unwrap();
                let failed_reads = 4;
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
                    subscription_work
                        .test_local_subscribed_contains(PropertyBinding { siid: 9, piid: 1 })
                })
                .await;
                assert_eq!(peer.count("get", "one", 2), 4);
                peer.state.borrow_mut().online.insert("one".into(), false);
                peer.list_changed(&["one"]);
                until(|| {
                    matches!(
                        function(&control.context, "one", "outlet:2")
                            .snapshot()
                            .availability,
                        Availability::Unavailable(_)
                    ) && read_work.observer().is_none()
                })
                .await;
                peer.state.borrow_mut().online.insert("one".into(), true);
                peer.list_changed(&["one"]);
                until(|| {
                    read_work.observer().is_some()
                        && !read_work.refresh.borrow().contains("outlet:2", read_binding)
                        && function(&control.context, "one", "outlet:2").snapshot().power()
                        == Some(Power::On)
                })
                .await;
                // Availability and the new notification baseline can request separate reads.
                let recovered_reads = peer.count("get", "one", 2) - failed_reads;
                assert!((1..=2).contains(&recovered_reads), "{recovered_reads}");
            })
            .await;
        }));
    }
}
