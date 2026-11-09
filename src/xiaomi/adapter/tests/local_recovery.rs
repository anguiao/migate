use super::*;
use crate::xiaomi::{
    adapter::routing::{ControlPath, PathPriority},
    capability::PropertyBinding,
};

#[test]
fn one_device_read_timeout_keeps_other_devices_on_the_hub() {
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
            until(|| {
                control.context.cloud_candidate("one", true).is_ok()
                    && control.context.cloud_candidate("two", true).is_ok()
            })
            .await;
            control
                .clock
                .automatic
                .store(false, std::sync::atomic::Ordering::Relaxed);
            let work = control.context.live.borrow().devices["one"]
                .admitted
                .clone()
                .unwrap();
            let one = function(&control.context.registry, "one", "outlet:2");
            let two = function(&control.context.registry, "two", "outlet:2");
            let last = one.snapshot().power();
            let reads = peer.count("get", "one", 2);
            let lists = peer.lists();
            peer.state
                .borrow_mut()
                .read_timeout
                .insert(("one".into(), 2, 1));
            work.refresh();
            for _ in 0..20 {
                future::yield_now().await;
            }
            control.clock.advance(Duration::from_secs(1));
            until(|| peer.count("get", "one", 2) > reads).await;
            until(|| {
                work.refresh
                    .borrow()
                    .property_problem("outlet:2", PropertyBinding { siid: 2, piid: 1 })
                    .is_some()
            })
            .await;
            assert_eq!(one.snapshot().power(), last);
            assert_eq!(control.context.priority(), PathPriority::LocalFirst);
            assert!(matches!(
                control.context.control_path("one"),
                Some(ControlPath::Local(_))
            ));
            assert!(matches!(
                control.context.control_path("two"),
                Some(ControlPath::Local(_))
            ));
            two.execute(Command::On).await.unwrap();
            assert_eq!(peer.count("rpcReq", "two", 2), 1);
            assert!(control.cloud.lock().unwrap().property_writes.is_empty());
            assert_eq!(
                peer.lists(),
                lists,
                "a device timeout must not start a new probe"
            );
        })
        .await;
    }));
}

#[test]
fn sent_write_timeout_is_not_replayed_and_other_devices_stay_on_the_hub() {
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
            until(|| {
                control.context.cloud_candidate("one", true).is_ok()
                    && control.context.cloud_candidate("two", true).is_ok()
            })
            .await;
            let one = function(&control.context.registry, "one", "outlet:2");
            let two = function(&control.context.registry, "two", "outlet:2");
            let lists = peer.lists();
            peer.state.borrow_mut().hold_writes = true;
            assert_eq!(
                one.execute(Command::On).await,
                Err(crate::device::CommandError::Timeout)
            );
            peer.state.borrow_mut().hold_writes = false;
            assert_eq!(control.context.priority(), PathPriority::LocalFirst);
            assert!(matches!(
                control.context.control_path("two"),
                Some(ControlPath::Local(_))
            ));
            two.execute(Command::On).await.unwrap();
            peer.release("one", 2, accepted("one", 2, 0)).await;
            for _ in 0..20 {
                future::yield_now().await;
            }
            assert_eq!(peer.count("rpcReq", "one", 2), 1);
            assert_eq!(peer.count("rpcReq", "two", 2), 1);
            assert!(control.cloud.lock().unwrap().property_writes.is_empty());
            assert_eq!(
                peer.lists(),
                lists,
                "a device timeout must not start a new probe"
            );
        })
        .await;
    }));
}
