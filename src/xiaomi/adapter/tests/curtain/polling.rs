use super::*;
use std::sync::atomic::Ordering;

#[test]
fn hub_poll_recovers_an_external_change_with_subscription_ready() {
    block_on(rig().run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(peer(), |peer| async move {
            settled(control).await;
            assert_eq!(
                control.context.diagnostics().devices[0].functions[0].selected_read,
                Some(PathKind::Local)
            );
            control.clock.automatic.store(false, Ordering::Relaxed);
            let before = peer.count("get", "one", 0);
            peer.value("one", CURRENT, json!(52));
            peer.value("one", STATUS, json!(2));
            control.clock.advance(Duration::from_secs(11));
            control.clock.automatic.store(true, Ordering::Relaxed);
            until(|| {
                curtain(control).current_position.value == Some(52)
                    && curtain(control).status.value == Some(CurtainMotion::Stopped)
                    && peer.count("get", "one", 0) >= before + 3
            })
            .await;
            assert!(work(control).function_observer("curtain:2").is_some());
        })
        .await;
    }));
}

#[test]
fn cloud_poll_batches_the_curtain_and_recovers_an_external_change() {
    let (rig, listener, cloud) = cloud_rig();
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            settled(&c).await;
            assert_eq!(
                c.context.diagnostics().devices[0].functions[0].selected_read,
                Some(PathKind::Cloud)
            );
            c.clock.automatic.store(false, Ordering::Relaxed);
            let before = c.cloud.lock().unwrap().read_batches.len();
            c.cloud
                .lock()
                .unwrap()
                .raw_values
                .insert(("one".into(), 2, 3), json!(48));
            c.clock.advance(Duration::from_secs(11));
            c.clock.automatic.store(true, Ordering::Relaxed);
            until(|| curtain(&c).current_position.value == Some(48)).await;
            let batches = c.cloud.lock().unwrap().read_batches.clone();
            assert!(batches[before..].iter().any(|batch| {
                [STATUS, CURRENT, TARGET]
                    .iter()
                    .all(|binding| batch.contains(&("one".into(), binding.siid, binding.piid)))
            }));
            assert!(work(&c).function_observer("curtain:2").is_some());
        })
        .await;
    }));
}

#[test]
fn cloud_poll_rate_limit_backs_off_without_disabling_cloud_control() {
    let (rig, listener, cloud) = cloud_rig();
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            settled(&c).await;
            c.clock.automatic.store(false, Ordering::Relaxed);
            c.cloud.lock().unwrap().read_status = 429;
            let before = c.cloud.lock().unwrap().read_batches.len();
            c.clock.advance(Duration::from_secs(11));
            c.clock.automatic.store(true, Ordering::Relaxed);
            until(|| {
                c.cloud.lock().unwrap().read_batches.len() > before
                    && [STATUS, CURRENT, TARGET]
                        .iter()
                        .all(|binding| !work(&c).refresh.borrow().contains("curtain:2", *binding))
            })
            .await;
            c.clock.automatic.store(false, Ordering::Relaxed);
            assert!(!c.context.http_paused.get());
            assert_eq!(
                c.context.diagnostics().devices[0].selected_control,
                Some(PathKind::Cloud)
            );
            let failed_batches = c.cloud.lock().unwrap().read_batches.len();
            c.cloud.lock().unwrap().read_status = 200;
            c.clock.advance(Duration::from_secs(20));
            async_io::Timer::after(Duration::from_millis(40)).await;
            assert_eq!(c.cloud.lock().unwrap().read_batches.len(), failed_batches);
            c.clock.advance(Duration::from_secs(11));
            c.clock.automatic.store(true, Ordering::Relaxed);
            until(|| c.cloud.lock().unwrap().read_batches.len() > failed_batches).await;
        })
        .await;
    }));
}
