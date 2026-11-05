use super::*;
use crate::storage::RecoveryStop;
use crate::xiaomi::auth::AuthenticationState;

fn set_missing(rig: &Rig, stop: Option<RecoveryStop>) {
    let mut record = rig.store.xiaomi().load().unwrap().unwrap();
    record.mijia = None;
    record.recovery.as_mut().unwrap().stop = stop;
    rig.store.xiaomi().replace(&record).unwrap();
    rig.source.context.adopt_credentials(record).unwrap();
}

#[test]
fn missing_session_recovers_without_probing_old_session() {
    let rig = Rig::new(true, &["one"]);
    rig.enable_recovery();
    set_missing(&rig, None);
    rig.cloud.lock().unwrap().online.insert("one".into());
    block_on(rig.run(|c| async move {
        until(|| c.context.cloud_candidate("one", true).is_ok()).await;
        let record = c.store.xiaomi().load().unwrap().unwrap();
        assert_eq!(record.mijia.unwrap().service_token(), "restored-service");
        let cloud = c.cloud.lock().unwrap();
        assert_eq!(cloud.restores, 1);
        assert_eq!(cloud.sts_requests, 1);
        assert!(
            cloud
                .home_requests
                .iter()
                .all(|(headers, _)| headers.contains("serviceToken=restored-service"))
        );
        assert_eq!(cloud.tokens, 0);
    }));
}

#[test]
fn rejected_read_returns_to_existing_queue_after_recovery() {
    let rig = Rig::new(true, &["one"]);
    rig.enable_recovery();
    rig.cloud.lock().unwrap().online.insert("one".into());
    let (release, gate) = flume::bounded(1);
    rig.cloud.lock().unwrap().restore_gate = Some(gate);
    block_on(rig.run(|c| async move {
        let one = function(&c.context.registry, "one", "outlet:2");
        until(|| one.snapshot().power() == Some(Power::Off)).await;
        c.clock
            .automatic
            .store(false, std::sync::atomic::Ordering::Relaxed);
        let before = c.cloud.lock().unwrap().property_reads.len();
        c.cloud.lock().unwrap().read_status = 401;
        let work = c.context.live.borrow().devices["one"]
            .admitted
            .clone()
            .unwrap();
        work.refresh_property(
            "outlet:2",
            crate::xiaomi::capability::PropertyBinding { siid: 2, piid: 1 },
        );
        c.clock.advance(Duration::from_millis(201));
        until(|| c.cloud.lock().unwrap().restores == 1).await;
        {
            let mut cloud = c.cloud.lock().unwrap();
            cloud.read_status = 200;
            cloud.read_values.insert(("one".into(), 2, 1), true);
        }
        release.send(()).unwrap();
        c.clock.advance(Duration::from_secs(1));
        until(|| one.snapshot().power() == Some(Power::On)).await;
        assert!(c.cloud.lock().unwrap().property_reads.len() >= before + 2);
        assert_eq!(c.cloud.lock().unwrap().restores, 1);
    }));
}

#[test]
fn late_unauthorized_from_identical_old_session_cannot_stop_new_generation() {
    let rig = Rig::new(true, &["one"]);
    rig.enable_recovery();
    rig.cloud.lock().unwrap().online.insert("one".into());
    let (release, gate) = flume::bounded(1);
    block_on(rig.run(|c| async move {
        let one = function(&c.context.registry, "one", "outlet:2");
        until(|| one.snapshot().power() == Some(Power::Off)).await;
        {
            let mut cloud = c.cloud.lock().unwrap();
            cloud.property_status = 401;
            cloud.property_gate = Some(gate);
        }
        let mut command = Box::pin(one.execute(Command::On));
        assert!(poll_once(command.as_mut()).await.is_none());
        until(|| c.cloud.lock().unwrap().property_writes.len() == 1).await;
        let record = c.store.xiaomi().load().unwrap().unwrap();
        c.store.xiaomi().replace(&record).unwrap();
        c.context.adopt_credentials(record).unwrap();
        c.cloud.lock().unwrap().property_status = 200;
        release.send(()).unwrap();
        assert!(command.await.is_err());
        async_io::Timer::after(Duration::from_millis(20)).await;
        assert!(c.context.mijia_rejected.borrow().is_none());
        assert_eq!(c.cloud.lock().unwrap().restores, 0);
        assert_eq!(c.cloud.lock().unwrap().tokens, 0);
        assert!(c.context.cloud_candidate("one", true).is_ok());
    }));
}

#[test]
fn directory_guard_rechecks_database_generation_on_each_poll() {
    let rig = Rig::new(true, &["one"]);
    let path = rig.source.context.mijia_path().unwrap();
    let context = rig.source.context.clone();
    let mut retired = Box::pin(context.mijia_retired(&path));
    block_on(async {
        assert!(poll_once(retired.as_mut()).await.is_none());
        let record = rig.store.xiaomi().load().unwrap().unwrap();
        rig.store.xiaomi().replace(&record).unwrap();
        assert!(poll_once(retired.as_mut()).await.is_some());
    });
}

#[test]
fn replaced_session_cannot_send_the_next_old_directory_page() {
    let rig = Rig::new(true, &["one"]);
    let (release, gate) = flume::bounded(1);
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.home_more = true;
        cloud.home_gate = Some(gate);
    }
    block_on(rig.run(|c| async move {
        until(|| c.cloud.lock().unwrap().homes == 1).await;
        let record = c.store.xiaomi().load().unwrap().unwrap();
        c.store.xiaomi().replace(&record).unwrap();
        {
            let mut cloud = c.cloud.lock().unwrap();
            cloud.home_more = false;
            cloud.home_gate = None;
        }
        release.send(()).unwrap();
        until(|| c.cloud.lock().unwrap().homes >= 2).await;
        async_io::Timer::after(Duration::from_millis(20)).await;
        assert_eq!(c.cloud.lock().unwrap().member_pages, 0);
        assert_eq!(c.cloud.lock().unwrap().restores, 0);
    }));
}

#[test]
fn replaced_session_restarts_an_interrupted_manual_directory_round_without_waiting() {
    let rig = Rig::new(true, &["one"]);
    let (release, gate) = flume::bounded(1);
    block_on(rig.run(|c| async move {
        until(|| c.cloud.lock().unwrap().catalogs > 0 && !c.context.sync_requested.get()).await;
        c.clock
            .automatic
            .store(false, std::sync::atomic::Ordering::Relaxed);
        let before = c.cloud.lock().unwrap().homes;
        {
            let mut cloud = c.cloud.lock().unwrap();
            cloud.home_more = true;
            cloud.home_gate = Some(gate);
        }
        c.context.request_sync();
        until(|| c.cloud.lock().unwrap().homes == before + 1).await;
        let record = c.store.xiaomi().load().unwrap().unwrap();
        c.store.xiaomi().replace(&record).unwrap();
        {
            let mut cloud = c.cloud.lock().unwrap();
            cloud.home_more = false;
            cloud.home_gate = None;
        }
        release.send(()).unwrap();
        until(|| c.cloud.lock().unwrap().homes == before + 2).await;
        assert_eq!(c.cloud.lock().unwrap().member_pages, 0);
    }));
}

#[test]
fn stopped_recovery_survives_restart_without_disabling_a_valid_session() {
    for has_session in [false, true] {
        let rig = Rig::new(true, &["one"]);
        rig.enable_recovery();
        let mut record = rig.store.xiaomi().load().unwrap().unwrap();
        if !has_session {
            record.mijia = None;
        }
        record.recovery.as_mut().unwrap().stop = Some(RecoveryStop::Rejected);
        rig.store.xiaomi().replace(&record).unwrap();
        let (adapter, _) = XiaomiAdapter::build(
            rig.store.clone(),
            rig.source.cloud.clone(),
            rig.discovery.clone(),
            rig.clock.clone(),
        )
        .unwrap();
        rig.cloud.lock().unwrap().online.insert("one".into());
        block_on(future::or(adapter.run(|_| {}), async {
            async_io::Timer::after(Duration::from_millis(80)).await;
            Ok(())
        }))
        .unwrap();
        let cloud = rig.cloud.lock().unwrap();
        assert_eq!(cloud.restores, 0);
        assert_eq!(cloud.homes > 0, has_session);
        assert_eq!(
            rig.store
                .xiaomi()
                .load()
                .unwrap()
                .unwrap()
                .recovery
                .unwrap()
                .stop,
            Some(RecoveryStop::Rejected)
        );
    }
}

#[test]
fn committed_login_clears_stale_recovery_stop_before_new_session_is_reported_ready() {
    let rig = Rig::new(true, &["one"]);
    rig.enable_recovery();
    let session = rig.store.xiaomi().load().unwrap().unwrap().mijia.unwrap();
    set_missing(&rig, Some(RecoveryStop::Rejected));
    let (_release, gate) = flume::bounded(1);
    rig.cloud.lock().unwrap().home_gate = Some(gate);
    block_on(rig.run(|c| async move {
        until(|| c.context.status.borrow().auth.recovery_stop == Some(RecoveryStop::Rejected))
            .await;
        let mut replacement = c.store.xiaomi().load().unwrap().unwrap();
        replacement.mijia = Some(session);
        replacement.recovery.as_mut().unwrap().stop = None;
        c.store.xiaomi().replace(&replacement).unwrap();
        c.context.adopt_credentials(replacement).unwrap();
        let report = c.context.status.borrow().auth.clone();
        assert_eq!(report.recovery_stop, None);
        assert_eq!(report.mijia, AuthenticationState::Checking);
    }));
}

#[test]
fn temporary_recovery_failure_backs_off_without_stopping_credentials() {
    let rig = Rig::new(true, &["one"]);
    rig.enable_recovery();
    set_missing(&rig, None);
    rig.clock
        .automatic
        .store(false, std::sync::atomic::Ordering::Relaxed);
    rig.cloud.lock().unwrap().restore_status = 503;
    block_on(rig.run(|c| async move {
        until(|| {
            c.cloud.lock().unwrap().restores == 1
                && matches!(
                    c.context.status.borrow().auth.mijia,
                    AuthenticationState::Unavailable(_)
                )
        })
        .await;
        until(|| {
            c.clock
                .deadlines
                .lock()
                .unwrap()
                .contains(&Duration::from_secs(1))
        })
        .await;
        c.context.credentials_changed.notify(usize::MAX);
        async_io::Timer::after(Duration::from_millis(20)).await;
        assert_eq!(c.cloud.lock().unwrap().restores, 1);
        c.clock.advance(Duration::from_millis(999));
        async_io::Timer::after(Duration::from_millis(10)).await;
        assert_eq!(c.cloud.lock().unwrap().restores, 1);
        c.clock.advance(Duration::from_millis(1));
        until(|| c.cloud.lock().unwrap().restores == 2).await;
        assert_eq!(
            c.store
                .xiaomi()
                .load()
                .unwrap()
                .unwrap()
                .recovery
                .unwrap()
                .stop,
            None
        );
    }));
}

#[test]
fn signature_failure_does_not_attempt_account_recovery() {
    let rig = Rig::new(true, &["one"]);
    rig.enable_recovery();
    rig.cloud.lock().unwrap().home_bad_signature = true;
    block_on(rig.run(|c| async move {
        until(|| {
            matches!(
                c.context.status.borrow().auth.mijia,
                AuthenticationState::Unavailable(_)
            )
        })
        .await;
        c.clock.advance(Duration::from_secs(3));
        async_io::Timer::after(Duration::from_millis(20)).await;
        let cloud = c.cloud.lock().unwrap();
        assert_eq!(cloud.restores, 0);
        assert_eq!(cloud.tokens, 0);
        assert_eq!(
            c.store
                .xiaomi()
                .load()
                .unwrap()
                .unwrap()
                .recovery
                .unwrap()
                .stop,
            None
        );
    }));
}

#[test]
fn miot_description_service_unauthorized_does_not_recover_mijia_session() {
    let rig = Rig::new(true, &["one"]);
    rig.enable_recovery();
    let urn = "urn:miot-spec-v2:device:outlet:0000A002:vendor-new:1";
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.online.insert("one".into());
        cloud.instance_types.insert("one".into(), urn.into());
        cloud.instance_status.insert(urn.into(), 401);
    }
    block_on(rig.run(|c| async move {
        until(|| {
            c.cloud
                .lock()
                .unwrap()
                .instance_reads
                .get(urn)
                .copied()
                .unwrap_or(0)
                > 0
        })
        .await;
        async_io::Timer::after(Duration::from_millis(20)).await;
        assert_eq!(c.cloud.lock().unwrap().restores, 0);
        assert_eq!(c.cloud.lock().unwrap().tokens, 0);
        assert!(c.context.mijia_rejected.borrow().is_none());
        assert!(!c.context.http_paused.get());
    }));
}

#[test]
fn account_protocol_failure_does_not_repeat_on_unrelated_credential_events() {
    let rig = Rig::new(true, &["one"]);
    rig.enable_recovery();
    set_missing(&rig, None);
    rig.clock
        .automatic
        .store(false, std::sync::atomic::Ordering::Relaxed);
    rig.cloud.lock().unwrap().restore_status = 422;
    block_on(rig.run(|c| async move {
        until(|| {
            c.cloud.lock().unwrap().restores == 1
                && matches!(
                    c.context.status.borrow().auth.mijia,
                    AuthenticationState::Unavailable(_)
                )
        })
        .await;
        c.context.credentials_changed.notify(usize::MAX);
        c.clock.advance(Duration::from_secs(3600));
        async_io::Timer::after(Duration::from_millis(20)).await;
        assert_eq!(c.cloud.lock().unwrap().restores, 1);
        assert_eq!(
            c.store
                .xiaomi()
                .load()
                .unwrap()
                .unwrap()
                .recovery
                .unwrap()
                .stop,
            None
        );
    }));
}
