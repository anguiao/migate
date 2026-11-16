use super::*;

#[test]
fn certificate_failure_does_not_delay_cloud_directory_or_control() {
    let rig = Rig::new(true, &["one"]);
    let (_release, certificate) = flume::bounded(1);
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.online.insert("one".into());
        cloud.cert_status = 503;
        cloud.cert_gate = Some(certificate);
    }
    block_on(rig.run(|c| async move {
        until(|| c.cloud.lock().unwrap().certs == 1).await;
        let one = function(&c.context.registry, "one", "outlet:2");
        until(|| one.snapshot().availability == Availability::Available).await;
        one.execute(Command::On).await.unwrap();
        assert_eq!(c.cloud.lock().unwrap().property_writes.len(), 1);
        assert_eq!(c.cloud.lock().unwrap().tokens, 0);
    }));
}

#[test]
fn expired_local_certificate_revokes_only_the_local_candidate() {
    let rig = Rig::new(true, &["one"]);
    rig.cloud.lock().unwrap().online.insert("one".into());
    block_on(rig.run(|c| async move {
        c.announce();
        let control = &c;
        c.peer(Rc::new(Peer::default()), |peer| async move {
            initialized(control, &peer, &["one"]).await;
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
            let by = Duration::from_secs((expiry - control.clock.wall()) as u64);
            control.cloud.lock().unwrap().cert_status = 503;
            control.clock.advance(by);
            until(|| control.context.local_candidate("one", false).is_err()).await;
            assert!(control.context.cloud_candidate("one", true).is_ok());
            function(&control.context.registry, "one", "outlet:2")
                .execute(Command::On)
                .await
                .unwrap();
            assert_eq!(peer.count("rpcReq", "one", 2), 0);
            assert_eq!(control.cloud.lock().unwrap().property_writes.len(), 1);
        })
        .await;
    }));
}

#[test]
fn late_old_certificate_401_keeps_fresh_auth_and_retries_with_current_credentials() {
    let rig = Rig::new(true, &["one"]);
    rig.cloud.lock().unwrap().online.insert("one".into());
    let (release, gate) = flume::bounded(1);
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.cert_status = 401;
        cloud.cert_gate = Some(gate);
    }
    block_on(rig.run(|c| async move {
        until(|| {
            c.cloud.lock().unwrap().certs == 1 && c.context.cloud_candidate("one", true).is_ok()
        })
        .await;
        c.clock
            .automatic
            .store(false, std::sync::atomic::Ordering::Relaxed);
        let key = c.context.ha_key().unwrap();
        c.context
            .token_rejected(
                &key,
                crate::xiaomi::auth::FailureReason::CloudNotification(
                    crate::xiaomi::cloud::NotificationError::Unauthorized,
                ),
            )
            .unwrap();
        until(|| {
            c.context
                .credentials
                .borrow()
                .record()
                .unwrap()
                .ha
                .authorization
                .tokens
                .access_token
                == "fresh-access"
        })
        .await;
        {
            let mut cloud = c.cloud.lock().unwrap();
            cloud.cert_status = 200;
            cloud.cert_gate = None;
        }
        release.send(()).unwrap();
        until(|| c.cloud.lock().unwrap().certs == 2).await;
        until(|| {
            matches!(
                c.context.diagnostics().auth.certificate_update,
                crate::xiaomi::auth::CertificateUpdate::Updated
            )
        })
        .await;
        assert_eq!(c.cloud.lock().unwrap().tokens, 1);
        assert!(!c.context.credentials.borrow().refresh_paused());
        assert!(!matches!(
            c.context.diagnostics().auth.ha,
            crate::xiaomi::auth::AuthenticationState::SignInRequired(_)
        ));
    }));
}

#[test]
fn temporary_certificate_renewal_failures_retry_without_interrupting_local_control() {
    let rig = Rig::new(true, &["one"]);
    rig.cloud.lock().unwrap().cert_status = 503;
    block_on(rig.run(|c| async move {
        c.announce();
        let peer = Rc::new(Peer::default());
        let control = &c;
        c.peer(peer, |peer| async move {
            initialized(control, &peer, &["one"]).await;
            until(|| {
                matches!(
                    control.context.diagnostics().auth.certificate_update,
                    crate::xiaomi::auth::CertificateUpdate::Failed(_)
                )
            })
            .await;
            let before = control.cloud.lock().unwrap().certs;
            function(&control.context.registry, "one", "outlet:2")
                .execute(Command::On)
                .await
                .unwrap();
            control.cloud.lock().unwrap().cert_status = 200;
            until(|| {
                control.cloud.lock().unwrap().certs > before
                    && matches!(
                        control.context.diagnostics().auth.certificate_update,
                        crate::xiaomi::auth::CertificateUpdate::Updated
                    )
            })
            .await;
            assert_eq!(
                function(&control.context.registry, "one", "outlet:2")
                    .snapshot()
                    .availability,
                Availability::Available
            );
        })
        .await;
    }));
}

#[test]
fn background_prepares_missing_hub_identity_before_certificate_request() {
    let rig = Rig::new(true, &["one"]);
    let mut record = rig.store.xiaomi().load().unwrap().unwrap();
    record.hub = None;
    rig.store.xiaomi().fixture_replace(&record).unwrap();
    rig.source.context.adopt_credentials(record);
    rig.cloud.lock().unwrap().cert_status = 503;
    rig.clock
        .automatic
        .store(false, std::sync::atomic::Ordering::Relaxed);
    block_on(rig.run(|c| async move {
        async_io::Timer::after(Duration::from_millis(150)).await;
        assert_eq!(c.cloud.lock().unwrap().certs, 1);
        let hub = c.store.xiaomi().load().unwrap().unwrap().hub.unwrap();
        assert!(hub.certificate_pem.is_none());
        assert!(!hub.virtual_did.is_empty());
        assert!(!hub.private_key_pem.is_empty());
    }));
}

#[test]
fn initial_certificate_failure_reuses_the_saved_identity_on_retry() {
    let rig = Rig::new(true, &["one"]);
    let mut record = rig.store.xiaomi().load().unwrap().unwrap();
    record.hub = None;
    rig.store.xiaomi().fixture_replace(&record).unwrap();
    rig.source.context.adopt_credentials(record);
    rig.clock
        .automatic
        .store(false, std::sync::atomic::Ordering::Relaxed);
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.cert_status = 503;
        cloud.cert_sign_csr = true;
    }
    block_on(rig.run(|c| async move {
        until(|| {
            c.cloud.lock().unwrap().certs == 1
                && matches!(
                    c.context.diagnostics().auth.certificate_update,
                    crate::xiaomi::auth::CertificateUpdate::Failed(
                        crate::xiaomi::auth::FailureReason::Cloud(_)
                    )
                )
        })
        .await;
        let identity = c.store.xiaomi().load().unwrap().unwrap().hub.unwrap();
        assert!(identity.certificate_pem.is_none());
        c.cloud.lock().unwrap().cert_status = 200;
        c.clock.advance(Duration::from_secs(1));
        until(|| {
            c.store
                .xiaomi()
                .load()
                .unwrap()
                .unwrap()
                .hub
                .unwrap()
                .certificate_pem
                .is_some()
        })
        .await;
        let saved = c.store.xiaomi().load().unwrap().unwrap().hub.unwrap();
        assert_eq!(saved.virtual_did, identity.virtual_did);
        assert_eq!(saved.private_key_pem, identity.private_key_pem);
        assert_eq!(c.cloud.lock().unwrap().certs, 2);
    }));
}

#[test]
fn late_certificate_rejection_cannot_stop_identical_replacement_generation() {
    let rig = Rig::new(true, &["one"]);
    rig.clock
        .automatic
        .store(false, std::sync::atomic::Ordering::Relaxed);
    let (release, gate) = flume::bounded(1);
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.cert_status = 401;
        cloud.cert_gate = Some(gate);
    }
    block_on(rig.run(|c| async move {
        until(|| c.cloud.lock().unwrap().certs == 1).await;
        let record = c.store.xiaomi().load().unwrap().unwrap();
        c.store
            .xiaomi()
            .fixture_update_tokens(&record.ha_authorization().unwrap().tokens)
            .unwrap();
        c.context.reload_credentials();
        {
            let mut cloud = c.cloud.lock().unwrap();
            cloud.cert_status = 200;
            cloud.cert_gate = None;
        }
        release.send(()).unwrap();
        until(|| {
            c.cloud.lock().unwrap().certs == 2
                && matches!(
                    c.context.diagnostics().auth.certificate_update,
                    crate::xiaomi::auth::CertificateUpdate::Updated
                )
        })
        .await;
        assert_eq!(c.cloud.lock().unwrap().tokens, 0);
        assert!(c.context.token_usable());
        assert!(!c.store.xiaomi().load().unwrap().unwrap().ha.stopped);
    }));
}

#[test]
fn background_reports_and_repairs_a_damaged_certificate_without_replacing_identity() {
    let rig = Rig::new(true, &["one"]);
    rig.clock
        .automatic
        .store(false, std::sync::atomic::Ordering::Relaxed);
    let (release, gate) = flume::bounded(1);
    let mut record = rig.store.xiaomi().load().unwrap().unwrap();
    let identity = record.hub.as_ref().unwrap().clone();
    record.hub.as_mut().unwrap().certificate_pem = Some("damaged certificate".into());
    rig.store.xiaomi().fixture_replace(&record).unwrap();
    rig.source.context.adopt_credentials(record);
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.cert_sign_csr = true;
        cloud.cert_gate = Some(gate);
    }
    block_on(rig.run(|c| async move {
        until(|| c.cloud.lock().unwrap().certs == 1).await;
        assert_eq!(
            c.context.diagnostics().auth.certificate_update,
            crate::xiaomi::auth::CertificateUpdate::Failed(
                crate::xiaomi::auth::FailureReason::InvalidCertificate
            )
        );
        assert!(c.context.diagnostics().auth.certificate.is_none());
        release.send(()).unwrap();
        until(|| {
            matches!(
                c.context.diagnostics().auth.certificate_update,
                crate::xiaomi::auth::CertificateUpdate::Updated
            )
        })
        .await;
        let repaired = c.store.xiaomi().load().unwrap().unwrap().hub.unwrap();
        assert_eq!(repaired.virtual_did, identity.virtual_did);
        assert_eq!(repaired.private_key_pem, identity.private_key_pem);
        assert!(c.context.diagnostics().auth.certificate.is_some());
    }));
}
