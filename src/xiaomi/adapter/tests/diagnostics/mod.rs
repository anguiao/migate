use super::*;
use crate::xiaomi::adapter::{PathKind, PathPriority};

mod deadlines;
mod history;
mod publishing;

#[test]
fn credential_report_checks_expiry_without_a_new_authentication_check() {
    use crate::xiaomi::auth::{AuthenticationState, FailureReason};

    let rig = Rig::new(true, &["one"]);
    let context = &rig.source.context;
    let checked = context.diagnostics().auth;
    let expires_at = context
        .credentials
        .borrow()
        .record()
        .unwrap()
        .ha_authorization()
        .unwrap()
        .tokens
        .expires_at;
    rig.clock
        .advance(Duration::from_secs((expires_at - rig.clock.wall()) as u64));

    for _ in 0..2 {
        let report = context.diagnostics().auth;
        assert_eq!(
            report.ha,
            AuthenticationState::Unavailable(FailureReason::AccessTokenExpired)
        );
        assert_eq!(report.completed_at(), checked.completed_at());
        assert_eq!(report.certificate, checked.certificate);
        assert!(context.mijia_path().is_some());
    }
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
                control.context.directory.borrow().scope_valid()
                    && control.context.directory.borrow().devices["one"]
                        .admitted
                        .as_ref()
                        .unwrap()
                        .observer()
                        .is_some()
            })
            .await;
            control
                .clock
                .automatic
                .store(false, std::sync::atomic::Ordering::Relaxed);
            let status = control.context.diagnostics();
            assert!(status.routing.local_connected);
            assert_eq!(status.routing.priority, PathPriority::LocalFirst);
            let one = &status.devices[0];
            assert_eq!(one.did, "one");
            assert_eq!(one.usable_control_paths, [PathKind::Local, PathKind::Cloud]);
            assert_eq!(one.selected_control, Some(PathKind::Local));
            assert_eq!(one.notification_source, Some(PathKind::Local));
            assert!(one.sync_limited_reason.is_none());
            control.context.credentials.borrow_mut().pause_http();
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
fn virtual_group_notification_diagnostics_follow_member_subscription_readiness() {
    let (rig, listener, cloud) = with_cloud_rig(repair_rig(0, true));
    let held = "device/member-b/up/properties_changed/4/1";
    cloud.state.borrow_mut().hold.insert(held.into());
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            repair_ready(&c).await;
            until(|| {
                cloud_active(&c, "member-a")
                    && cloud
                        .state
                        .borrow()
                        .held
                        .iter()
                        .any(|subscription| subscription.filters[0].path == held)
            })
            .await;
            let check = |expected| {
                let status = c.context.diagnostics();
                let group = status
                    .devices
                    .iter()
                    .find(|device| device.did == "group-1")
                    .unwrap();
                let light = group
                    .functions
                    .iter()
                    .find(|function| function.function == "light:2")
                    .unwrap();
                assert_eq!(group.notification_source, expected);
                assert_eq!(light.notification_source, expected);
                assert_eq!(group.sync_limited_reason.is_some(), expected.is_none());
                assert_eq!(light.sync_limited_reason.is_some(), expected.is_none());
            };
            check(None);

            cloud.state.borrow_mut().hold.remove(held);
            cloud.release(held);
            until(|| c.context.active_notification("group-1") == Some(PathKind::Cloud)).await;
            check(Some(PathKind::Cloud));
            cloud.property("member-a", 2, false);
            cloud.property("member-b", 2, false);
            until(|| {
                function(&c.context.registry, "group-1", "light:2")
                    .snapshot()
                    .power()
                    == Some(Power::Off)
            })
            .await;

            cloud.online("member-b", false);
            until(|| c.context.active_notification("group-1").is_none()).await;
            check(None);
            cloud.online("member-b", true);
            until(|| c.context.active_notification("group-1") == Some(PathKind::Cloud)).await;
            check(Some(PathKind::Cloud));
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
            function(&c.context.registry, "one", "outlet:2")
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
        let mut foreign = c.context.credentials.borrow().record().unwrap().clone();
        foreign.uid = "foreign".into();
        c.context.adopt_credentials(foreign);
        let status = c.context.diagnostics();
        assert_eq!(status.devices[0].selected_control, None);
        assert!(
            status.devices[0]
                .unavailable_reason
                .as_ref()
                .unwrap()
                .contains("account/home identity mismatch")
        );
    }));
}

#[test]
fn command_diagnostics_keep_inflight_actual_path_when_control_selection_changes() {
    let rig = Rig::new(true, &["one"]);
    rig.network.gateway_timeout.set(Duration::from_secs(2));
    rig.cloud.lock().unwrap().online.insert("one".into());
    block_on(rig.run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(Rc::new(Peer::default()), |peer| async move {
            initialized(control, &peer, &["one"]).await;
            until(|| control.context.directory.borrow().scope_valid()).await;
            peer.state.borrow_mut().hold_writes = true;
            let one = function(&control.context.registry, "one", "outlet:2");
            let mut command = Box::pin(one.execute(Command::On));
            assert!(poll_once(command.as_mut()).await.is_none());
            until(|| peer.count("rpcReq", "one", 2) == 1).await;
            let handle = control
                .context
                .hub
                .borrow()
                .session()
                .unwrap()
                .handle
                .clone();
            local_device_offline(&control.context, &handle, "one");
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
        let one = function(&c.context.registry, "one", "outlet:2");
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
        let one = function(&c.context.registry, "one", "outlet:2");
        until(|| one.snapshot().availability == Availability::Available).await;
        assert!(one.execute(Command::On).await.is_err());
        let status = c.context.diagnostics();
        assert!(
            status.devices[0]
                .last_switch_reason
                .as_ref()
                .unwrap()
                .contains("Mijia HTTP request failed")
        );
        assert!(
            status.devices[0]
                .unavailable_reason
                .as_ref()
                .unwrap()
                .contains("Mijia HTTP unavailable")
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
    record.ha.authorization.tokens.refresh_at = rig.clock.wall() + 5;
    record.ha.authorization.tokens.expires_at = rig.clock.wall() + 10;
    rig.store.xiaomi().fixture_replace(&record).unwrap();
    rig.source.context.adopt_credentials(record);
    let (release, gate) = flume::bounded(1);
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.online.insert("one".into());
        cloud.token_gate = Some(gate);
    }
    block_on(rig.run(|c| async move {
        let one = function(&c.context.registry, "one", "outlet:2");
        until(|| one.snapshot().availability == Availability::Available).await;
        c.clock.advance(Duration::from_secs(5));
        until(|| c.cloud.lock().unwrap().tokens == 1).await;
        c.clock.advance(Duration::from_secs(5));
        until(|| !c.context.token_usable()).await;
        until(|| {
            matches!(
                c.context.diagnostics().auth.ha,
                crate::xiaomi::auth::AuthenticationState::Unavailable(
                    crate::xiaomi::auth::FailureReason::AccessTokenExpired
                )
            )
        })
        .await;
        let status = c.context.diagnostics();
        assert_eq!(status.devices[0].selected_control, Some(PathKind::Cloud));
        assert!(matches!(
            status.auth.ha,
            crate::xiaomi::auth::AuthenticationState::Unavailable(
                crate::xiaomi::auth::FailureReason::AccessTokenExpired
            )
        ));
        c.cloud.lock().unwrap().token_gate = None;
        release.send(()).unwrap();
        until(|| {
            c.context.token_usable() && one.snapshot().availability == Availability::Available
        })
        .await;
        let status = c.context.diagnostics();

        assert_eq!(status.devices[0].selected_control, Some(PathKind::Cloud));
    }));
}
