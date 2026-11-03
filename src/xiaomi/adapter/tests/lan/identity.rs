use super::*;

fn retry_new_credentials_after_temporary_failure(
    old_is_active: bool,
    old_usable: bool,
    reject_after_timeout: bool,
) {
    use std::sync::atomic::Ordering;

    let instance = serde_json::from_str(include_str!(
        "../../../../../tests/fixtures/miot/dmaker-p5c.json"
    ))
    .unwrap();
    let rig = Rig::custom_lights(&[("656878122", "dmaker.fan.p5c", instance)]);
    let scope = rig
        .store
        .devices()
        .load_configuration()
        .unwrap()
        .scope
        .unwrap();
    let old = Token::parse("00112233445566778899aabbccddeeff").unwrap();
    let new = Token::parse("11223344556677889900aabbccddeeff").unwrap();
    let invalid = Token::parse("ffeeddccbbaa99887766554433221100").unwrap();
    let peer = FakeDevice::new(if old_usable { old.clone() } else { invalid }, new.clone());
    peer.accept_replacement.store(true, Ordering::Relaxed);
    let record = LanRecord {
        scope: scope.clone(),
        did: "656878122".into(),
        model: "dmaker.fan.p5c".into(),
        address: "127.0.0.1".parse().unwrap(),
        token: old,
    };
    rig.store.lan().save(&record).unwrap();
    let context = rig.source.context.clone();
    context.lan_port.set(peer.address.port());
    context.lan_timeout.set(Duration::from_millis(80));
    context
        .lan_discovery
        .configure_test(peer.address, Duration::from_millis(20));
    let new_info = CloudDeviceInfo {
        did: record.did.clone(),
        model: record.model.clone(),
        instance_type: "fan".into(),
        online: false,
        lan: LanCredentials::Candidate(LanCandidate {
            address: record.address,
            token: new.clone(),
        }),
    };
    if !old_is_active {
        peer.drop_replacement.store(true, Ordering::Relaxed);
        context
            .live
            .borrow_mut()
            .devices
            .get_mut(&record.did)
            .unwrap()
            .cloud_access = Some(new_info.clone());
    }

    block_on(async {
        future::or(
            async {
                future::or(
                    async {
                        future::or(
                            super::super::super::lan::run(context.clone()),
                            context.tasks.run(),
                        )
                        .await
                        .unwrap();
                        panic!("LAN runner stopped");
                    },
                    async {
                        if old_is_active {
                            until(|| context.direct_candidate(&record.did).is_ok()).await;
                            peer.drop_replacement.store(true, Ordering::Relaxed);
                            context
                                .live
                                .borrow_mut()
                                .devices
                                .get_mut(&record.did)
                                .unwrap()
                                .cloud_access = Some(new_info);
                            context.changed.notify(usize::MAX);
                        }
                        until(|| {
                            context.live.borrow().devices[&record.did]
                                .lan
                                .status
                                .as_ref()
                                .is_some_and(|status| status.problem.is_some())
                        })
                        .await;
                        let status = context.live.borrow().devices[&record.did]
                            .lan
                            .status
                            .clone()
                            .unwrap();
                        assert!(matches!(status.recovery, RecoveryStatus::Scheduled(_)));
                        assert_eq!(context.direct_candidate(&record.did).is_ok(), old_usable);
                        assert_eq!(
                            rig.store.lan().load(&scope, &record.did).unwrap(),
                            Some(record.clone())
                        );
                        let deadline = rig.clock.now() + Duration::from_secs(1);
                        until(|| rig.clock.deadlines.lock().unwrap().contains(&deadline)).await;
                        peer.drop_replacement.store(false, Ordering::Relaxed);
                        if reject_after_timeout {
                            peer.accept_replacement.store(false, Ordering::Relaxed);
                        }
                        rig.clock.advance(Duration::from_secs(1));
                        if reject_after_timeout {
                            until(|| {
                                context.live.borrow().devices[&record.did]
                                    .lan
                                    .status
                                    .as_ref()
                                    .is_some_and(|status| {
                                        status.recovery == RecoveryStatus::None
                                            && status.problem.as_ref().is_some_and(|problem| {
                                                problem.contains("identity does not match")
                                            })
                                    })
                            })
                            .await;
                            let packets = peer.packets.load(Ordering::Relaxed);
                            rig.clock.advance(Duration::from_secs(120));
                            context.changed.notify(usize::MAX);
                            async_io::Timer::after(Duration::from_millis(40)).await;
                            assert_eq!(peer.packets.load(Ordering::Relaxed), packets);
                            assert_eq!(
                                rig.store.lan().load(&scope, &record.did).unwrap(),
                                Some(record.clone())
                            );
                        } else {
                            until(|| {
                                rig.store
                                    .lan()
                                    .load(&scope, &record.did)
                                    .unwrap()
                                    .is_some_and(|saved| saved.token == new)
                            })
                            .await;
                        }
                        assert!(context.direct_candidate(&record.did).is_ok());
                    },
                )
                .await;
            },
            async {
                async_io::Timer::after(Duration::from_secs(3)).await;
                panic!("credential recovery stalled");
            },
        )
        .await;
    });
}

#[test]
fn temporary_new_credential_failure_retries_after_unavailable_stored_credentials() {
    retry_new_credentials_after_temporary_failure(false, false, false);
}

#[test]
fn temporary_new_credential_failure_retries_after_stored_credentials_recover() {
    retry_new_credentials_after_temporary_failure(false, true, false);
}

#[test]
fn temporary_new_credential_failure_retries_while_old_connection_remains_usable() {
    retry_new_credentials_after_temporary_failure(true, true, false);
}

#[test]
fn rejected_new_credential_stops_retrying_and_preserves_old_connection() {
    retry_new_credentials_after_temporary_failure(true, true, true);
}

#[test]
fn cached_credentials_restore_offline_and_failed_candidate_never_replaces_them() {
    let instance = serde_json::from_str(include_str!(
        "../../../../../tests/fixtures/miot/dmaker-p5c.json"
    ))
    .unwrap();
    let rig = Rig::custom_lights(&[("656878122", "dmaker.fan.p5c", instance)]);
    let before = rig.store.devices().load_configuration().unwrap();
    let scope = before.scope.clone().unwrap();
    let token = Token::parse("00112233445566778899aabbccddeeff").unwrap();
    let replacement = Token::parse("11223344556677889900aabbccddeeff").unwrap();
    let peer = FakeDevice::new(token.clone(), replacement.clone());
    let record = LanRecord {
        scope: scope.clone(),
        did: "656878122".into(),
        model: "dmaker.fan.p5c".into(),
        address: "127.0.0.1".parse().unwrap(),
        token: token.clone(),
    };
    rig.store.lan().save(&record).unwrap();
    let mut tokens = rig.store.xiaomi().load().unwrap().unwrap().tokens;
    tokens.refresh_at = 1;
    tokens.expires_at = 1;
    rig.store.xiaomi().update_tokens(&tokens).unwrap();
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.token_status = 503;
        cloud.home_status = 503;
        cloud.cert_status = 503;
    }
    rig.clock.advance(Duration::from_secs(7200));
    let (adapter, _) = XiaomiAdapter::build(
        rig.store.clone(),
        rig.source.cloud.clone(),
        rig.discovery.clone(),
        rig.clock.clone(),
    )
    .unwrap();
    let context = adapter.context.clone();
    assert!(
        context
            .status
            .borrow()
            .authentication
            .certificate
            .unwrap()
            .not_after
            < rig.clock.wall()
    );
    context.lan_port.set(peer.address.port());
    context
        .lan_discovery
        .configure_test(peer.address, Duration::from_millis(20));
    block_on(async {
        future::or(
            async {
                future::or(
                    async {
                        adapter.run(|_| {}).await.unwrap();
                        panic!("adapter stopped");
                    },
                    async {
                        until(|| {
                            context.diagnostics().devices.iter().any(|device| {
                                device.did == record.did
                                    && device
                                        .lan
                                        .as_ref()
                                        .is_some_and(|status| status.state == LanState::Ready)
                            })
                        })
                        .await;
                        let status = context
                            .diagnostics()
                            .devices
                            .into_iter()
                            .find(|device| device.did == record.did)
                            .unwrap();
                        assert!(
                            status.selected_control.is_some(),
                            "authenticated LAN must permit offline control"
                        );
                        assert!(
                            status.selected_read.is_some(),
                            "authenticated LAN must permit offline reads"
                        );
                        assert_eq!(
                            rig.store.lan().load(&scope, &record.did).unwrap(),
                            Some(record.clone())
                        );
                        assert_eq!(rig.store.devices().load_configuration().unwrap(), before);
                        let received = peer.packets.load(std::sync::atomic::Ordering::Relaxed);
                        rig.clock.advance(Duration::from_secs(120));
                        async_io::Timer::after(Duration::from_millis(40)).await;
                        assert_eq!(
                            peer.packets.load(std::sync::atomic::Ordering::Relaxed),
                            received
                        );
                        let mut info = CloudDeviceInfo {
                            did: record.did.clone(),
                            model: record.model.clone(),
                            instance_type: "fan".into(),
                            online: false,
                            lan: LanCredentials::Candidate(LanCandidate {
                                address: record.address,
                                token: replacement,
                            }),
                        };
                        context
                            .live
                            .borrow_mut()
                            .devices
                            .get_mut(&record.did)
                            .unwrap()
                            .cloud_access = Some(info.clone());
                        context.changed.notify(usize::MAX);
                        until(|| {
                            context.diagnostics().devices.iter().any(|device| {
                                device
                                    .lan
                                    .as_ref()
                                    .is_some_and(|status| status.problem.is_some())
                            })
                        })
                        .await;
                        let status = context
                            .diagnostics()
                            .devices
                            .into_iter()
                            .find(|device| device.did == record.did)
                            .unwrap();
                        assert_eq!(status.lan.unwrap().state, LanState::Ready);
                        assert_eq!(
                            rig.store.lan().load(&scope, &record.did).unwrap(),
                            Some(record.clone())
                        );
                        info.lan = LanCredentials::NotIp;
                        context
                            .live
                            .borrow_mut()
                            .devices
                            .get_mut(&record.did)
                            .unwrap()
                            .cloud_access = Some(info);
                        context.changed.notify(usize::MAX);
                        until(|| {
                            context.live.borrow().devices[&record.did]
                                .lan
                                .status
                                .as_ref()
                                .is_some_and(|status| status.state == LanState::Ineligible)
                        })
                        .await;
                        assert_eq!(rig.store.lan().load(&scope, &record.did).unwrap(), None);
                        let physical = context
                            .registry
                            .get(&DeviceKey::new("xiaomi", &record.did))
                            .unwrap();
                        assert!(
                            matches!(physical.availability(), Availability::Unavailable(_)),
                            "an ineligible device without another path must become unavailable"
                        );
                    },
                )
                .await;
            },
            async {
                async_io::Timer::after(Duration::from_secs(3)).await;
                panic!("LAN scenario stalled");
            },
        )
        .await;
    });
}

#[test]
fn enrollment_requires_successful_read_and_revocation_survives_restart() {
    use std::sync::atomic::Ordering;
    let instance = serde_json::from_str(include_str!(
        "../../../../../tests/fixtures/miot/dmaker-p5c.json"
    ))
    .unwrap();
    let rig = Rig::custom_lights(&[("656878122", "dmaker.fan.p5c", instance)]);
    let scope = rig
        .store
        .devices()
        .load_configuration()
        .unwrap()
        .scope
        .unwrap();
    let token = Token::parse("00112233445566778899aabbccddeeff").unwrap();
    let replacement = Token::parse("11223344556677889900aabbccddeeff").unwrap();
    let peer = FakeDevice::new(token.clone(), replacement.clone());
    peer.reject_read.store(true, Ordering::Relaxed);
    let context = rig.source.context.clone();
    context.lan_port.set(peer.address.port());
    context.lan_timeout.set(Duration::from_millis(100));
    context
        .lan_discovery
        .configure_test(peer.address, Duration::from_millis(20));
    context
        .live
        .borrow_mut()
        .devices
        .get_mut("656878122")
        .unwrap()
        .cloud_access = Some(CloudDeviceInfo {
        did: "656878122".into(),
        model: "dmaker.fan.p5c".into(),
        instance_type: "fan".into(),
        online: false,
        lan: LanCredentials::Candidate(LanCandidate {
            address: "127.0.0.1".parse().unwrap(),
            token: token.clone(),
        }),
    });
    block_on(async {
        future::or(
            async {
                future::or(
                    async {
                        future::or(
                            super::super::super::lan::run(context.clone()),
                            context.tasks.run(),
                        )
                        .await
                        .unwrap();
                        panic!("LAN runner stopped");
                    },
                    async {
                        until(|| {
                            context.live.borrow().devices["656878122"]
                                .lan
                                .status
                                .as_ref()
                                .is_some_and(|status| status.state == LanState::Failed)
                        })
                        .await;
                        assert_eq!(rig.store.lan().load(&scope, "656878122").unwrap(), None);
                        let packets = peer.packets.load(Ordering::Relaxed);
                        context.changed.notify(usize::MAX);
                        async_io::Timer::after(Duration::from_millis(40)).await;
                        assert_eq!(peer.packets.load(Ordering::Relaxed), packets);
                        peer.reject_read.store(false, Ordering::Relaxed);
                        peer.accept_replacement.store(true, Ordering::Relaxed);
                        context
                            .live
                            .borrow_mut()
                            .devices
                            .get_mut("656878122")
                            .unwrap()
                            .cloud_access
                            .as_mut()
                            .unwrap()
                            .lan = LanCredentials::Candidate(LanCandidate {
                            address: "127.0.0.1".parse().unwrap(),
                            token: replacement.clone(),
                        });
                        context.changed.notify(usize::MAX);
                        until(|| {
                            context.live.borrow().devices["656878122"]
                                .lan
                                .status
                                .as_ref()
                                .is_some_and(|status| status.state == LanState::Ready)
                        })
                        .await;
                        let record = rig.store.lan().load(&scope, "656878122").unwrap().unwrap();
                        assert_eq!(record.token, replacement);
                        context
                            .live
                            .borrow_mut()
                            .devices
                            .get_mut("656878122")
                            .unwrap()
                            .cloud_forbidden = true;
                        context.changed.notify(usize::MAX);
                        async_io::Timer::after(Duration::from_millis(30)).await;
                        assert!(
                            context.live.borrow().devices["656878122"]
                                .lan
                                .status
                                .as_ref()
                                .is_some_and(|status| status.state == LanState::Ready)
                        );
                        context.revoke("656878122", "test revocation").unwrap();
                        assert_eq!(rig.store.lan().load(&scope, "656878122").unwrap(), None);
                        let (restored, _) = XiaomiAdapter::build(
                            rig.store.clone(),
                            rig.source.cloud.clone(),
                            rig.discovery.clone(),
                            rig.clock.clone(),
                        )
                        .unwrap();
                        assert!(
                            restored
                                .context
                                .live
                                .borrow()
                                .devices
                                .get("656878122")
                                .is_none_or(|slot| slot.admitted.is_none())
                        );
                    },
                )
                .await;
            },
            async {
                async_io::Timer::after(Duration::from_secs(3)).await;
                panic!("LAN enrollment stalled");
            },
        )
        .await;
    });
}

#[test]
fn discovered_new_address_is_saved_only_after_authenticated_read() {
    let instance = serde_json::from_str(include_str!(
        "../../../../../tests/fixtures/miot/dmaker-p5c.json"
    ))
    .unwrap();
    let rig = Rig::custom_lights(&[("656878122", "dmaker.fan.p5c", instance)]);
    let scope = rig
        .store
        .devices()
        .load_configuration()
        .unwrap()
        .scope
        .unwrap();
    let token = Token::parse("00112233445566778899aabbccddeeff").unwrap();
    let peer = FakeDevice::new(token.clone(), token.clone());
    let record = LanRecord {
        scope: scope.clone(),
        did: "656878122".into(),
        model: "dmaker.fan.p5c".into(),
        address: "127.0.0.2".parse().unwrap(),
        token,
    };
    rig.store.lan().save(&record).unwrap();
    let (adapter, _) = XiaomiAdapter::build(
        rig.store.clone(),
        rig.source.cloud.clone(),
        rig.discovery.clone(),
        rig.clock.clone(),
    )
    .unwrap();
    let context = adapter.context.clone();
    context.lan_port.set(peer.address.port());
    context.lan_timeout.set(Duration::from_millis(40));
    context
        .lan_discovery
        .configure_test(peer.address, Duration::from_millis(20));
    block_on(async {
        future::or(
            async {
                future::or(
                    async {
                        future::or(
                            super::super::super::lan::run(context.clone()),
                            context.tasks.run(),
                        )
                        .await
                        .unwrap();
                        panic!("LAN runner stopped");
                    },
                    async {
                        until(|| {
                            context.live.borrow().devices[&record.did]
                                .lan
                                .status
                                .as_ref()
                                .is_some_and(|status| status.state == LanState::Ready)
                        })
                        .await;
                        let saved = rig.store.lan().load(&scope, &record.did).unwrap().unwrap();
                        assert_eq!(saved.address, std::net::Ipv4Addr::LOCALHOST);
                        assert_eq!(saved.token, record.token);
                    },
                )
                .await;
            },
            async {
                async_io::Timer::after(Duration::from_secs(3)).await;
                panic!("LAN address recovery stalled");
            },
        )
        .await;
    });
}

fn discovered_address_error_controls_recovery(rejected: bool) {
    use std::sync::atomic::Ordering;

    let instance = serde_json::from_str(include_str!(
        "../../../../../tests/fixtures/miot/dmaker-p5c.json"
    ))
    .unwrap();
    let rig = Rig::custom_lights(&[("656878122", "dmaker.fan.p5c", instance)]);
    let scope = rig
        .store
        .devices()
        .load_configuration()
        .unwrap()
        .scope
        .unwrap();
    let token = Token::parse("00112233445566778899aabbccddeeff").unwrap();
    let other = Token::parse("11223344556677889900aabbccddeeff").unwrap();
    let peer = FakeDevice::new(token.clone(), token.clone());
    peer.reject_read.store(rejected, Ordering::Relaxed);
    peer.drop_business.store(!rejected, Ordering::Relaxed);
    let old = (!rejected).then(|| FakeDevice::new(other, token.clone()));
    let record = LanRecord {
        scope: scope.clone(),
        did: "656878122".into(),
        model: "dmaker.fan.p5c".into(),
        address: "127.0.0.2".parse().unwrap(),
        token,
    };
    rig.store.lan().save(&record).unwrap();
    let context = rig.source.context.clone();
    context.lan_port.set(peer.address.port());
    context.lan_timeout.set(Duration::from_millis(80));
    if let Some(old) = &old {
        context
            .lan_test_endpoints
            .borrow_mut()
            .insert(record.address, old.address);
    }
    context
        .lan_discovery
        .configure_test(peer.address, Duration::from_millis(20));

    block_on(async {
        future::or(
            async {
                future::or(
                    async {
                        future::or(
                            super::super::super::lan::run(context.clone()),
                            context.tasks.run(),
                        )
                        .await
                        .unwrap();
                        panic!("LAN runner stopped");
                    },
                    async {
                        until(|| {
                            context.live.borrow().devices[&record.did]
                                .lan
                                .status
                                .as_ref()
                                .is_some_and(|status| status.state == LanState::Failed)
                        })
                        .await;
                        let status = context.live.borrow().devices[&record.did]
                            .lan
                            .status
                            .clone()
                            .unwrap();
                        assert_eq!(
                            rig.store.lan().load(&scope, &record.did).unwrap(),
                            Some(record.clone())
                        );
                        if rejected {
                            assert_eq!(status.recovery, RecoveryStatus::None);
                            assert!(
                                status
                                    .problem
                                    .unwrap()
                                    .contains("device rejected LAN request")
                            );
                            let packets = peer.packets.load(Ordering::Relaxed);
                            rig.clock.advance(Duration::from_secs(120));
                            context.changed.notify(usize::MAX);
                            async_io::Timer::after(Duration::from_millis(40)).await;
                            assert_eq!(peer.packets.load(Ordering::Relaxed), packets);
                        } else {
                            assert!(matches!(status.recovery, RecoveryStatus::Scheduled(_)));
                            assert!(status.problem.unwrap().contains("timed out"));
                            let deadline = rig.clock.now() + Duration::from_secs(1);
                            until(|| rig.clock.deadlines.lock().unwrap().contains(&deadline)).await;
                            peer.drop_business.store(false, Ordering::Relaxed);
                            rig.clock.advance(Duration::from_secs(1));
                            until(|| context.direct_candidate(&record.did).is_ok()).await;
                            assert_eq!(
                                rig.store
                                    .lan()
                                    .load(&scope, &record.did)
                                    .unwrap()
                                    .unwrap()
                                    .address,
                                std::net::Ipv4Addr::LOCALHOST,
                            );
                        }
                        assert_eq!(status.address, Some(std::net::Ipv4Addr::LOCALHOST));
                    },
                )
                .await;
            },
            async {
                async_io::Timer::after(Duration::from_secs(3)).await;
                panic!("discovered address recovery stalled");
            },
        )
        .await;
    });
}

#[test]
fn discovered_address_rejection_stops_retrying() {
    discovered_address_error_controls_recovery(true);
}

#[test]
fn discovered_address_timeout_retries_after_old_identity_mismatch() {
    discovered_address_error_controls_recovery(false);
}

fn occupied_address_recovers(old: &FakeDevice, expected_old_business: usize, token: Token) {
    use std::sync::atomic::Ordering;
    let instance = serde_json::from_str(include_str!(
        "../../../../../tests/fixtures/miot/dmaker-p5c.json"
    ))
    .unwrap();
    let rig = Rig::custom_lights(&[("656878122", "dmaker.fan.p5c", instance)]);
    let scope = rig
        .store
        .devices()
        .load_configuration()
        .unwrap()
        .scope
        .unwrap();
    let target = FakeDevice::new(token.clone(), token.clone());
    target.hold_read.store(true, Ordering::Relaxed);
    let record = LanRecord {
        scope: scope.clone(),
        did: "656878122".into(),
        model: "dmaker.fan.p5c".into(),
        address: "127.0.0.2".parse().unwrap(),
        token,
    };
    rig.store.lan().save(&record).unwrap();
    let (adapter, _) = XiaomiAdapter::build(
        rig.store.clone(),
        rig.source.cloud.clone(),
        rig.discovery.clone(),
        rig.clock.clone(),
    )
    .unwrap();
    let context = adapter.context.clone();
    context.lan_port.set(target.address.port());
    context.lan_timeout.set(Duration::from_millis(500));
    context
        .lan_test_endpoints
        .borrow_mut()
        .insert(record.address, old.address);
    context
        .lan_discovery
        .configure_test(target.address, Duration::from_millis(20));
    block_on(async {
        future::or(
            async {
                future::or(
                    async {
                        future::or(
                            super::super::super::lan::run(context.clone()),
                            context.tasks.run(),
                        )
                        .await
                        .unwrap();
                        panic!("LAN runner stopped");
                    },
                    async {
                        until(|| target.reads.load(Ordering::Relaxed) > 0).await;
                        assert!(old.packets.load(Ordering::Relaxed) > 0);
                        assert_eq!(old.business.load(Ordering::Relaxed), expected_old_business);
                        assert_eq!(old.reads.load(Ordering::Relaxed), 0);
                        assert_eq!(
                            rig.store.lan().load(&scope, &record.did).unwrap(),
                            Some(record.clone())
                        );
                        target.hold_read.store(false, Ordering::Relaxed);
                        until(|| {
                            context.live.borrow().devices[&record.did]
                                .lan
                                .status
                                .as_ref()
                                .is_some_and(|status| status.state == LanState::Ready)
                        })
                        .await;
                        let saved = rig.store.lan().load(&scope, &record.did).unwrap().unwrap();
                        assert_eq!(saved.address, std::net::Ipv4Addr::LOCALHOST);
                        assert_eq!(saved.token, record.token);
                    },
                )
                .await;
            },
            async {
                async_io::Timer::after(Duration::from_secs(3)).await;
                panic!("occupied-address recovery stalled");
            },
        )
        .await;
    });
}

#[test]
fn wrong_did_at_old_address_receives_no_business_and_discovery_recovers() {
    let token = Token::parse("00112233445566778899aabbccddeeff").unwrap();
    let old = FakeDevice::with_did(token.clone(), token.clone(), 123456789);
    occupied_address_recovers(&old, 0, token);
}

#[test]
fn authenticated_identity_mismatch_at_old_address_still_discovers_target() {
    let token = Token::parse("00112233445566778899aabbccddeeff").unwrap();
    let other = Token::parse("11223344556677889900aabbccddeeff").unwrap();
    let old = FakeDevice::new(other, token.clone());
    occupied_address_recovers(&old, 1, token);
}

#[test]
fn identity_mismatch_without_discovery_stops_without_retries() {
    use std::{net::UdpSocket, sync::atomic::Ordering};
    let instance = serde_json::from_str(include_str!(
        "../../../../../tests/fixtures/miot/dmaker-p5c.json"
    ))
    .unwrap();
    let rig = Rig::custom_lights(&[("656878122", "dmaker.fan.p5c", instance)]);
    let scope = rig
        .store
        .devices()
        .load_configuration()
        .unwrap()
        .scope
        .unwrap();
    let token = Token::parse("00112233445566778899aabbccddeeff").unwrap();
    let other = Token::parse("11223344556677889900aabbccddeeff").unwrap();
    let old = FakeDevice::new(other, token.clone());
    let discovery = UdpSocket::bind("127.0.0.1:0").unwrap();
    discovery
        .set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    let record = LanRecord {
        scope: scope.clone(),
        did: "656878122".into(),
        model: "dmaker.fan.p5c".into(),
        address: "127.0.0.2".parse().unwrap(),
        token,
    };
    rig.store.lan().save(&record).unwrap();
    let (adapter, _) = XiaomiAdapter::build(
        rig.store.clone(),
        rig.source.cloud.clone(),
        rig.discovery.clone(),
        rig.clock.clone(),
    )
    .unwrap();
    let context = adapter.context.clone();
    context
        .lan_test_endpoints
        .borrow_mut()
        .insert(record.address, old.address);
    context.lan_timeout.set(Duration::from_millis(500));
    context
        .lan_discovery
        .configure_test(discovery.local_addr().unwrap(), Duration::from_millis(20));
    block_on(async {
        future::or(
            async {
                future::or(
                    async {
                        future::or(
                            super::super::super::lan::run(context.clone()),
                            context.tasks.run(),
                        )
                        .await
                        .unwrap();
                        panic!("LAN runner stopped");
                    },
                    async {
                        until(|| {
                            context.live.borrow().devices[&record.did]
                                .lan
                                .status
                                .as_ref()
                                .is_some_and(|status| {
                                    status.state == LanState::Failed
                                        && status.recovery == RecoveryStatus::None
                                })
                        })
                        .await;
                        let mut datagram = [0; 64];
                        let (length, _) = discovery
                            .recv_from(&mut datagram)
                            .expect("identity mismatch must trigger discovery");
                        assert_eq!(length, 32);
                        assert_eq!(&datagram[..2], &[0x21, 0x31]);
                        context.changed.notify(usize::MAX);
                        rig.clock.advance(Duration::from_secs(120));
                        async_io::Timer::after(Duration::from_millis(40)).await;
                        assert!(
                            discovery.recv_from(&mut datagram).is_err(),
                            "identity mismatch must not keep scanning"
                        );
                        assert_eq!(old.business.load(Ordering::Relaxed), 1);
                        assert_eq!(old.reads.load(Ordering::Relaxed), 0);
                        assert_eq!(
                            rig.store.lan().load(&scope, &record.did).unwrap(),
                            Some(record.clone())
                        );
                    },
                )
                .await;
            },
            async {
                async_io::Timer::after(Duration::from_secs(3)).await;
                panic!("identity mismatch scenario stalled");
            },
        )
        .await;
    });
}
