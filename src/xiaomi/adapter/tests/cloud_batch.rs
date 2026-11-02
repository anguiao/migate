use super::*;
use crate::xiaomi::capability::PropertyBinding;
use std::sync::atomic::Ordering;

fn work(c: &Control, did: &str) -> Rc<super::super::work::DeviceWork> {
    c.context.live.borrow().devices[did]
        .admitted
        .as_ref()
        .unwrap()
        .clone()
}

async fn settled(c: &Control, dids: &[&str]) {
    until(|| {
        let live = c.context.live.borrow();
        let cloud = c.cloud.lock().unwrap();
        dids.iter().all(|did| {
            live.devices
                .get(*did)
                .and_then(|slot| slot.admitted.as_ref())
                .is_some_and(|work| {
                    let refresh = work.refresh.borrow().snapshot();
                    refresh.inflight.is_empty()
                        && refresh.pending.is_empty()
                        && cloud.property_reads.iter().any(|(id, _, _)| id == did)
                })
        })
    })
    .await;
}

#[test]
fn cloud_reads_limit_each_device_and_space_function_batches() {
    let rig = Rig::new(true, &["multi"]);
    rig.cloud.lock().unwrap().online.insert("multi".into());
    block_on(rig.run(|c| async move {
        settled(&c, &["multi"]).await;
        c.clock.automatic.store(false, Ordering::Relaxed);
        let mut releases = BTreeMap::new();
        {
            let mut cloud = c.cloud.lock().unwrap();
            for siid in [3, 4, 9] {
                let (release, gate) = flume::bounded(1);
                cloud.read_gates.insert(("multi".into(), siid, 1), gate);
                releases.insert(siid, release);
            }
        }
        let before = c.cloud.lock().unwrap().read_batches.len();
        let work = work(&c, "multi");
        for siid in [3, 4, 9] {
            work.refresh_property(&format!("outlet:{siid}"), PropertyBinding { siid, piid: 1 });
        }
        c.clock.advance(Duration::from_millis(201));
        until(|| c.cloud.lock().unwrap().read_batches.len() == before + 1).await;
        let first_start = c.clock.now();
        until(|| {
            c.clock
                .deadlines
                .lock()
                .unwrap()
                .contains(&(first_start + Duration::from_millis(200)))
        })
        .await;
        c.clock.advance(Duration::from_millis(199));
        async_io::Timer::after(Duration::from_millis(20)).await;
        assert_eq!(c.cloud.lock().unwrap().read_batches.len(), before + 1);
        c.clock.advance(Duration::from_millis(1));
        until(|| c.cloud.lock().unwrap().read_batches.len() == before + 2).await;

        c.clock.advance(Duration::from_millis(201));
        async_io::Timer::after(Duration::from_millis(20)).await;
        assert_eq!(c.cloud.lock().unwrap().read_batches.len(), before + 2);
        let first_siid = c.cloud.lock().unwrap().read_batches[before][0].1;
        releases[&first_siid].send(()).unwrap();
        until(|| c.cloud.lock().unwrap().read_batches.len() == before + 3).await;
    }));
}

#[test]
fn cloud_reads_limit_total_flights_without_blocking_other_devices_forever() {
    let dids = [
        "device00", "device01", "device02", "device03", "device04", "device05", "device06",
        "device07", "device08",
    ];
    let rig = Rig::new(true, &dids);
    rig.cloud
        .lock()
        .unwrap()
        .online
        .extend(dids.iter().map(|did| (*did).into()));
    block_on(rig.run(|c| async move {
        settled(&c, &dids).await;
        c.clock.automatic.store(false, Ordering::Relaxed);
        let mut releases = BTreeMap::new();
        {
            let mut cloud = c.cloud.lock().unwrap();
            for did in dids {
                let (release, gate) = flume::bounded(1);
                cloud.read_gates.insert((did.into(), 2, 1), gate);
                releases.insert(did, release);
            }
        }
        let before = c.cloud.lock().unwrap().read_batches.len();
        for did in dids {
            work(&c, did).refresh_property("outlet:2", PropertyBinding { siid: 2, piid: 1 });
        }
        c.clock.advance(Duration::from_millis(201));
        until(|| c.cloud.lock().unwrap().read_batches.len() == before + 8).await;
        async_io::Timer::after(Duration::from_millis(20)).await;
        let started = c.cloud.lock().unwrap().read_batches[before..]
            .iter()
            .map(|batch| batch[0].0.clone())
            .collect::<BTreeSet<_>>();
        assert_eq!(started.len(), 8);
        assert_eq!(c.cloud.lock().unwrap().read_batches.len(), before + 8);
        releases[started.first().unwrap().as_str()]
            .send(())
            .unwrap();
        until(|| c.cloud.lock().unwrap().read_batches.len() == before + 9).await;
    }));
}

#[test]
fn a_slow_periodic_read_does_not_block_another_device() {
    let dids = ["one", "two"];
    let rig = Rig::new(true, &dids);
    rig.cloud
        .lock()
        .unwrap()
        .online
        .extend(dids.iter().map(|did| (*did).into()));
    block_on(rig.run(|c| async move {
        settled(&c, &dids).await;
        c.clock.automatic.store(false, Ordering::Relaxed);
        let mut releases = BTreeMap::new();
        {
            let mut cloud = c.cloud.lock().unwrap();
            for did in dids {
                let (release, gate) = flume::bounded(1);
                cloud.read_gates.insert((did.into(), 2, 1), gate);
                cloud.read_values.insert((did.into(), 2, 1), true);
                releases.insert(did, release);
            }
        }
        let before = c.cloud.lock().unwrap().read_batches.len();
        c.clock.advance(Duration::from_secs(61));
        until(|| c.cloud.lock().unwrap().read_batches.len() == before + 1).await;
        c.clock.advance(Duration::from_millis(200));
        until(|| c.cloud.lock().unwrap().read_batches.len() == before + 2).await;
        let first = c.cloud.lock().unwrap().read_batches[before][0].0.clone();
        let second = c.cloud.lock().unwrap().read_batches[before + 1][0]
            .0
            .clone();
        assert_ne!(first, second);
        releases[second.as_str()].send(()).unwrap();
        until(|| {
            function(&c.context.registry, &second, "outlet:2")
                .snapshot()
                .power()
                == Some(Power::On)
        })
        .await;
        assert_eq!(
            function(&c.context.registry, &first, "outlet:2")
                .snapshot()
                .power(),
            Some(Power::Off)
        );
        releases[first.as_str()].send(()).unwrap();
    }));
}

#[test]
fn malformed_periodic_response_preserves_correlated_offline_error() {
    let rig = Rig::new(true, &["one"]);
    rig.cloud.lock().unwrap().online.insert("one".into());
    block_on(rig.run(|c| async move {
        settled(&c, &["one"]).await;
        c.clock.automatic.store(false, Ordering::Relaxed);
        c.cloud.lock().unwrap().read_results = Some(vec![
            json!({"did":"one","siid":2,"piid":1,"code":-704_042_011}),
            json!({"did":"foreign","siid":2,"piid":1,"code":0,"value":true}),
        ]);
        let before = c.cloud.lock().unwrap().read_batches.len();
        c.clock.advance(Duration::from_secs(61));
        until(|| {
            c.cloud.lock().unwrap().read_batches.len() > before
                && c.context.cloud_candidate("one", true).is_err()
        })
        .await;
        assert!(
            !c.context.live.borrow().devices["one"]
                .cloud_access
                .as_ref()
                .unwrap()
                .online
        );
        assert!(!c.context.http_paused.get());
    }));
}

#[test]
fn old_periodic_success_does_not_reset_a_newer_rate_limit_backoff() {
    let dids = ["one", "two", "three"];
    let rig = Rig::new(true, &dids);
    rig.cloud
        .lock()
        .unwrap()
        .online
        .extend(dids.iter().map(|did| (*did).into()));
    block_on(rig.run(|c| async move {
        settled(&c, &dids).await;
        c.clock.automatic.store(false, Ordering::Relaxed);
        let mut releases = BTreeMap::new();
        {
            let mut cloud = c.cloud.lock().unwrap();
            for did in dids {
                let (release, gate) = flume::bounded(1);
                cloud.read_gates.insert((did.into(), 2, 1), gate);
                releases.insert(did, release);
            }
        }
        let before = c.cloud.lock().unwrap().read_batches.len();
        c.clock.advance(Duration::from_secs(61));
        until(|| c.cloud.lock().unwrap().read_batches.len() == before + 1).await;
        let first = c.cloud.lock().unwrap().read_batches[before][0].0.clone();
        c.cloud.lock().unwrap().read_status = 429;
        c.clock.advance(Duration::from_millis(200));
        until(|| c.cloud.lock().unwrap().read_batches.len() == before + 2).await;
        let second = c.cloud.lock().unwrap().read_batches[before + 1][0]
            .0
            .clone();
        releases[second.as_str()].send(()).unwrap();
        let binding = PropertyBinding { siid: 2, piid: 1 };
        until(|| {
            work(&c, &second)
                .refresh
                .borrow()
                .property_problem("outlet:2", binding)
                .is_some()
        })
        .await;
        releases[first.as_str()].send(()).unwrap();
        until(|| {
            !work(&c, &first)
                .refresh
                .borrow()
                .contains("outlet:2", binding)
        })
        .await;
        c.cloud.lock().unwrap().read_gates.clear();

        c.clock.advance(Duration::from_secs(30));
        until(|| c.cloud.lock().unwrap().read_batches.len() == before + 3).await;
        let third = c.cloud.lock().unwrap().read_batches[before + 2][0]
            .0
            .clone();
        until(|| {
            work(&c, &third)
                .refresh
                .borrow()
                .property_problem("outlet:2", binding)
                .is_some()
        })
        .await;
        c.clock.advance(Duration::from_secs(31));
        async_io::Timer::after(Duration::from_millis(20)).await;
        assert_eq!(c.cloud.lock().unwrap().read_batches.len(), before + 3);
        c.clock.advance(Duration::from_secs(30));
        until(|| c.cloud.lock().unwrap().read_batches.len() > before + 3).await;
    }));
}

#[test]
fn concurrent_periodic_failures_back_off_once_then_advance_next_generation() {
    let dids = ["one", "two", "three"];
    let rig = Rig::new(true, &dids);
    rig.cloud
        .lock()
        .unwrap()
        .online
        .extend(dids.iter().map(|did| (*did).into()));
    block_on(rig.run(|c| async move {
        settled(&c, &dids).await;
        c.clock.automatic.store(false, Ordering::Relaxed);
        let mut releases = BTreeMap::new();
        {
            let mut cloud = c.cloud.lock().unwrap();
            cloud.read_status = 429;
            for did in dids {
                let (release, gate) = flume::bounded(1);
                cloud.read_gates.insert((did.into(), 2, 1), gate);
                releases.insert(did, release);
            }
        }
        let before = c.cloud.lock().unwrap().read_batches.len();
        c.clock.advance(Duration::from_secs(61));
        until(|| c.cloud.lock().unwrap().read_batches.len() == before + 1).await;
        c.clock.advance(Duration::from_millis(200));
        until(|| c.cloud.lock().unwrap().read_batches.len() == before + 2).await;
        let first = c.cloud.lock().unwrap().read_batches[before][0].0.clone();
        let second = c.cloud.lock().unwrap().read_batches[before + 1][0]
            .0
            .clone();
        let binding = PropertyBinding { siid: 2, piid: 1 };
        releases[first.as_str()].send(()).unwrap();
        until(|| {
            work(&c, &first)
                .refresh
                .borrow()
                .property_problem("outlet:2", binding)
                .is_some()
        })
        .await;
        c.clock.advance(Duration::from_secs(10));
        releases[second.as_str()].send(()).unwrap();
        until(|| {
            work(&c, &second)
                .refresh
                .borrow()
                .property_problem("outlet:2", binding)
                .is_some()
        })
        .await;
        c.cloud.lock().unwrap().read_gates.clear();

        // The late failure from the original wave must not extend 30 seconds.
        c.clock.advance(Duration::from_secs(20));
        until(|| c.cloud.lock().unwrap().read_batches.len() == before + 3).await;
        let third = c.cloud.lock().unwrap().read_batches[before + 2][0]
            .0
            .clone();
        until(|| {
            work(&c, &third)
                .refresh
                .borrow()
                .property_problem("outlet:2", binding)
                .is_some()
        })
        .await;
        // The new wave failed too, so the next cooldown is 60 seconds.
        c.clock.advance(Duration::from_secs(31));
        async_io::Timer::after(Duration::from_millis(20)).await;
        assert_eq!(c.cloud.lock().unwrap().read_batches.len(), before + 3);
        c.clock.advance(Duration::from_secs(30));
        until(|| c.cloud.lock().unwrap().read_batches.len() > before + 3).await;
    }));
}
