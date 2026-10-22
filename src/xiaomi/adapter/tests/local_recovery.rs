use super::*;
use crate::xiaomi::adapter::{
    routing::PathPriority,
    state::{AccessRefresh, AccessRequest},
};

#[test]
fn shared_recovery_starts_after_sixty_seconds_and_retries_from_round_end() {
    block_on(async {
        let clock = TestClock::default();
        let access = AccessRefresh::default();
        access.request(clock.now());
        access.request_filtered(vec!["one".into()]);
        assert!(access.degrade(clock.now()));
        assert!(poll_once(access.next(&clock)).await.is_none());
        clock.advance(Duration::from_secs(59));
        assert!(!access.degrade(clock.now()));
        access.request(clock.now());
        access.retry_full(clock.now());
        assert!(poll_once(access.next(&clock)).await.is_none());
        clock.advance(Duration::from_secs(1));
        assert!(matches!(access.next(&clock).await, AccessRequest::Recovery));
        clock.advance(Duration::from_secs(10));
        assert!(poll_once(access.next(&clock)).await.is_none());
        access.recovery_finished(clock.now());
        clock.advance(Duration::from_secs(299));
        assert!(!access.degrade(clock.now()));
        assert!(poll_once(access.next(&clock)).await.is_none());
        clock.advance(Duration::from_secs(1));
        assert!(matches!(access.next(&clock).await, AccessRequest::Recovery));
        access.recovered();
        clock.advance(Duration::from_secs(1000));
        assert!(poll_once(access.next(&clock)).await.is_none());
        assert!(!access.is_degraded());
    });
}

#[test]
fn new_device_events_are_allowed_without_restarting_shared_recovery() {
    block_on(async {
        let clock = TestClock::default();
        let access = AccessRefresh::default();
        access.degrade(clock.now());
        clock.advance(Duration::from_secs(30));
        access.request_filtered(vec!["one".into(), "one".into()]);
        let AccessRequest::Filtered(dids) = access.next(&clock).await else {
            panic!("new device events must remain eligible")
        };
        assert_eq!(dids, ["one"]);
        clock.advance(Duration::from_secs(29));
        assert!(poll_once(access.next(&clock)).await.is_none());
        clock.advance(Duration::from_secs(1));
        assert!(matches!(access.next(&clock).await, AccessRequest::Recovery));
    });
}

#[test]
fn an_old_successful_round_cannot_delay_a_new_response_timeout_recovery() {
    block_on(async {
        let clock = TestClock::default();
        let access = AccessRefresh::default();
        access.degrade(clock.now());
        clock.advance(Duration::from_secs(60));
        assert!(matches!(access.next(&clock).await, AccessRequest::Recovery));
        // The list callback runs on the driver before the access future resumes.
        access.recovered();
        // Another sent request can time out on that driver before this future wakes.
        assert!(access.degrade(clock.now()));
        super::super::local::finish_access_round(&access, true, &Ok(Vec::new()), clock.now());
        clock.advance(Duration::from_secs(59));
        assert!(poll_once(access.next(&clock)).await.is_none());
        clock.advance(Duration::from_secs(1));
        assert!(matches!(
            poll_once(access.next(&clock)).await,
            Some(AccessRequest::Recovery)
        ));
    });
}

#[test]
fn degraded_local_can_still_accept_explicit_writes_when_cloud_is_ineligible() {
    block_on(Rig::new(true, &["one"]).run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(Rc::new(Peer::default()), |peer| async move {
            initialized(control, &peer, &["one"]).await;
            control
                .clock
                .automatic
                .store(false, std::sync::atomic::Ordering::Relaxed);
            let one = function(&control.context.registry, "one", "main");
            let reads = peer.count("get", "one", 2);
            peer.state.borrow_mut().hold_writes = true;
            assert!(one.execute(Command::On).await.is_err());
            peer.state.borrow_mut().hold_writes = false;
            one.execute(Command::Off).await.unwrap();
            assert_eq!(control.context.priority(), PathPriority::CloudFirst);
            assert_eq!(peer.count("rpcReq", "one", 2), 2);
            assert_eq!(control.cloud.lock().unwrap().property_writes.len(), 0);
            control.context.live.borrow().devices["one"]
                .admitted
                .as_ref()
                .unwrap()
                .refresh();
            control.clock.advance(Duration::from_secs(3));
            for _ in 0..20 {
                future::yield_now().await;
            }
            assert_eq!(peer.count("get", "one", 2), reads);
        })
        .await;
    }));
}

#[test]
fn sent_timeout_degrades_without_replaying_the_write_or_revoking_local_notifications() {
    let rig = Rig::new(true, &["one"]);
    rig.cloud.lock().unwrap().online.insert("one".into());
    block_on(rig.run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(Rc::new(Peer::default()), |peer| async move {
            initialized(control, &peer, &["one"]).await;
            until(|| control.context.cloud_scope_valid.get()).await;
            control
                .clock
                .automatic
                .store(false, std::sync::atomic::Ordering::Relaxed);
            let one = function(&control.context.registry, "one", "main");
            peer.state.borrow_mut().hold_writes = true;
            assert_eq!(
                one.execute(Command::On).await,
                Err(crate::device::CommandError::Timeout)
            );
            assert_eq!(control.context.priority(), PathPriority::CloudFirst);
            assert_eq!(peer.count("rpcReq", "one", 2), 1);
            assert_eq!(control.cloud.lock().unwrap().property_writes.len(), 0);
            one.execute(Command::On).await.unwrap();
            assert_eq!(control.cloud.lock().unwrap().property_writes.len(), 1);
            assert_eq!(peer.count("rpcReq", "one", 2), 1);
            peer.notify("one", 2, false);
            until(|| one.snapshot().power() == Some(Power::Off)).await;
            assert_eq!(one.snapshot().availability(), Availability::Available);
        })
        .await;
    }));
}

#[test]
fn valid_recovery_list_restores_priority_even_when_the_device_is_still_offline() {
    let rig = Rig::new(true, &["one"]);
    rig.cloud.lock().unwrap().online.insert("one".into());
    block_on(rig.run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(Rc::new(Peer::default()), |peer| async move {
            initialized(control, &peer, &["one"]).await;
            until(|| control.context.cloud_scope_valid.get()).await;
            control
                .clock
                .automatic
                .store(false, std::sync::atomic::Ordering::Relaxed);
            let one = function(&control.context.registry, "one", "main");
            peer.state.borrow_mut().hold_writes = true;
            assert!(one.execute(Command::On).await.is_err());
            let lists = peer.lists();
            peer.state.borrow_mut().online.insert("one".into(), false);
            control.clock.advance(Duration::from_secs(59));
            for _ in 0..20 {
                future::yield_now().await;
            }
            assert_eq!(peer.lists(), lists);
            control.clock.advance(Duration::from_secs(1));
            until(|| {
                peer.lists() == lists + 1 && control.context.priority() == PathPriority::LocalFirst
            })
            .await;
            assert!(
                !control.context.live.borrow().devices["one"]
                    .access
                    .as_ref()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .online
            );
            one.execute(Command::Off).await.unwrap();
            assert_eq!(control.cloud.lock().unwrap().property_writes.len(), 1);
            assert_eq!(peer.count("rpcReq", "one", 2), 1);
            let lists = peer.lists();
            control.clock.advance(Duration::from_secs(1000));
            for _ in 0..20 {
                future::yield_now().await;
            }
            assert_eq!(peer.lists(), lists);
        })
        .await;
    }));
}
