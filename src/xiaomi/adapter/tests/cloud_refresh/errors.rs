use super::*;

#[test]
fn a_switch_keeps_a_failed_read_and_retry_after_a_new_notification() {
    use crate::xiaomi::capability::PropertyBinding;

    let (rig, listener, peer) = with_cloud_peer(&["one"]);
    let (release, gate) = flume::bounded(1);
    block_on(rig.run(|c| async move {
        future::or(peer.run(&listener), async {
            until(|| {
                cloud_active(&c, "one")
                    && function(&c.context.registry, "one", "outlet:2")
                        .snapshot()
                        .power()
                        == Some(Power::Off)
                    && work(&c, "one")
                        .refresh
                        .borrow()
                        .snapshot()
                        .inflight
                        .is_empty()
            })
            .await;
            c.clock
                .automatic
                .store(false, std::sync::atomic::Ordering::Relaxed);
            {
                let mut cloud = c.cloud.lock().unwrap();
                cloud.read_status = 503;
                cloud.read_gates.insert(("one".into(), 2, 1), gate);
            }
            let binding = PropertyBinding { siid: 2, piid: 1 };
            let one_work = work(&c, "one");
            let one = function(&c.context.registry, "one", "outlet:2");
            let before = cloud_reads(&c, "one", 2);
            one_work.refresh_property("outlet:2", binding);
            c.clock.advance(Duration::from_millis(201));
            until(|| cloud_reads(&c, "one", 2) == before + 1).await;
            peer.property("one", 2, true);
            until(|| one.snapshot().power() == Some(Power::On)).await;
            {
                let mut cloud = c.cloud.lock().unwrap();
                cloud.read_status = 200;
                cloud.read_gates.remove(&("one".into(), 2, 1));
                cloud.read_values.insert(("one".into(), 2, 1), true);
            }
            release.send(()).unwrap();
            until(|| {
                one_work
                    .refresh
                    .borrow()
                    .property_problem("outlet:2", binding)
                    .is_some()
            })
            .await;
            assert!(one_work.refresh.borrow().contains("outlet:2", binding));
            assert_eq!(one.snapshot().power(), Some(Power::On));
            assert!(!c.context.http_paused.get());
            c.clock.advance(Duration::from_secs(3));
            until(|| {
                cloud_reads(&c, "one", 2) == before + 2
                    && one_work
                        .refresh
                        .borrow()
                        .property_problem("outlet:2", binding)
                        .is_none()
            })
            .await;
            assert_eq!(one.snapshot().power(), Some(Power::On));
        })
        .await;
    }));
}

#[test]
fn foreground_cloud_read_failure_preserves_other_control_and_live_notifications() {
    use crate::xiaomi::capability::PropertyBinding;

    for timeout in [false, true] {
        let (rig, listener, peer) = with_cloud_peer(&["one", "two"]);
        let (release, gate) = flume::bounded(1);
        block_on(rig.run(|c| async move {
            future::or(peer.run(&listener), async {
                until(|| {
                    cloud_active(&c, "one")
                        && cloud_active(&c, "two")
                        && function(&c.context.registry, "one", "outlet:2")
                            .snapshot()
                            .power()
                            == Some(Power::Off)
                        && function(&c.context.registry, "two", "outlet:2")
                            .snapshot()
                            .power()
                            == Some(Power::Off)
                        && work(&c, "one")
                            .refresh
                            .borrow()
                            .snapshot()
                            .inflight
                            .is_empty()
                })
                .await;
                c.clock
                    .automatic
                    .store(false, std::sync::atomic::Ordering::Relaxed);
                let one = function(&c.context.registry, "one", "outlet:2");
                peer.property("one", 2, true);
                until(|| one.snapshot().power() == Some(Power::On)).await;
                {
                    let mut cloud = c.cloud.lock().unwrap();
                    if timeout {
                        cloud.read_gates.insert(("one".into(), 2, 1), gate);
                    } else {
                        cloud.read_status = 503;
                    }
                }
                let binding = PropertyBinding { siid: 2, piid: 1 };
                let one_work = work(&c, "one");
                let before = cloud_reads(&c, "one", 2);
                one_work.refresh_property("outlet:2", binding);
                c.clock.advance(Duration::from_millis(201));
                until(|| {
                    cloud_reads(&c, "one", 2) > before
                        && one_work
                            .refresh
                            .borrow()
                            .property_problem("outlet:2", binding)
                            .is_some()
                })
                .await;
                let problem = one_work
                    .refresh
                    .borrow()
                    .property_problem("outlet:2", binding)
                    .unwrap();
                assert!(
                    problem.contains(if timeout { "timed out" } else { "503" }),
                    "{problem}"
                );
                assert_eq!(one.snapshot().power(), Some(Power::On));
                assert!(!c.context.http_paused.get());
                assert!(c.context.cloud_candidate("one", true).is_ok());
                assert!(c.context.cloud_candidate("two", true).is_ok());
                assert!(cloud_active(&c, "one"));
                assert!(cloud_active(&c, "two"));
                function(&c.context.registry, "two", "outlet:2")
                    .execute(Command::On)
                    .await
                    .unwrap();
                assert_eq!(c.cloud.lock().unwrap().property_writes.len(), 1);
                peer.property("one", 2, false);
                until(|| one.snapshot().power() == Some(Power::Off)).await;
                drop(release);
            })
            .await;
        }));
    }
}

#[test]
fn foreground_cloud_read_device_rejection_only_revokes_the_affected_device() {
    use crate::xiaomi::capability::PropertyBinding;

    for code in [-704_042_011, -704_010_000, -704_012_904, -704_012_905] {
        let rig = Rig::new(true, &["one", "two"]);
        rig.cloud
            .lock()
            .unwrap()
            .online
            .extend(["one".into(), "two".into()]);
        block_on(rig.run(|c| async move {
            until(|| {
                c.context.cloud_candidate("one", true).is_ok()
                    && c.context.cloud_candidate("two", true).is_ok()
                    && work(&c, "one")
                        .refresh
                        .borrow()
                        .snapshot()
                        .inflight
                        .is_empty()
            })
            .await;
            c.clock
                .automatic
                .store(false, std::sync::atomic::Ordering::Relaxed);
            c.cloud
                .lock()
                .unwrap()
                .read_codes
                .insert(("one".into(), 2, 1), code);
            let before = cloud_reads(&c, "one", 2);
            work(&c, "one").refresh_property("outlet:2", PropertyBinding { siid: 2, piid: 1 });
            c.clock.advance(Duration::from_millis(201));
            until(|| {
                cloud_reads(&c, "one", 2) > before
                    && c.context.cloud_candidate("one", true).is_err()
            })
            .await;
            assert!(c.context.cloud_candidate("two", true).is_ok());
            assert!(!c.context.http_paused.get());
            let live = c.context.live.borrow();
            let slot = &live.devices["one"];
            if code == -704_042_011 {
                assert!(!slot.cloud_access.as_ref().unwrap().online);
            } else {
                assert!(slot.cloud_forbidden);
            }
        }));
    }
}

#[test]
fn foreground_cloud_read_unauthorized_rejects_current_credentials() {
    use crate::xiaomi::capability::PropertyBinding;

    let rig = Rig::new(true, &["one"]);
    rig.cloud.lock().unwrap().online.insert("one".into());
    let (release, gate) = flume::bounded(1);
    block_on(rig.run(|c| async move {
        until(|| {
            c.context.cloud_candidate("one", true).is_ok()
                && work(&c, "one")
                    .refresh
                    .borrow()
                    .snapshot()
                    .inflight
                    .is_empty()
        })
        .await;
        c.clock
            .automatic
            .store(false, std::sync::atomic::Ordering::Relaxed);
        let token = c
            .context
            .live
            .borrow()
            .credentials
            .as_ref()
            .unwrap()
            .tokens
            .access_token
            .clone();
        {
            let mut cloud = c.cloud.lock().unwrap();
            cloud.read_status = 401;
            cloud.token_gate = Some(gate);
        }
        let before = c.cloud.lock().unwrap().tokens;
        work(&c, "one").refresh_property("outlet:2", PropertyBinding { siid: 2, piid: 1 });
        c.clock.advance(Duration::from_millis(201));
        until(|| c.cloud.lock().unwrap().tokens > before).await;
        assert!(matches!(
            &*c.context.cloud_sync.borrow(),
            super::super::cloud::CloudSync::Rejected(rejected) if rejected == &token
        ));
        assert!(c.context.cloud_candidate("one", true).is_err());
        assert!(!c.context.http_paused.get());
        drop(release);
    }));
}
