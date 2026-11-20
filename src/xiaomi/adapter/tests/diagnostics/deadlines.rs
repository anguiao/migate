use super::*;
use crate::xiaomi::capability::PropertyBinding;

#[test]
fn hub_offline_at_an_unpolled_certificate_deadline_keeps_the_access_failure_reason() {
    let rig = Rig::new(true, &["one"]);
    rig.cloud.lock().unwrap().online.insert("one".into());
    let (_release, certificate) = flume::bounded(1);
    rig.cloud.lock().unwrap().cert_gate = Some(certificate);
    block_on(rig.run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(Rc::new(Peer::default()), |peer| async move {
            initialized(control, &peer, &["one"]).await;
            until(|| {
                control.context.directory.borrow().scope_valid()
                    && control.context.active_notification("one") == Some(PathKind::Local)
            })
            .await;
            control
                .clock
                .automatic
                .store(false, std::sync::atomic::Ordering::Relaxed);
            let expiry = control
                .context
                .diagnostics()
                .auth
                .certificate
                .unwrap()
                .not_after;
            let handle = control
                .context
                .hub
                .borrow()
                .session()
                .unwrap()
                .handle
                .clone();
            control
                .clock
                .advance(Duration::from_secs((expiry - control.clock.wall()) as u64));
            local_device_offline(&control.context, &handle, "one");
            assert_eq!(
                control.context.diagnostics().devices[0]
                    .last_switch_reason
                    .as_deref(),
                Some("hub reported device one offline or missing")
            );
        })
        .await;
    }));
}

#[test]
fn rejected_ha_at_an_unpolled_token_deadline_keeps_the_authorization_failure_reason() {
    let (rig, listener, cloud) = with_cloud_peer(&["one"]);
    rig.clock
        .automatic
        .store(false, std::sync::atomic::Ordering::Relaxed);
    let mut record = rig.store.xiaomi().load().unwrap().unwrap();
    record.ha.authorization.tokens.refresh_at = rig.clock.wall() + 3;
    record.ha.authorization.tokens.expires_at = rig.clock.wall() + 5;
    rig.store.xiaomi().fixture_replace(&record).unwrap();
    rig.source.context.adopt_credentials(record);
    let (_release, token) = flume::bounded(1);
    rig.cloud.lock().unwrap().token_gate = Some(token);
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            until(|| cloud_active(&c, "one")).await;
            c.clock.advance(Duration::from_secs(3));
            until(|| c.cloud.lock().unwrap().tokens == 1).await;
            let key = c.context.ha_key().unwrap();
            c.clock.advance(Duration::from_secs(2));
            c.context
                .token_rejected(
                    &key,
                    crate::xiaomi::auth::FailureReason::CloudNotification(
                        crate::xiaomi::cloud::NotificationError::Unauthorized,
                    ),
                )
                .unwrap();
            let status = c.context.diagnostics();
            assert_eq!(status.devices[0].selected_control, Some(PathKind::Cloud));
            assert_eq!(
                status.devices[0].last_switch_reason.as_deref(),
                Some("cloud access token rejected")
            );
        })
        .await;
    }));
}

#[test]
fn mixed_group_attributes_both_deadlines_only_when_both_are_needed_to_restore_its_selection() {
    let (rig, listener, cloud) = with_cloud_rig(repair_rig(0, true));
    let expiry = rig
        .source
        .context
        .diagnostics()
        .auth
        .certificate
        .unwrap()
        .not_after;
    let mut record = rig.store.xiaomi().load().unwrap().unwrap();
    record.ha.authorization.tokens.refresh_at = rig.clock.wall() + 1;
    record.ha.authorization.tokens.expires_at = expiry;
    rig.store.xiaomi().fixture_replace(&record).unwrap();
    rig.source.context.adopt_credentials(record);
    let (_token_release, token) = flume::bounded(1);
    let (_certificate_release, certificate) = flume::bounded(1);
    rig.cloud.lock().unwrap().token_gate = Some(token);
    rig.cloud.lock().unwrap().cert_gate = Some(certificate);
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            c.announce();
            let peer = Rc::new(Peer::default());
            peer.state.borrow_mut().bad.insert("member-b".into());
            let control = &c;
            c.peer(peer, |_peer| async move {
                until(|| {
                    control.context.active_notification("member-a") == Some(PathKind::Local)
                        && control.context.active_notification("group-1") == Some(PathKind::Cloud)
                        && control.cloud.lock().unwrap().tokens == 1
                })
                .await;
                control
                    .clock
                    .automatic
                    .store(false, std::sync::atomic::Ordering::Relaxed);
                control
                    .clock
                    .advance(Duration::from_secs((expiry - control.clock.wall()) as u64));
                let status = control.context.diagnostics();
                for (did, reason) in [
                    ("member-a", "hub certificate expired"),
                    ("member-b", "access token expired"),
                    ("group-1", "hub certificate expired; access token expired"),
                ] {
                    assert_eq!(
                        status
                            .devices
                            .iter()
                            .find(|device| device.did == did)
                            .unwrap()
                            .last_switch_reason
                            .as_deref(),
                        Some(reason)
                    );
                }
            })
            .await;
        })
        .await;
    }));
}

#[test]
fn expired_hub_snapshot_is_read_only_and_maintenance_publishes_the_same_change_once() {
    let rig = Rig::new(true, &["one"]);
    rig.cloud.lock().unwrap().online.insert("one".into());
    let (_release, certificate) = flume::bounded(1);
    rig.cloud.lock().unwrap().cert_gate = Some(certificate);
    let published = Rc::new(RefCell::new(Vec::new()));
    let reports = published.clone();
    block_on(rig.run_with_status(
        move |status| reports.borrow_mut().push(status),
        |c| async move {
            c.announce();
            let control = &c;
            c.peer(Rc::new(Peer::default()), |peer| async move {
                initialized(control, &peer, &["one"]).await;
                until(|| {
                    control.context.directory.borrow().scope_valid()
                        && control.context.directory.borrow().devices["one"]
                            .work
                            .as_ref()
                            .unwrap()
                            .observer()
                            .is_some()
                        && control.cloud.lock().unwrap().certs == 1
                })
                .await;
                control
                    .clock
                    .automatic
                    .store(false, std::sync::atomic::Ordering::Relaxed);
                let current = control.context.directory.borrow().devices["one"]
                    .work
                    .clone()
                    .unwrap();
                let before = control.context.diagnostics();
                assert_eq!(before.devices[0].selected_control, Some(PathKind::Local));
                let auth = crate::terminal::AuthStatus::new(
                    before.auth.clone(),
                    "/bin/migate".into(),
                    "/data".into(),
                );
                let terminal =
                    crate::terminal::bridge::Context::committed(control.publication.clone(), &auth);
                let report = terminal.handle_line("status").await.unwrap().unwrap();
                assert!(
                    report.contains("Gateway certificate: valid until"),
                    "{report}"
                );
                let previous_reason = control.context.directory.borrow().devices["one"]
                    .routing_history
                    .as_ref()
                    .and_then(|history| history.reason.clone())
                    .clone();
                let observer = current.observer().unwrap();
                let subscriptions = current.subscription_status("outlet:2");
                let queued = current
                    .refresh
                    .borrow()
                    .contains("outlet:2", PropertyBinding { siid: 2, piid: 1 });
                let expiry = before.auth.certificate.unwrap().not_after;
                control
                    .clock
                    .advance(Duration::from_secs((expiry - control.clock.wall()) as u64));
                let status_changed = control.context.status_changed.listen();
                let changed = control.context.changed.listen();
                let refresh_changed = control.context.refresh_changed.listen();
                let credentials_changed = control.context.credentials_changed.listen();

                let instant = control.context.diagnostics();
                assert_eq!(instant.devices[0].selected_control, Some(PathKind::Cloud));
                assert_eq!(instant.devices[0].selected_read, Some(PathKind::Cloud));
                assert_eq!(instant.devices[0].notification_source, None);
                assert_eq!(
                    instant.devices[0].last_switch_reason.as_deref(),
                    Some("hub certificate expired")
                );
                assert_eq!(control.context.diagnostics(), instant);
                assert_eq!(
                    control.context.directory.borrow().devices["one"]
                        .routing_history
                        .as_ref()
                        .and_then(|history| history.reason.clone()),
                    previous_reason
                );
                assert!(current.observer().unwrap().same(&observer));
                assert_eq!(current.subscription_status("outlet:2"), subscriptions);
                assert_eq!(
                    current
                        .refresh
                        .borrow()
                        .contains("outlet:2", PropertyBinding { siid: 2, piid: 1 }),
                    queued
                );
                assert!(poll_once(status_changed).await.is_none());
                assert!(poll_once(changed).await.is_none());
                assert!(poll_once(refresh_changed).await.is_none());
                assert!(poll_once(credentials_changed).await.is_none());

                let report = terminal.handle_line("status").await.unwrap().unwrap();
                assert!(
                    report.contains("Gateway certificate: expired at"),
                    "{report}"
                );

                until(|| {
                    control.context.directory.borrow().devices["one"]
                        .routing_history
                        .as_ref()
                        .and_then(|history| history.reason.clone())
                        .as_deref()
                        == Some("hub certificate expired")
                })
                .await;
                until(|| {
                    published.borrow().iter().any(|report| {
                        report.certificate_status == instant.certificate_status
                            && report.devices.iter().any(|device| {
                                device.did == "one"
                                    && device.selected_control
                                        == instant.devices[0].selected_control
                                    && device.selected_read == instant.devices[0].selected_read
                                    && device.notification_source
                                        == instant.devices[0].notification_source
                                    && device.last_switch_reason
                                        == instant.devices[0].last_switch_reason
                            })
                    })
                })
                .await;
                assert_eq!(
                    control.context.diagnostics().auth.completed_at(),
                    before.auth.completed_at()
                );
                control.context.status_changed.notify(usize::MAX);
                for _ in 0..10 {
                    future::yield_now().await;
                }
                assert!(published.borrow().windows(2).all(|pair| pair[0] != pair[1]));
            })
            .await;
        },
    ));
}

#[test]
fn simultaneous_expiry_attributes_only_the_paths_that_actually_change() {
    let rig = Rig::new(true, &["one", "two"]);
    let expiry = rig
        .source
        .context
        .diagnostics()
        .auth
        .certificate
        .unwrap()
        .not_after;
    let mut record = rig.store.xiaomi().load().unwrap().unwrap();
    record.ha.authorization.tokens.expires_at = expiry;
    record.ha.authorization.tokens.refresh_at = rig.clock.wall() + 1;
    rig.store.xiaomi().fixture_replace(&record).unwrap();
    rig.source.context.adopt_credentials(record);
    let (_token_release, token) = flume::bounded(1);
    let (_certificate_release, certificate) = flume::bounded(1);
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.online.extend(["one".into(), "two".into()]);
        cloud.token_gate = Some(token);
        cloud.cert_gate = Some(certificate);
    }
    block_on(rig.run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        peer.state.borrow_mut().bad.insert("two".into());
        let control = &c;
        c.peer(peer, |peer| async move {
            initialized(control, &peer, &["one"]).await;
            until(|| {
                control.context.cloud_candidate("two", true).is_ok()
                    && control.context.active_notification("one") == Some(PathKind::Local)
                    && control.cloud.lock().unwrap().tokens == 1
            })
            .await;
            control
                .clock
                .automatic
                .store(false, std::sync::atomic::Ordering::Relaxed);
            let previous = control.context.directory.borrow().devices["two"]
                .routing_history
                .as_ref()
                .and_then(|history| history.reason.clone())
                .clone();
            control
                .clock
                .advance(Duration::from_secs((expiry - control.clock.wall()) as u64));
            let status = control.context.diagnostics();
            let one = status
                .devices
                .iter()
                .find(|device| device.did == "one")
                .unwrap();
            let two = status
                .devices
                .iter()
                .find(|device| device.did == "two")
                .unwrap();
            assert_eq!(
                one.last_switch_reason.as_deref(),
                Some("hub certificate expired")
            );
            assert_eq!(two.last_switch_reason, previous);
            assert_eq!(one.selected_control, Some(PathKind::Cloud));
            assert_eq!(two.selected_control, Some(PathKind::Cloud));
            assert!(control.context.mijia_path().is_some());
        })
        .await;
    }));
}

#[test]
fn mqtt_expiry_records_notification_loss_while_mijia_http_remains_usable() {
    let (rig, listener, cloud) = with_cloud_peer(&["one"]);
    rig.clock
        .automatic
        .store(false, std::sync::atomic::Ordering::Relaxed);
    let mut record = rig.store.xiaomi().load().unwrap().unwrap();
    record.ha.authorization.tokens.refresh_at = rig.clock.wall() + 3;
    record.ha.authorization.tokens.expires_at = rig.clock.wall() + 5;
    rig.store.xiaomi().fixture_replace(&record).unwrap();
    rig.source.context.adopt_credentials(record);
    let (_release, token) = flume::bounded(1);
    rig.cloud.lock().unwrap().token_gate = Some(token);
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            until(|| cloud_active(&c, "one")).await;
            c.clock.advance(Duration::from_secs(3));
            until(|| c.cloud.lock().unwrap().tokens == 1).await;
            let previous = c.context.directory.borrow().devices["one"]
                .routing_history
                .as_ref()
                .and_then(|history| history.reason.clone())
                .clone();
            c.clock.advance(Duration::from_secs(2));
            let changed = c.context.status_changed.listen();
            let instant = c.context.diagnostics();
            assert_eq!(instant.devices[0].selected_control, Some(PathKind::Cloud));
            assert_eq!(instant.devices[0].notification_source, None);
            assert_eq!(
                instant.devices[0].last_switch_reason.as_deref(),
                Some("access token expired")
            );
            assert_eq!(
                c.context.directory.borrow().devices["one"]
                    .routing_history
                    .as_ref()
                    .and_then(|history| history.reason.clone()),
                previous
            );
            assert!(poll_once(changed).await.is_none());
            until(|| {
                c.context.directory.borrow().devices["one"]
                    .routing_history
                    .as_ref()
                    .and_then(|history| history.reason.clone())
                    .as_deref()
                    == Some("access token expired")
            })
            .await;
            function(&c.context, "one", "outlet:2")
                .execute(Command::On)
                .await
                .unwrap();
            assert_eq!(c.cloud.lock().unwrap().property_writes.len(), 1);
        })
        .await;
    }));
}

#[test]
fn virtual_group_reports_the_expired_member_path_in_a_mixed_notification_summary() {
    let (rig, listener, cloud) = with_cloud_rig(repair_rig(0, true));
    // Keep the cloud standby unready while asserting the expiry history;
    // a later member handover legitimately replaces the group's last reason.
    cloud
        .state
        .borrow_mut()
        .hold
        .insert("device/member-a/up/properties_changed/4/2".into());
    let (_release, certificate) = flume::bounded(1);
    rig.cloud.lock().unwrap().cert_gate = Some(certificate);
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            c.announce();
            let peer = Rc::new(Peer::default());
            peer.state.borrow_mut().bad.insert("member-b".into());
            let control = &c;
            c.peer(peer, |_peer| async move {
                until(|| {
                    control.context.active_notification("member-a") == Some(PathKind::Local)
                        && cloud_active(control, "member-b")
                        && control.context.active_notification("group-1") == Some(PathKind::Cloud)
                })
                .await;
                control
                    .clock
                    .automatic
                    .store(false, std::sync::atomic::Ordering::Relaxed);
                let before = control.context.diagnostics();
                let expiry = before.auth.certificate.unwrap().not_after;
                control
                    .clock
                    .advance(Duration::from_secs((expiry - control.clock.wall()) as u64));
                let status = control.context.diagnostics();
                let group = status
                    .devices
                    .iter()
                    .find(|device| device.did == "group-1")
                    .unwrap();
                assert_eq!(group.notification_source, None);
                assert_eq!(group.selected_control, Some(PathKind::Cloud));
                assert_eq!(
                    group.last_switch_reason.as_deref(),
                    Some("hub certificate expired")
                );
                let cloud_member = status
                    .devices
                    .iter()
                    .find(|device| device.did == "member-b")
                    .unwrap();
                assert_eq!(cloud_member.notification_source, Some(PathKind::Cloud));
                assert_eq!(
                    cloud_member.last_switch_reason,
                    before
                        .devices
                        .iter()
                        .find(|device| device.did == "member-b")
                        .unwrap()
                        .last_switch_reason
                );
                until(|| {
                    control.context.directory.borrow().devices["group-1"]
                        .routing_history
                        .as_ref()
                        .and_then(|history| history.reason.clone())
                        .as_deref()
                        == Some("hub certificate expired")
                })
                .await;
            })
            .await;
        })
        .await;
    }));
}

#[test]
fn certificate_expiry_is_visible_and_published_even_without_devices() {
    let rig = Rig::new(true, &[]);
    rig.clock
        .automatic
        .store(false, std::sync::atomic::Ordering::Relaxed);
    let (_release, certificate) = flume::bounded(1);
    let (_directory_release, directory) = flume::bounded(1);
    rig.cloud.lock().unwrap().cert_gate = Some(certificate);
    let published = Rc::new(RefCell::new(Vec::new()));
    let reports = published.clone();
    block_on(rig.run_with_status(
        move |status| reports.borrow_mut().push(status),
        |c| async move {
            until(|| {
                c.cloud.lock().unwrap().certs == 1 && c.context.directory.borrow().scope_valid()
            })
            .await;
            c.clock.advance(Duration::from_secs(30));
            until(|| {
                matches!(
                    c.context.hub.borrow().gateway(),
                    GatewayStatus::Unavailable(_)
                )
            })
            .await;
            c.cloud.lock().unwrap().home_gate = Some(directory);
            let before = c.context.diagnostics();
            let expiry = before.auth.certificate.unwrap().not_after;
            c.clock
                .advance(Duration::from_secs((expiry - c.clock.wall()) as u64));
            let instant = c.context.diagnostics();
            assert_eq!(
                instant.certificate_status,
                Some(crate::xiaomi::certificate::CertificateStatus::Expired)
            );
            assert_eq!(instant.auth.completed_at(), before.auth.completed_at());
            until(|| published.borrow().contains(&instant)).await;
            assert!(instant.devices.is_empty());
        },
    ));
}

#[test]
fn certificate_renewal_phase_is_published_before_the_maintenance_request_completes() {
    let rig = Rig::new(true, &[]);
    rig.clock
        .automatic
        .store(false, std::sync::atomic::Ordering::Relaxed);
    let mut record = rig.store.xiaomi().load().unwrap().unwrap();
    let hub = record.hub.as_ref().unwrap();
    let identity = crate::xiaomi::certificate::ClientIdentity::from_private_key(
        &record.uid,
        &hub.virtual_did,
        &hub.private_key_pem,
    )
    .unwrap();
    let not_after = rig.clock.wall() + 3 * 24 * 60 * 60 + 5;
    record.hub.as_mut().unwrap().certificate_pem = Some(
        crate::xiaomi::tests::support::sign_csr(
            &identity.csr_pem,
            rig.clock.wall() - 60,
            not_after,
        )
        .unwrap(),
    );
    rig.store.xiaomi().fixture_replace(&record).unwrap();
    rig.source.context.adopt_credentials(record);
    let (_release, certificate) = flume::bounded(1);
    rig.cloud.lock().unwrap().cert_gate = Some(certificate);
    let published = Rc::new(RefCell::new(Vec::<AdapterStatus>::new()));
    let reports = published.clone();
    block_on(rig.run_with_status(
        move |status| reports.borrow_mut().push(status),
        |c| async move {
            until(|| {
                published.borrow().last().is_some_and(|status| {
                    status.certificate_status
                        == Some(crate::xiaomi::certificate::CertificateStatus::Valid)
                }) && c.context.directory.borrow().scope_valid()
            })
            .await;
            let before = c.context.diagnostics();
            c.clock.advance(Duration::from_secs(5));
            let instant = c.context.diagnostics();
            assert_eq!(
                instant.certificate_status,
                Some(crate::xiaomi::certificate::CertificateStatus::RenewalDue)
            );
            until(|| c.cloud.lock().unwrap().certs == 1).await;
            assert_eq!(instant.auth.completed_at(), before.auth.completed_at());
            until(|| published.borrow().contains(&instant)).await;
        },
    ));
}

#[test]
fn credential_expiry_without_devices_is_published_without_completing_an_authentication_check() {
    let rig = Rig::new(true, &[]);
    rig.clock
        .automatic
        .store(false, std::sync::atomic::Ordering::Relaxed);
    let mut record = rig.store.xiaomi().load().unwrap().unwrap();
    record.ha.authorization.tokens.expires_at = rig.clock.wall() + 5;
    record.ha.authorization.tokens.refresh_at = rig.clock.wall() + 3;
    rig.store.xiaomi().fixture_replace(&record).unwrap();
    rig.source.context.adopt_credentials(record);
    let (release, gate) = flume::bounded(1);
    let (_certificate_release, certificate) = flume::bounded(1);
    rig.cloud.lock().unwrap().token_gate = Some(gate);
    rig.cloud.lock().unwrap().cert_gate = Some(certificate);
    let published = Rc::new(RefCell::new(Vec::new()));
    let reports = published.clone();
    block_on(rig.run_with_status(
        move |status| reports.borrow_mut().push(status),
        |c| async move {
            until(|| {
                c.cloud.lock().unwrap().certs == 1 && c.context.directory.borrow().scope_valid()
            })
            .await;
            let before = c.context.diagnostics();
            assert!(before.devices.is_empty());
            c.clock.advance(Duration::from_secs(3));
            until(|| c.cloud.lock().unwrap().tokens == 1).await;
            assert_eq!(
                c.context.diagnostics().auth.completed_at(),
                before.auth.completed_at()
            );
            c.clock.advance(Duration::from_secs(2));
            let instant = c.context.diagnostics();
            assert_eq!(instant.auth.completed_at(), before.auth.completed_at());
            until(|| published.borrow().contains(&instant)).await;
            assert_eq!(
                c.context.diagnostics().auth.completed_at(),
                before.auth.completed_at()
            );
            c.cloud.lock().unwrap().token_gate = None;
            release.send(()).unwrap();
            until(|| c.context.token_usable()).await;
            assert_eq!(c.context.diagnostics().auth.completed_at(), c.clock.wall());
        },
    ));
}
