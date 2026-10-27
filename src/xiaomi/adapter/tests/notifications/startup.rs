use super::*;

#[test]
fn many_devices_share_sixteen_cloud_startup_handshakes_without_queue_full_retries() {
    let members = (0..80)
        .map(|index| format!("device{index:02}"))
        .collect::<Vec<_>>();
    let members = members.iter().map(String::as_str).collect::<Vec<_>>();
    let paths = startup_paths(&members);
    let (rig, listener, peer) = with_cloud_peer(&members);
    rig.clock
        .automatic
        .store(false, std::sync::atomic::Ordering::Relaxed);
    peer.state.borrow_mut().hold.extend(paths.iter().cloned());
    block_on(rig.run(|c| async move {
        future::or(peer.run(&listener), async {
            until(|| peer.state.borrow().held.len() >= 16).await;
            for _ in 0..20 {
                future::yield_now().await;
            }
            assert_eq!(peer.state.borrow().held.len(), 16);
            release_startup_subscriptions(&peer, paths.len()).await;
            until(|| members.iter().all(|did| cloud_active(&c, did))).await;
            for path in &paths {
                assert_eq!(peer.count(path), 1, "{path}");
            }
            assert_eq!(c.clock.now(), Duration::ZERO);
        })
        .await;
    }));
}

#[test]
fn waiting_cloud_startup_subscriptions_are_revoked_and_replaced_before_sending() {
    let members = (0..80)
        .map(|index| format!("device{index:02}"))
        .collect::<Vec<_>>();
    let members = members.iter().map(String::as_str).collect::<Vec<_>>();
    let paths = startup_paths(&members);
    let (rig, listener, peer) = with_cloud_peer(&members);
    rig.clock
        .automatic
        .store(false, std::sync::atomic::Ordering::Relaxed);
    peer.state.borrow_mut().hold.extend(paths.iter().cloned());
    block_on(rig.run(|c| async move {
        future::or(peer.run(&listener), async {
            until(|| peer.state.borrow().held.len() == 16).await;
            assert_eq!(peer.count("device/device79/up/properties_changed/2/1"), 0);
            force_unavailable(&c.context, "device79", "test removal while queued");
            let replaced = c.context.live.borrow().devices["device78"]
                .admitted
                .clone()
                .unwrap();
            replaced.test_retire_cloud_preparation();
            c.context.changed.notify(usize::MAX);
            release_startup_subscriptions(&peer, paths.len() - 2).await;
            until(|| members[..79].iter().all(|did| cloud_active(&c, did))).await;
            assert_eq!(peer.count("device/device79/up/properties_changed/2/1"), 0);
            assert_eq!(peer.count("device/device79/state/#"), 0);
            assert_eq!(peer.count("device/device78/up/properties_changed/2/1"), 1);
            assert!(!replaced.test_cloud_preparing());
        })
        .await;
    }));
}

#[test]
fn reconnect_discards_inflight_and_waiting_startup_subscription_budget() {
    let members = (0..80)
        .map(|index| format!("device{index:02}"))
        .collect::<Vec<_>>();
    let members = members.iter().map(String::as_str).collect::<Vec<_>>();
    let paths = startup_paths(&members);
    let (rig, listener, peer) = with_cloud_peer(&members);
    rig.clock
        .automatic
        .store(false, std::sync::atomic::Ordering::Relaxed);
    peer.state.borrow_mut().hold.extend(paths.iter().cloned());
    block_on(rig.run(|c| async move {
        future::or(peer.run(&listener), async {
            until(|| peer.state.borrow().held.len() == 16).await;
            peer.send(Packet::Disconnect(Disconnect::new()));
            until(|| c.context.notifications.borrow().is_none()).await;
            let due = c.clock.now() + Duration::from_secs(10);
            until(|| c.clock.deadlines.lock().unwrap().contains(&due)).await;
            peer.state.borrow_mut().held.clear();
            peer.state.borrow_mut().seen.clear();
            c.clock.advance(Duration::from_secs(10));
            until(|| peer.state.borrow().connects.len() == 2).await;
            until(|| peer.state.borrow().held.len() == 16).await;
            // This lifecycle test must tolerate a scheduling pause while ACKs are held.
            // Request expiration is covered by the notification driver's own tests.
            async_io::Timer::after(Duration::from_millis(200)).await;
            assert_eq!(peer.state.borrow().held.len(), 16);
            release_startup_subscriptions(&peer, paths.len()).await;
            until(|| members.iter().all(|did| cloud_active(&c, did))).await;
            for path in &paths {
                assert_eq!(peer.count(path), 1, "{path}");
            }
        })
        .await;
    }));
}

#[test]
fn failed_cloud_handshakes_release_capacity_during_the_retry_delay() {
    let members = (0..20)
        .map(|index| format!("device{index:02}"))
        .collect::<Vec<_>>();
    let members = members.iter().map(String::as_str).collect::<Vec<_>>();
    let paths = startup_paths(&members);
    let (rig, listener, peer) = with_cloud_peer(&members);
    rig.clock
        .automatic
        .store(false, std::sync::atomic::Ordering::Relaxed);
    for did in &members[..16] {
        peer.state.borrow_mut().reject.insert(
            format!("device/{did}/up/properties_changed/2/1"),
            SubscribeReasonCode::QuotaExceeded,
        );
    }
    block_on(rig.run(|c| async move {
        future::or(peer.run(&listener), async {
            until(|| paths.iter().all(|path| peer.count(path) == 1)).await;
            until(|| members[16..].iter().all(|did| cloud_active(&c, did))).await;
            until(|| {
                c.clock
                    .deadlines
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|deadline| **deadline == Duration::from_secs(1))
                    .count()
                    == 16
            })
            .await;
            assert_eq!(c.clock.now(), Duration::ZERO);
            assert!(members[..16].iter().all(|did| !cloud_active(&c, did)));
            peer.state.borrow_mut().reject.clear();
            c.clock.advance(Duration::from_secs(1));
            until(|| members.iter().all(|did| cloud_active(&c, did))).await;
            for (index, did) in members.iter().enumerate() {
                assert_eq!(peer.count(&format!("device/{did}/state/#")), 1);
                assert_eq!(
                    peer.count(&format!("device/{did}/up/properties_changed/2/1")),
                    if index < 16 { 2 } else { 1 }
                );
            }
        })
        .await;
    }));
}
