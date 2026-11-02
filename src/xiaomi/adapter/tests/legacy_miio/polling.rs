use super::*;

#[test]
fn legacy_poll_reads_external_changes_with_cloud_notifications_ready() {
    let did = "308540429";
    let (rig, peer) = setup(did);
    let (rig, listener, cloud) = with_cloud_rig(rig);
    block_on(rig.run(|control| async move {
        future::or(cloud.run(&listener), async {
            let ac = function(&control.context.registry, did, "air-conditioner:2");
            let work = control.context.live.borrow().devices[did]
                .admitted
                .clone()
                .unwrap();
            let bindings = work.native.device().functions()[0].observation_bindings();
            until(|| {
                work.function_observer("air-conditioner:2")
                    .is_some_and(|source| work.source_ready("air-conditioner:2", &source))
                    && control.context.direct_candidate(did).is_ok()
                    && bindings.iter().all(|binding| {
                        !work
                            .refresh
                            .borrow()
                            .contains("air-conditioner:2", *binding)
                    })
                    && ac.snapshot().air_conditioner().unwrap().temperature.value == Some(24)
            })
            .await;
            control.clock.automatic.store(false, Ordering::Relaxed);
            let reads = || {
                peer.requests
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|request| request["method"] == "get_prop")
                    .count()
            };
            let before = reads();
            peer.values.lock().unwrap()[2] = json!(27);
            control.clock.advance(Duration::from_secs(61));
            until(|| ac.snapshot().air_conditioner().unwrap().temperature.value == Some(27)).await;
            assert_eq!(reads(), before + 1);

            control.clock.advance(Duration::from_secs(59));
            async_io::Timer::after(Duration::from_millis(30)).await;
            assert_eq!(reads(), before + 1);
            control.clock.advance(Duration::from_secs(1));
            until(|| reads() == before + 2).await;
            until(|| {
                bindings.iter().all(|binding| {
                    !work
                        .refresh
                        .borrow()
                        .contains("air-conditioner:2", *binding)
                })
            })
            .await;
            assert!(
                peer.requests
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|request| !request["method"].as_str().unwrap().starts_with("set_"))
            );

            peer.hold_read.store(true, Ordering::Relaxed);
            control.clock.advance(Duration::from_secs(60));
            until(|| reads() == before + 3).await;
            assert!(control.context.stop_lan(did));
            assert!(matches!(
                control.context.read_path_for(did, "air-conditioner:2"),
                Some(super::super::super::routing::ControlPath::Cloud(_))
            ));
            until(|| {
                bindings.iter().all(|binding| {
                    !work
                        .refresh
                        .borrow()
                        .contains("air-conditioner:2", *binding)
                })
            })
            .await;
            let cloud_reads = control.cloud.lock().unwrap().property_reads.len();
            control.clock.advance(Duration::from_secs(61));
            until(|| control.cloud.lock().unwrap().property_reads.len() > cloud_reads).await;
            assert_eq!(reads(), before + 3);
            peer.hold_read.store(false, Ordering::Relaxed);
        })
        .await;
    }));
}

#[test]
fn slow_legacy_poll_does_not_accumulate_or_block_user_control() {
    let did = "308540429";
    let (rig, peer) = setup(did);
    block_on(rig.run(|control| async move {
        let ac = function(&control.context.registry, did, "air-conditioner:2");
        let work = control.context.live.borrow().devices[did]
            .admitted
            .clone()
            .unwrap();
        let bindings = work.native.device().functions()[0].observation_bindings();
        until(|| {
            control.context.direct_candidate(did).is_ok()
                && bindings.iter().all(|binding| {
                    !work
                        .refresh
                        .borrow()
                        .contains("air-conditioner:2", *binding)
                })
                && ac.snapshot().air_conditioner().unwrap().temperature.value == Some(24)
        })
        .await;
        control.clock.automatic.store(false, Ordering::Relaxed);
        let reads = || {
            peer.requests
                .lock()
                .unwrap()
                .iter()
                .filter(|request| request["method"] == "get_prop")
                .count()
        };
        let before = reads();
        peer.hold_read.store(true, Ordering::Relaxed);
        control.clock.advance(Duration::from_secs(61));
        until(|| reads() == before + 1).await;
        control.clock.advance(Duration::from_secs(180));
        async_io::Timer::after(Duration::from_millis(30)).await;
        assert_eq!(reads(), before + 1);

        ac.begin_air_conditioner(AirConditionerCommand::Temperature(26))
            .unwrap()
            .accepted()
            .await
            .unwrap();
        assert!(peer.hold_read.load(Ordering::Relaxed));
        assert!(peer.requests.lock().unwrap().iter().any(|request| {
            request["method"] == "set_tar_temp" && request["params"] == json!([26])
        }));
        peer.hold_read.store(false, Ordering::Relaxed);
    }));
}
