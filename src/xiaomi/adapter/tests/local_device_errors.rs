use super::*;
use crate::xiaomi::adapter::routing::{ControlPath, PathPriority};

#[test]
fn local_offline_or_missing_writes_revoke_only_the_target_until_a_new_list_observation() {
    for code in [-10_007, -704_042_001, -704_042_011, -704_090_001] {
        let rig = Rig::new(true, &["one", "two"]);
        rig.cloud
            .lock()
            .unwrap()
            .online
            .extend(["one".into(), "two".into()]);
        block_on(rig.run(|c| async move {
            c.announce();
            let control = &c;
            c.peer(Rc::new(Peer::default()), |peer| async move {
                initialized(control, &peer, &["one", "two"]).await;
                until(|| control.context.cloud_candidate("one", true).is_ok()).await;
                control
                    .clock
                    .automatic
                    .store(false, std::sync::atomic::Ordering::Relaxed);
                let one = function(&control.context.registry, "one", "main");
                let two = function(&control.context.registry, "two", "main");
                peer.state.borrow_mut().hold_writes = true;
                let mut failed = Box::pin(one.execute(Command::On));
                assert!(poll_once(failed.as_mut()).await.is_none());
                peer.release("one", 2, accepted("one", 2, code)).await;
                assert!(matches!(
                    failed.await,
                    Err(crate::device::CommandError::Rejected(_))
                ));
                assert_eq!(control.context.priority(), PathPriority::LocalFirst);
                assert_eq!(one.snapshot().availability, Availability::Available);
                assert!(matches!(
                    control.context.control_path("one"),
                    Some(ControlPath::Cloud(_))
                ));
                assert!(control.cloud.lock().unwrap().property_writes.is_empty());
                peer.state.borrow_mut().hold_writes = false;
                two.execute(Command::On).await.unwrap();
                one.execute(Command::Off).await.unwrap();
                assert_eq!(peer.count("rpcReq", "one", 2), 1);
                assert_eq!(peer.count("rpcReq", "two", 2), 1);
                assert_eq!(control.cloud.lock().unwrap().property_writes.len(), 1);

                peer.list_changed(&["one"]);
                until(|| {
                    matches!(
                        control.context.control_path("one"),
                        Some(ControlPath::Local(_))
                    )
                })
                .await;
                one.execute(Command::On).await.unwrap();
                assert_eq!(peer.count("rpcReq", "one", 2), 2);
                assert_eq!(control.cloud.lock().unwrap().property_writes.len(), 1);
            })
            .await;
        }));
    }
}

#[test]
fn local_business_rejections_and_device_operation_timeouts_keep_the_local_candidate() {
    for code in [-7, -704_030_023, -704_053_036, -704_083_036] {
        let rig = Rig::new(true, &["one"]);
        rig.cloud.lock().unwrap().online.insert("one".into());
        block_on(rig.run(|c| async move {
            c.announce();
            let control = &c;
            c.peer(Rc::new(Peer::default()), |peer| async move {
                initialized(control, &peer, &["one"]).await;
                until(|| control.context.cloud_candidate("one", true).is_ok()).await;
                peer.state.borrow_mut().hold_writes = true;
                let one = function(&control.context.registry, "one", "main");
                let mut failed = Box::pin(one.execute(Command::On));
                assert!(poll_once(failed.as_mut()).await.is_none());
                peer.release("one", 2, accepted("one", 2, code)).await;
                assert!(matches!(
                    failed.await,
                    Err(crate::device::CommandError::Rejected(_))
                ));
                assert_eq!(control.context.priority(), PathPriority::LocalFirst);
                assert!(matches!(
                    control.context.control_path("one"),
                    Some(ControlPath::Local(_))
                ));
                peer.state.borrow_mut().hold_writes = false;
                one.execute(Command::Off).await.unwrap();
                assert_eq!(peer.count("rpcReq", "one", 2), 2);
                assert!(control.cloud.lock().unwrap().property_writes.is_empty());
            })
            .await;
        }));
    }
}

#[test]
fn local_device_offline_reads_continue_over_cloud_without_degrading_other_devices() {
    let rig = Rig::new(true, &["one", "two"]);
    rig.cloud
        .lock()
        .unwrap()
        .online
        .extend(["one".into(), "two".into()]);
    block_on(rig.run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(Rc::new(Peer::default()), |peer| async move {
            initialized(control, &peer, &["one", "two"]).await;
            until(|| control.context.cloud_candidate("one", true).is_ok()).await;
            until(|| {
                control.context.live.borrow().devices.values().all(|slot| {
                    slot.work.as_ref().is_none_or(|work| {
                        work.refresh.borrow().snapshot().inflight.is_empty()
                            && work.refresh.borrow().snapshot().pending.is_empty()
                    })
                })
            })
            .await;
            peer.state.borrow_mut().hold_reads = true;
            let one = function(&control.context.registry, "one", "main");
            let work = control.context.live.borrow().devices["one"]
                .work
                .clone()
                .unwrap();
            control
                .cloud
                .lock()
                .unwrap()
                .read_values
                .insert(("one".into(), 2, 1), true);
            work.refresh();
            peer.release("one", 2, json!({"error":{"code":-704042011}}))
                .await;
            peer.state.borrow_mut().hold_reads = false;
            until(|| one.snapshot().power == Power::On).await;
            assert_eq!(control.context.priority(), PathPriority::LocalFirst);
            assert!(matches!(
                control.context.read_path("one"),
                Some(ControlPath::Cloud(_))
            ));
            assert!(matches!(
                control.context.read_path("two"),
                Some(ControlPath::Local(_))
            ));
            let reads = peer.count("get", "one", 2);
            peer.state.borrow_mut().hold_reads = false;
            peer.list_changed(&["one"]);
            until(|| {
                matches!(
                    control.context.read_path("one"),
                    Some(ControlPath::Local(_))
                )
            })
            .await;
            work.refresh();
            until(|| peer.count("get", "one", 2) > reads).await;
            until(|| one.snapshot().power == Power::Off).await;
        })
        .await;
    }));
}
