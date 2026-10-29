use super::*;

#[test]
fn published_cloud_only_device_is_refreshed_without_a_local_subscription() {
    let rig = Rig::new(true, &["one"]);
    rig.cloud.lock().unwrap().online.insert("one".into());
    block_on(rig.run(|c| async move {
        until(|| !c.cloud.lock().unwrap().property_reads.is_empty()).await;
        until(|| {
            function(&c.context.registry, "one", "outlet:2")
                .snapshot()
                .power()
                == Some(Power::Off)
        })
        .await;
        assert!(c.context.live.borrow().session.is_none());
        assert_eq!(
            c.cloud.lock().unwrap().property_reads,
            [("one".into(), 2, 1)]
        );
    }));
}

fn work(c: &Control, did: &str) -> Rc<super::super::work::DeviceWork> {
    c.context.live.borrow().devices[did]
        .admitted
        .as_ref()
        .unwrap()
        .clone()
}

fn cloud_reads(c: &Control, did: &str, siid: u32) -> usize {
    c.cloud
        .lock()
        .unwrap()
        .property_reads
        .iter()
        .filter(|(read_did, read_siid, _)| read_did == did && *read_siid == siid)
        .count()
}

#[test]
fn a_slow_cloud_channel_does_not_block_other_cloud_reads_or_commands() {
    let rig = Rig::new(true, &["multi"]);
    let (release, gate) = flume::bounded(1);
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.online.insert("multi".into());
        cloud.read_gates.insert(("multi".into(), 3, 1), gate);
        cloud.read_values.insert(("multi".into(), 4, 1), true);
        cloud.read_values.insert(("multi".into(), 9, 1), true);
    }
    block_on(rig.run(|c| async move {
        let outlet_1 = function(&c.context.registry, "multi", "outlet:3");
        let outlet_2 = function(&c.context.registry, "multi", "outlet:4");
        let usb = function(&c.context.registry, "multi", "outlet:9");
        until(|| {
            cloud_reads(&c, "multi", 3) == 1
                && outlet_2.snapshot().power() == Some(Power::On)
                && usb.snapshot().power() == Some(Power::On)
        })
        .await;
        assert_eq!(outlet_1.snapshot().power(), Some(Power::Unknown));
        assert_eq!(
            usb.execute(Command::Off).await.unwrap().power(),
            Some(Power::Off)
        );
        assert_eq!(c.cloud.lock().unwrap().property_writes.len(), 1);
        assert_eq!(cloud_reads(&c, "multi", 4), 1);
        assert_eq!(cloud_reads(&c, "multi", 9), 1);

        release.send(()).unwrap();
        until(|| outlet_1.snapshot().power() == Some(Power::Off)).await;
        assert_eq!(usb.snapshot().power(), Some(Power::Off));
    }));
}

#[test]
fn a_slow_cloud_did_preserves_local_round_spacing_and_local_commands() {
    let rig = Rig::new(true, &["one", "multi"]);
    let (release, gate) = flume::bounded(1);
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.online.insert("one".into());
        cloud.read_gates.insert(("one".into(), 2, 1), gate);
    }
    block_on(rig.run(|c| async move {
        until(|| cloud_reads(&c, "one", 2) == 1).await;
        let peer = Rc::new(Peer::default());
        peer.state.borrow_mut().omit.insert("one".into());
        c.announce();
        let control = &c;
        c.peer(peer, |peer| async move {
            initialized(control, &peer, &["multi"]).await;
            control
                .clock
                .automatic
                .store(false, std::sync::atomic::Ordering::Relaxed);
            async_io::Timer::after(Duration::from_millis(20)).await;
            let before = [3, 4, 9].map(|siid| peer.count("get", "multi", siid));
            peer.state.borrow_mut().hold_reads = true;
            let multi = work(control, "multi");
            multi.refresh();

            for (index, siid) in [3, 4, 9].into_iter().enumerate() {
                let deadline = control.clock.now() + Duration::from_millis(200);
                until(|| control.clock.deadlines.lock().unwrap().contains(&deadline)).await;
                control.clock.advance(Duration::from_millis(199));
                async_io::Timer::after(Duration::from_millis(5)).await;
                assert_eq!(peer.count("get", "multi", siid), before[index]);
                control.clock.advance(Duration::from_millis(1));
                until(|| peer.count("get", "multi", siid) == before[index] + 1).await;
                assert_eq!(cloud_reads(control, "one", 2), 1);
                assert_eq!(
                    function(&control.context.registry, "one", "outlet:2")
                        .snapshot()
                        .power(),
                    Some(Power::Unknown)
                );
                if index == 0 {
                    let usb = function(&control.context.registry, "multi", "outlet:9");
                    assert_eq!(
                        usb.execute(Command::On).await.unwrap().power(),
                        Some(Power::On)
                    );
                    assert_eq!(peer.count("rpcReq", "multi", 9), 1);
                    assert!(control.cloud.lock().unwrap().property_writes.is_empty());
                }
                peer.release("multi", siid, json!({"value":false})).await;
                until(|| multi.refresh.borrow().snapshot().inflight.is_empty()).await;
            }

            release.send(()).unwrap();
            until(|| {
                function(&control.context.registry, "one", "outlet:2")
                    .snapshot()
                    .power()
                    == Some(Power::Off)
            })
            .await;
        })
        .await;
    }));
}

#[test]
fn refreshes_during_a_cloud_read_coalesce_and_invalidate_its_old_value() {
    let rig = Rig::new(true, &["one"]);
    let (first_release, first_gate) = flume::bounded(1);
    let (next_release, next_gate) = flume::bounded(1);
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.online.insert("one".into());
        cloud.read_gates.insert(("one".into(), 2, 1), first_gate);
    }
    block_on(rig.run(|c| async move {
        until(|| cloud_reads(&c, "one", 2) == 1).await;
        let one = function(&c.context.registry, "one", "outlet:2");
        let work = work(&c, "one");
        for _ in 0..10 {
            work.refresh();
        }
        assert_eq!(work.refresh.borrow().snapshot().inflight.len(), 1);
        assert_eq!(work.refresh.borrow().snapshot().pending.len(), 1);
        assert_eq!(cloud_reads(&c, "one", 2), 1);
        {
            let mut cloud = c.cloud.lock().unwrap();
            cloud.read_gates.insert(("one".into(), 2, 1), next_gate);
            cloud.read_values.insert(("one".into(), 2, 1), true);
        }

        first_release.send(()).unwrap();
        until(|| cloud_reads(&c, "one", 2) == 2).await;
        assert_eq!(one.snapshot().power(), Some(Power::Unknown));
        assert_eq!(work.refresh.borrow().snapshot().inflight.len(), 1);
        assert!(work.refresh.borrow().snapshot().pending.is_empty());

        next_release.send(()).unwrap();
        until(|| one.snapshot().power() == Some(Power::On)).await;
        until(|| work.refresh.borrow().snapshot().inflight.is_empty()).await;
        assert_eq!(cloud_reads(&c, "one", 2), 2);
        assert!(work.refresh.borrow().snapshot().pending.is_empty());
    }));
}

#[test]
fn switching_to_local_reads_retires_a_held_cloud_read_and_returns_its_demand() {
    let rig = Rig::new(true, &["one"]);
    let (release, gate) = flume::bounded(1);
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.online.insert("one".into());
        cloud.read_gates.insert(("one".into(), 2, 1), gate);
    }
    block_on(rig.run(|c| async move {
        until(|| cloud_reads(&c, "one", 2) == 1).await;
        let before = work(&c, "one");
        let peer = Rc::new(Peer::default());
        peer.state
            .borrow_mut()
            .values
            .insert(("one".into(), 2, 1), true);
        c.announce();
        let control = &c;
        c.peer(peer, |peer| async move {
            initialized(control, &peer, &["one"]).await;
            let one = function(&control.context.registry, "one", "outlet:2");
            assert_eq!(one.snapshot().power(), Some(Power::On));
            assert!(Rc::ptr_eq(&before, &work(control, "one")));
            assert!(peer.count("get", "one", 2) > 0);

            release.send(()).unwrap();
            async_io::Timer::after(Duration::from_millis(20)).await;
            assert_eq!(one.snapshot().power(), Some(Power::On));
            assert_eq!(cloud_reads(control, "one", 2), 1);
        })
        .await;
    }));
}

#[test]
fn retiring_a_source_releases_a_cloud_read_slot_before_its_response_arrives() {
    let rig = Rig::new(true, &["one"]);
    let (old_release, old_gate) = flume::bounded(1);
    let (fresh_release, fresh_gate) = flume::bounded(1);
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.online.insert("one".into());
        cloud.read_gates.insert(("one".into(), 2, 1), old_gate);
    }
    block_on(rig.run(|c| async move {
        until(|| cloud_reads(&c, "one", 2) == 1).await;
        let one = function(&c.context.registry, "one", "outlet:2");
        let work = work(&c, "one");
        {
            let mut cloud = c.cloud.lock().unwrap();
            cloud.read_gates.insert(("one".into(), 2, 1), fresh_gate);
            cloud.read_values.insert(("one".into(), 2, 1), true);
        }
        work.test_source_changed();
        until(|| cloud_reads(&c, "one", 2) == 2).await;
        assert_eq!(one.snapshot().power(), Some(Power::Unknown));
        assert_eq!(work.refresh.borrow().snapshot().inflight.len(), 1);

        old_release.send(()).unwrap();
        async_io::Timer::after(Duration::from_millis(20)).await;
        assert_eq!(one.snapshot().power(), Some(Power::Unknown));
        fresh_release.send(()).unwrap();
        until(|| one.snapshot().power() == Some(Power::On)).await;
        until(|| work.refresh.borrow().snapshot().inflight.is_empty()).await;
        assert_eq!(cloud_reads(&c, "one", 2), 2);
        assert!(work.refresh.borrow().snapshot().pending.is_empty());
    }));
}
