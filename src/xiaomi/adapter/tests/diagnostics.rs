use super::*;
use crate::xiaomi::{
    adapter::{
        PathKind, PathPriority, RecoveryStatus,
        state::{AccessRefresh, AccessRequest},
    },
    central::{CentralError, RequestOutcome, RequestStage, RequestTimeout},
};

#[test]
fn recovery_diagnostics_show_only_a_real_schedule_or_a_running_round() {
    block_on(async {
        let clock = TestClock::default();
        let access = AccessRefresh::default();
        assert_eq!(access.recovery_status(&clock), RecoveryStatus::None);
        access.degrade(clock.now());
        assert_eq!(
            access.recovery_status(&clock),
            RecoveryStatus::Scheduled(clock.wall() + 60)
        );
        clock.advance(Duration::from_secs(60));
        assert!(matches!(access.next(&clock).await, AccessRequest::Recovery));
        assert_eq!(access.recovery_status(&clock), RecoveryStatus::Running);
        clock.advance(Duration::from_secs(10));
        access.recovery_finished(clock.now());
        assert_eq!(
            access.recovery_status(&clock),
            RecoveryStatus::Scheduled(clock.wall() + 300)
        );
        access.recovered();
        assert_eq!(access.recovery_status(&clock), RecoveryStatus::None);
    });
}

#[test]
fn diagnostics_keep_current_control_and_valid_notification_conditions_separate() {
    let rig = Rig::new(true, &["one"]);
    rig.cloud.lock().unwrap().online.insert("one".into());
    block_on(rig.run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(Rc::new(Peer::default()), |peer| async move {
            initialized(control, &peer, &["one"]).await;
            until(|| {
                control.context.cloud_scope_valid.get()
                    && control.context.live.borrow().devices["one"]
                        .work
                        .as_ref()
                        .unwrap()
                        .active
                        .borrow()
                        .is_some()
            })
            .await;
            control
                .clock
                .automatic
                .store(false, std::sync::atomic::Ordering::Relaxed);
            let handle = control
                .context
                .live
                .borrow()
                .session
                .as_ref()
                .unwrap()
                .handle
                .clone();
            control.context.local_outcome(
                &handle,
                &RequestOutcome::<()> {
                    result: Err(CentralError::Timeout),
                    stage: RequestStage::MayHaveBeenSent,
                    timeout: Some(RequestTimeout::Response),
                },
            );
            let status = control.context.diagnostics();
            assert!(status.routing.local_connected);
            assert!(status.routing.business_degraded);
            assert_eq!(status.routing.priority, PathPriority::CloudFirst);
            assert_eq!(
                status.routing.recovery,
                RecoveryStatus::Scheduled(control.clock.wall() + 60)
            );
            let one = &status.devices[0];
            assert_eq!(one.did, "one");
            assert_eq!(one.usable_control_paths, [PathKind::Local, PathKind::Cloud]);
            assert_eq!(one.selected_control, Some(PathKind::Cloud));
            assert_eq!(one.notification_source, Some(PathKind::Local));
            assert!(one.sync_limited_reason.is_none());
            control.context.http_paused.set(true);
            let status = control.context.diagnostics();
            assert_eq!(status.devices[0].selected_control, Some(PathKind::Local));
            assert_eq!(status.devices[0].usable_control_paths, [PathKind::Local]);
            assert_eq!(status.devices[0].notification_source, Some(PathKind::Local));
            assert!(status.devices[0].unavailable_reason.is_none());
            assert!(status.devices[0].sync_limited_reason.is_none());
        })
        .await;
    }));
}

#[test]
fn cloud_control_without_ready_notifications_is_sync_limited() {
    let rig = Rig::new(true, &["one"]);
    rig.cloud.lock().unwrap().online.insert("one".into());
    block_on(rig.run(|c| async move {
        until(|| {
            function(&c.context.registry, "one", "main")
                .snapshot()
                .availability
                == Availability::Available
        })
        .await;
        let status = c.context.diagnostics();
        assert!(!status.routing.local_connected);
        assert_eq!(status.devices.len(), 1);
        let one = &status.devices[0];
        assert_eq!(one.selected_control, Some(PathKind::Cloud));
        assert_eq!(one.notification_source, None);
        assert!(one.unavailable_reason.is_none());
        assert!(
            one.sync_limited_reason
                .as_ref()
                .unwrap()
                .contains("notification")
        );
    }));
}

#[test]
fn command_diagnostics_keep_inflight_actual_path_when_control_selection_changes() {
    let mut rig = Rig::new(true, &["one"]);
    rig.source.request_timeout = Duration::from_secs(2);
    rig.cloud.lock().unwrap().online.insert("one".into());
    block_on(rig.run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(Rc::new(Peer::default()), |peer| async move {
            initialized(control, &peer, &["one"]).await;
            until(|| control.context.cloud_scope_valid.get()).await;
            peer.state.borrow_mut().hold_writes = true;
            let one = function(&control.context.registry, "one", "main");
            let mut command = Box::pin(one.execute(Command::On));
            assert!(poll_once(command.as_mut()).await.is_none());
            until(|| peer.count("rpcReq", "one", 2) == 1).await;
            let handle = control
                .context
                .live
                .borrow()
                .session
                .as_ref()
                .unwrap()
                .handle
                .clone();
            control.context.local_outcome(
                &handle,
                &RequestOutcome::<()> {
                    result: Err(CentralError::Timeout),
                    stage: RequestStage::MayHaveBeenSent,
                    timeout: Some(RequestTimeout::Response),
                },
            );
            let status = control.context.diagnostics();
            let one_status = &status.devices[0];
            assert_eq!(one_status.selected_control, Some(PathKind::Cloud));
            let inflight = one_status.commands[0].inflight.as_ref().unwrap();
            let id = inflight.id;
            assert_eq!(inflight.actual_path, Some(PathKind::Local));
            peer.release("one", 2, accepted("one", 2, 0)).await;
            command.await.unwrap();
            let status = control.context.diagnostics();
            let channel = &status.devices[0].commands[0];
            assert!(channel.inflight.is_none());
            let last = channel.last.as_ref().unwrap();
            assert_eq!(last.id, id);
            assert_eq!(last.actual_path, Some(PathKind::Local));
            assert_eq!(last.result, Some(Ok(())));
            one.execute(Command::Off).await.unwrap();
            let status = control.context.diagnostics();
            let last = status.devices[0].commands[0].last.as_ref().unwrap();
            assert!(last.id > id);
            assert_eq!(last.actual_path, Some(PathKind::Cloud));
            assert_eq!(peer.count("rpcReq", "one", 2), 1);
            assert_eq!(control.cloud.lock().unwrap().property_writes.len(), 1);
        })
        .await;
    }));
}

#[test]
fn command_diagnostics_keep_cloud_rejection_results_without_replay() {
    let rig = Rig::new(true, &["one"]);
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.online.insert("one".into());
        cloud.property_code = -123;
    }
    block_on(rig.run(|c| async move {
        let one = function(&c.context.registry, "one", "main");
        until(|| one.snapshot().availability == Availability::Available).await;
        let result = one.execute(Command::On).await;
        assert_eq!(
            result,
            Err(crate::device::CommandError::Rejected(
                "cloud code -123".into()
            ))
        );
        let status = c.context.diagnostics();
        let channel = &status.devices[0].commands[0];
        assert!(channel.inflight.is_none());
        let last = channel.last.as_ref().unwrap();
        assert_eq!(last.actual_path, Some(PathKind::Cloud));
        assert_eq!(
            last.result,
            Some(Err(crate::device::CommandError::Rejected(
                "cloud code -123".into()
            )))
        );
        assert_eq!(c.cloud.lock().unwrap().property_writes.len(), 1);
    }));
}

#[test]
fn switch_reasons_identify_http_failure_instead_of_a_generic_access_change() {
    let rig = Rig::new(true, &["one"]);
    rig.clock
        .automatic
        .store(false, std::sync::atomic::Ordering::Relaxed);
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.online.insert("one".into());
        cloud.property_status = 503;
    }
    block_on(rig.run(|c| async move {
        let one = function(&c.context.registry, "one", "main");
        until(|| one.snapshot().availability == Availability::Available).await;
        assert!(one.execute(Command::On).await.is_err());
        let status = c.context.diagnostics();
        assert!(
            status.devices[0]
                .last_switch_reason
                .as_ref()
                .unwrap()
                .contains("cloud HTTP request failed")
        );
        assert!(
            status.devices[0]
                .unavailable_reason
                .as_ref()
                .unwrap()
                .contains("HTTP temporarily paused")
        );
    }));
}

#[test]
fn switch_reasons_identify_token_expiry_and_saved_credential_restoration() {
    let rig = Rig::new(true, &["one"]);
    rig.clock
        .automatic
        .store(false, std::sync::atomic::Ordering::Relaxed);
    let mut record = rig.store.xiaomi().load().unwrap().unwrap();
    record.tokens.refresh_at = rig.clock.wall() + 5;
    record.tokens.expires_at = rig.clock.wall() + 10;
    rig.store.xiaomi().replace(&record).unwrap();
    rig.source.context.live.borrow_mut().credentials = Some(record);
    let (release, gate) = flume::bounded(1);
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.online.insert("one".into());
        cloud.token_gate = Some(gate);
    }
    block_on(rig.run(|c| async move {
        let one = function(&c.context.registry, "one", "main");
        until(|| one.snapshot().availability == Availability::Available).await;
        c.clock.advance(Duration::from_secs(5));
        until(|| c.cloud.lock().unwrap().tokens == 1).await;
        c.clock.advance(Duration::from_secs(5));
        until(|| !c.context.token_usable()).await;
        let status = c.context.diagnostics();
        assert!(
            status.devices[0]
                .last_switch_reason
                .as_ref()
                .unwrap()
                .contains("access token expired")
        );
        c.cloud.lock().unwrap().token_gate = None;
        release.send(()).unwrap();
        until(|| {
            c.context.token_usable() && one.snapshot().availability == Availability::Available
        })
        .await;
        let status = c.context.diagnostics();
        assert!(
            status.devices[0]
                .last_switch_reason
                .as_ref()
                .unwrap()
                .contains("cloud credentials updated")
        );
        assert_eq!(status.devices[0].selected_control, Some(PathKind::Cloud));
    }));
}

#[test]
fn pending_group_diagnostics_classify_numeric_groups_and_use_the_relation_name() {
    let rig = Rig::group(&["one", "two"], "43", "mijia.light.group3");
    let (release, gate) = flume::unbounded();
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.detail_gates.insert("two".into(), gate);
        cloud.names.insert("43".into(), "Numeric group".into());
    }
    block_on(rig.run(|c| async move {
        c.announce();
        until(|| {
            c.context
                .status
                .borrow()
                .pending
                .iter()
                .any(|pending| pending.did == "43" && pending.reason.contains("two"))
        })
        .await;
        let status = c.context.diagnostics();
        let pending = status
            .pending
            .iter()
            .find(|pending| pending.did == "43")
            .unwrap();
        assert!(pending.group, "numeric group identity stays classified");
        assert_eq!(pending.name, "Numeric group");
        assert!(
            status
                .pending
                .iter()
                .filter(|pending| pending.did != "43")
                .all(|pending| !pending.group)
        );
        assert!(
            c.context.registry.snapshot().applied.groups.is_empty(),
            "an unpublished group never enters the composition registry"
        );
        release.send(()).unwrap();
    }));
}

#[test]
fn pending_group_diagnostics_keep_the_saved_name_over_a_cloud_rename() {
    let rig = Rig::restored_group(
        &["one", "two"],
        "group.1",
        "mijia.light.group3",
        "Saved Home lamp",
    );
    block_on(rig.run(|c| async move {
        c.announce();
        until(|| {
            c.cloud
                .lock()
                .unwrap()
                .detail_counts
                .get("group.1")
                .copied()
                .unwrap_or(0)
                > 0
        })
        .await;
        {
            let mut cloud = c.cloud.lock().unwrap();
            cloud.names.insert("group.1".into(), "Cloud rename".into());
            cloud.detail_failures.insert("group.1".into(), 503);
        }
        c.context.request_sync();
        until(|| {
            c.context
                .status
                .borrow()
                .pending
                .iter()
                .any(|pending| pending.did == "group.1" && pending.reason.contains("503"))
        })
        .await;
        let status = c.context.diagnostics();
        let pending = status
            .pending
            .iter()
            .find(|pending| pending.did == "group.1")
            .unwrap();
        assert!(pending.group);
        assert_eq!(pending.name, "Saved Home lamp");
    }));
}

#[test]
fn pending_restored_numeric_group_keeps_classification_before_relation_metadata() {
    let rig = Rig::restored_group(
        &["one", "two"],
        "43",
        "mijia.light.group3",
        "Saved numeric lamp",
    );
    rig.cloud
        .lock()
        .unwrap()
        .detail_failures
        .insert("43".into(), 503);
    block_on(rig.run(|c| async move {
        c.announce();
        until(|| {
            c.context
                .status
                .borrow()
                .pending
                .iter()
                .any(|pending| pending.did == "43")
        })
        .await;
        assert!(
            c.context.live.borrow().devices["43"].group.is_none(),
            "relation metadata is still absent"
        );
        let status = c.context.diagnostics();
        let pending = status
            .pending
            .iter()
            .find(|pending| pending.did == "43")
            .unwrap();
        assert!(
            pending.group,
            "the restored composition already classifies the identity"
        );
        assert_eq!(pending.name, "Saved numeric lamp");
    }));
}
