use super::*;
use crate::{
    storage::LanRecord,
    xiaomi::{
        cloud::{CloudDeviceInfo, LanCandidate, LanCredentials},
        lan::{Token, test_support::FakeDevice},
    },
};

#[test]
fn cached_credentials_restore_offline_and_failed_candidate_never_replaces_them() {
    let instance = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/dmaker-p5c.json"
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
fn failed_device_has_one_backoff_recovery_and_no_healthy_polling() {
    use std::sync::atomic::Ordering;
    let instance = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/dmaker-p5c.json"
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
    peer.drop_business.store(true, Ordering::Relaxed);
    let record = LanRecord {
        scope,
        did: "656878122".into(),
        model: "dmaker.fan.p5c".into(),
        address: "127.0.0.1".parse().unwrap(),
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
                        future::or(super::super::lan::run(context.clone()), context.tasks.run())
                            .await
                            .unwrap();
                        panic!("LAN runner stopped");
                    },
                    async {
                        let scheduled = |delay| {
                            context.live.borrow().devices[&record.did]
                                .lan
                                .status
                                .as_ref()
                                .is_some_and(|status| {
                                    status.recovery
                                        == RecoveryStatus::Scheduled(rig.clock.wall() + delay)
                                })
                        };
                        until(|| scheduled(1)).await;
                        let packets = peer.packets.load(Ordering::Relaxed);
                        for _ in 0..5 {
                            context.changed.notify(usize::MAX);
                        }
                        async_io::Timer::after(Duration::from_millis(30)).await;
                        assert_eq!(peer.packets.load(Ordering::Relaxed), packets);
                        rig.clock.advance(Duration::from_secs(1));
                        until(|| scheduled(2)).await;
                        assert!(peer.packets.load(Ordering::Relaxed) > packets);
                        peer.drop_business.store(false, Ordering::Relaxed);
                        rig.clock.advance(Duration::from_secs(2));
                        until(|| {
                            context.live.borrow().devices[&record.did]
                                .lan
                                .status
                                .as_ref()
                                .is_some_and(|status| status.state == LanState::Ready)
                        })
                        .await;
                        let packets = peer.packets.load(Ordering::Relaxed);
                        rig.clock.advance(Duration::from_secs(120));
                        async_io::Timer::after(Duration::from_millis(40)).await;
                        assert_eq!(peer.packets.load(Ordering::Relaxed), packets);
                    },
                )
                .await;
            },
            async {
                async_io::Timer::after(Duration::from_secs(3)).await;
                panic!("LAN recovery stalled");
            },
        )
        .await;
    });
}

#[test]
fn enrollment_requires_successful_read_and_revocation_survives_restart() {
    use std::sync::atomic::Ordering;
    let instance = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/dmaker-p5c.json"
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
                        future::or(super::super::lan::run(context.clone()), context.tasks.run())
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
        "../../../../tests/fixtures/miot/dmaker-p5c.json"
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
                        future::or(super::super::lan::run(context.clone()), context.tasks.run())
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

fn occupied_address_recovers(old: &FakeDevice, expected_old_business: usize, token: Token) {
    use std::sync::atomic::Ordering;
    let instance = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/dmaker-p5c.json"
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
                        future::or(super::super::lan::run(context.clone()), context.tasks.run())
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
        "../../../../tests/fixtures/miot/dmaker-p5c.json"
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
                        future::or(super::super::lan::run(context.clone()), context.tasks.run())
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
