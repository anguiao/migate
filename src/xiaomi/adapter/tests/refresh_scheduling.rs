use super::*;

fn manual_rig(members: &[&str]) -> Rig {
    let mut rig = Rig::new(true, members);
    rig.clock
        .automatic
        .store(false, std::sync::atomic::Ordering::Relaxed);
    rig.source.request_timeout = Duration::from_secs(2);
    rig
}

async fn wait_deadline(clock: &TestClock, millis: u64) {
    let deadline = Duration::from_millis(millis);
    until(|| clock.deadlines.lock().unwrap().contains(&deadline)).await;
}

async fn settle_wire() {
    async_io::Timer::after(Duration::from_millis(20)).await;
}

fn reads_since(peer: &Peer, start: usize) -> Vec<String> {
    peer.state.borrow().seen[start..]
        .iter()
        .filter(|(operation, _)| operation == "get")
        .map(|(_, payload)| payload["did"].as_str().unwrap().to_owned())
        .collect()
}

async fn subscriptions_ready(c: &Control, dids: &[&str]) {
    advance_to(&c.clock, Duration::from_millis(10)).await;
    until(|| {
        let live = c.context.live.borrow();
        dids.iter().all(|did| {
            live.devices
                .get(*did)
                .and_then(|slot| slot.admitted.as_ref())
                .is_some_and(|work| {
                    let cached = &work.native;
                    work.test_local_subscribed_len()
                        == cached.device().functions().len()
                            + usize::from(cached.device().refresh_trigger().is_some())
                })
        })
    })
    .await;
    wait_deadline(&c.clock, 200).await;
}

async fn trigger_burst(c: &Control, peer: &Peer) {
    for value in [true, false, true] {
        peer.notify("multi", 2, value);
    }
    // The following property notification confirms the preceding triggers arrived.
    let outlet = function(&c.context.registry, "multi", "outlet:4");
    let value = outlet.snapshot().power() != Some(Power::On);
    peer.notify("multi", 4, value);
    until(|| outlet.snapshot().power() == Some(if value { Power::On } else { Power::Off })).await;
}

#[test]
fn periodic_hub_reads_are_bounded_fair_and_yield_to_command_feedback() {
    let dids = [
        "device00", "device01", "device02", "device03", "device04", "device05", "device06",
        "device07", "device08", "device09", "device10", "device11", "device12", "device13",
        "device14", "device15", "device16",
    ];
    let mut rig = Rig::new(true, &dids).with_stall_timeout(Duration::from_secs(10));
    rig.source.request_timeout = Duration::from_secs(2);
    block_on(rig.run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        // Without push, a successful local command schedules a feedback read.
        peer.state
            .borrow_mut()
            .pushless
            .extend(dids.iter().map(|did| (*did).to_owned()));
        let control = &c;
        c.peer(peer, |peer| async move {
            // Finish all startup reads before the clock jump, so every held
            // request below was caused by the periodic scheduler.
            until(|| {
                dids.iter().all(|did| {
                    peer.count("get", did, 2) > 0
                        && control.context.local_candidate(did, false).is_ok()
                        && function(&control.context.registry, did, "outlet:2")
                            .snapshot()
                            .power()
                            == Some(Power::Off)
                        && control.context.live.borrow().devices[*did]
                            .admitted
                            .as_ref()
                            .is_some_and(|work| {
                                let refresh = work.refresh.borrow().snapshot();
                                refresh.pending.is_empty() && refresh.inflight.is_empty()
                            })
                })
            })
            .await;
            control
                .clock
                .automatic
                .store(false, std::sync::atomic::Ordering::Relaxed);
            settle_wire().await;
            let start = peer.state.borrow().seen.len();
            peer.state.borrow_mut().hold_reads = true;

            control.clock.advance(Duration::from_secs(61));
            let due = control.clock.now() + Duration::from_millis(200);
            advance_to(&control.clock, due).await;
            until(|| reads_since(&peer, start).len() >= 8).await;
            settle_wire().await;
            let first = reads_since(&peer, start);
            assert_eq!(first.len(), 8);
            assert_eq!(first.iter().collect::<BTreeSet<_>>().len(), 8);
            assert_eq!(peer.state.borrow().held.len(), 8);

            let feedback_did = &first[0];
            assert!(
                control.context.live.borrow().devices[feedback_did]
                    .admitted
                    .as_ref()
                    .unwrap()
                    .function_observer("outlet:2")
                    .is_none()
            );
            function(&control.context.registry, feedback_did, "outlet:2")
                .execute(Command::On)
                .await
                .unwrap();
            assert_eq!(reads_since(&peer, start).len(), 8);
            for did in &first {
                peer.release(did, 2, json!({"value":true})).await;
            }

            let due = control.clock.now() + Duration::from_millis(200);
            advance_to(&control.clock, due).await;
            until(|| reads_since(&peer, start).len() >= 16).await;
            settle_wire().await;
            let reads = reads_since(&peer, start);
            assert_eq!(reads.len(), 16);
            let second = &reads[8..];
            assert!(second.contains(feedback_did));
            assert_eq!(peer.state.borrow().held.len(), 8);
            for did in second {
                peer.release(did, 2, json!({"value":true})).await;
            }

            let due = control.clock.now() + Duration::from_millis(200);
            advance_to(&control.clock, due).await;
            until(|| reads_since(&peer, start).len() >= 18).await;
            settle_wire().await;
            let reads = reads_since(&peer, start);
            assert_eq!(reads.len(), 18);
            assert_eq!(reads.iter().collect::<BTreeSet<_>>().len(), dids.len());
            assert_eq!(peer.state.borrow().held.len(), 2);
            for did in &reads[16..] {
                peer.release(did, 2, json!({"value":true})).await;
            }
            until(|| {
                dids.iter().all(|did| {
                    function(&control.context.registry, did, "outlet:2")
                        .snapshot()
                        .power()
                        == Some(Power::On)
                })
            })
            .await;
        })
        .await;
    }));
}

#[test]
fn numeric_outlet_refresh_trigger_is_ignored() {
    block_on(Rig::new(true, &["multi"]).run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(Rc::new(Peer::default()), |peer| async move {
            initialized(control, &peer, &["multi"]).await;
            let before = peer.count("get", "multi", 0);
            peer.notify_value(
                "multi",
                crate::xiaomi::capability::PropertyBinding { siid: 2, piid: 1 },
                json!(42),
                false,
            );
            let outlet = function(&control.context.registry, "multi", "outlet:4");
            peer.notify("multi", 4, true);
            until(|| outlet.snapshot().power() == Some(Power::On)).await;
            async_io::Timer::after(Duration::from_millis(50)).await;
            assert_eq!(peer.count("get", "multi", 0), before);
        })
        .await;
    }));
}

#[test]
fn refresh_rounds_wait_for_every_did_and_trigger_bursts_keep_the_pending_deadline() {
    block_on(manual_rig(&["one", "multi"]).run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        peer.state.borrow_mut().hold_reads = true;
        let control = &c;
        c.peer(peer, |peer| async move {
            subscriptions_ready(control, &["one", "multi"]).await;
            control.clock.advance(Duration::from_millis(189));
            settle_wire().await;
            assert_eq!(peer.count("get", "multi", 0), 0);
            assert_eq!(peer.count("get", "one", 0), 0);

            control.clock.advance(Duration::from_millis(1));
            until(|| peer.count("get", "multi", 3) == 1 && peer.count("get", "one", 2) == 1).await;
            assert_eq!(peer.count("get", "multi", 0), 1);
            peer.release("multi", 3, json!({"value":false})).await;
            until(|| {
                function(&control.context.registry, "multi", "outlet:3")
                    .snapshot()
                    .power()
                    == Some(Power::Off)
            })
            .await;

            // A completed DID cannot start its next property while another DID is slow.
            control.clock.advance(Duration::from_millis(500));
            trigger_burst(control, &peer).await;
            settle_wire().await;
            assert_eq!(peer.count("get", "multi", 0), 1);
            peer.release("one", 2, json!({"value":false})).await;
            wait_deadline(&control.clock, 900).await;

            control.clock.advance(Duration::from_millis(199));
            trigger_burst(control, &peer).await;
            settle_wire().await;
            assert_eq!(peer.count("get", "multi", 0), 1);
            control.clock.advance(Duration::from_millis(1));
            peer.release("multi", 4, json!({"value":false})).await;

            advance_to(&control.clock, Duration::from_millis(1100)).await;
            peer.release("multi", 9, json!({"value":false})).await;
            advance_to(&control.clock, Duration::from_millis(1300)).await;
            peer.release("multi", 3, json!({"value":true})).await;
            until(|| {
                function(&control.context.registry, "multi", "outlet:3")
                    .snapshot()
                    .power()
                    == Some(Power::On)
            })
            .await;
            control.clock.advance(Duration::from_secs(1));
            settle_wire().await;
            assert_eq!(peer.count("get", "multi", 3), 2);
            assert_eq!(peer.count("get", "multi", 4), 1);
            assert_eq!(peer.count("get", "multi", 9), 1);
            assert_eq!(peer.count("get", "one", 2), 1);
        })
        .await;
    }));
}

#[test]
fn failed_properties_keep_independent_backoff_across_successes_and_triggers() {
    block_on(manual_rig(&["one", "multi", "two"]).run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        peer.state.borrow_mut().hold_reads = true;
        let control = &c;
        c.peer(peer, |peer| async move {
            subscriptions_ready(control, &["one", "multi", "two"]).await;
            advance_to(&control.clock, Duration::from_millis(200)).await;
            until(|| {
                peer.count("get", "multi", 3) == 1
                    && peer.count("get", "one", 2) == 1
                    && peer.count("get", "two", 2) == 1
            })
            .await;
            trigger_burst(control, &peer).await;
            peer.state.borrow_mut().hold_reads = false;
            peer.release("multi", 3, json!({"code":-7})).await;
            peer.release("one", 2, json!({"code":-7})).await;
            peer.release("two", 2, json!({"value":false})).await;

            advance_to(&control.clock, Duration::from_millis(400)).await;
            wait_deadline(&control.clock, 600).await;
            control.clock.advance(Duration::from_millis(100));
            trigger_burst(control, &peer).await;
            advance_to(&control.clock, Duration::from_millis(600)).await;
            advance_to(&control.clock, Duration::from_millis(800)).await;
            wait_deadline(&control.clock, 3200).await;
            assert_eq!(peer.count("get", "multi", 4), 2);
            assert_eq!(peer.count("get", "multi", 9), 1);

            control.clock.advance(Duration::from_millis(2399));
            settle_wire().await;
            assert_eq!(peer.count("get", "multi", 3), 1);
            assert_eq!(peer.count("get", "one", 2), 1);
            control.clock.advance(Duration::from_millis(1));
            until(|| peer.count("get", "multi", 3) == 2 && peer.count("get", "one", 2) == 2).await;
            initialized(control, &peer, &["one", "multi", "two"]).await;
            control.clock.advance(Duration::from_secs(1));
            settle_wire().await;
            assert_eq!(peer.count("get", "multi", 3), 2);
            assert_eq!(peer.count("get", "one", 2), 2);
            assert_eq!(peer.count("get", "two", 2), 1);
        })
        .await;
    }));
}

#[test]
fn a_late_did_waits_its_own_round_without_inheriting_another_dids_backoff() {
    block_on(manual_rig(&["one", "multi"]).run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        {
            let mut state = peer.state.borrow_mut();
            state.omit.insert("multi".into());
            state.read_fail.insert(("one".into(), 2, 1));
        }
        let control = &c;
        c.peer(peer, |peer| async move {
            subscriptions_ready(control, &["one"]).await;
            advance_to(&control.clock, Duration::from_millis(200)).await;
            wait_deadline(&control.clock, 3200).await;

            control.clock.advance(Duration::from_millis(500));
            peer.state.borrow_mut().omit.remove("multi");
            peer.list_changed(&["multi"]);
            // Hold time until the late device has a real read route and its own round.
            until(|| control.context.local_candidate("multi", false).is_ok()).await;
            wait_deadline(&control.clock, 900).await;
            advance_to(&control.clock, Duration::from_millis(710)).await;
            control.clock.advance(Duration::from_millis(189));
            settle_wire().await;
            assert_eq!(peer.count("get", "multi", 0), 0);
            assert_eq!(peer.count("get", "one", 2), 1);

            control.clock.advance(Duration::from_millis(1));
            until(|| peer.count("get", "multi", 3) == 1).await;
            advance_to(&control.clock, Duration::from_millis(1100)).await;
            advance_to(&control.clock, Duration::from_millis(1300)).await;
            initialized(control, &peer, &["multi"]).await;

            control.clock.advance(Duration::from_millis(1899));
            settle_wire().await;
            assert_eq!(peer.count("get", "one", 2), 1);
            assert_eq!(peer.count("get", "multi", 0), 3);
            peer.state.borrow_mut().read_fail.clear();
            control.clock.advance(Duration::from_millis(1));
            until(|| peer.count("get", "one", 2) == 2).await;
            initialized(control, &peer, &["one", "multi"]).await;
        })
        .await;
    }));
}

#[test]
fn removal_and_disconnect_cancel_pending_rounds_and_property_backoff() {
    for remove in [true, false] {
        block_on(manual_rig(&["one", "multi"]).run(|c| async move {
            c.announce();
            let peer = Rc::new(Peer::default());
            peer.state
                .borrow_mut()
                .read_fail
                .insert(("multi".into(), 3, 1));
            let control = &c;
            let observing = peer.clone();
            c.peer(peer, |peer| async move {
                subscriptions_ready(control, &["one", "multi"]).await;
                advance_to(&control.clock, Duration::from_millis(200)).await;
                wait_deadline(&control.clock, 400).await;
                assert_eq!(peer.count("get", "multi", 0), 1);
                if remove {
                    control.cloud.lock().unwrap().members = vec!["one".into()];
                    peer.list_changed(&["multi"]);
                    until(|| {
                        control
                            .context
                            .registry
                            .get(&DeviceKey::new("xiaomi", "multi"))
                            .is_none()
                            && !peer.state.borrow().unsubscribes.is_empty()
                    })
                    .await;
                    control.clock.advance(Duration::from_secs(3));
                    settle_wire().await;
                    assert_eq!(peer.count("get", "multi", 0), 1);
                }
            })
            .await;
            if !remove {
                until(|| {
                    matches!(
                        c.context.status.borrow().gateway,
                        GatewayStatus::Unavailable(_)
                    )
                })
                .await;
                c.clock.advance(Duration::from_secs(3));
                settle_wire().await;
                assert!(c.context.live.borrow().session.is_none());
                assert_eq!(observing.count("get", "multi", 0), 1);
            }
        }));
    }
}
