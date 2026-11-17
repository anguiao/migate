use super::*;

fn retired_queued_lan_task_does_not_probe(recovery: bool, retire_work: bool) {
    use crate::xiaomi::lan::{Client, Failure, RequestError, SendStage};
    use std::sync::atomic::Ordering;

    let did = "656878122";
    let instance = serde_json::from_str(include_str!(
        "../../../../../tests/fixtures/miot/dmaker-p5c.json"
    ))
    .unwrap();
    let rig = Rig::custom_lights(&[(did, "dmaker.fan.p5c", instance)]);
    let token = Token::parse("00112233445566778899aabbccddeeff").unwrap();
    let peer = FakeDevice::new(token.clone(), token.clone());
    let record = LanRecord {
        scope: rig
            .store
            .devices()
            .load_configuration()
            .unwrap()
            .scope
            .unwrap(),
        did: did.into(),
        model: "dmaker.fan.p5c".into(),
        address: "127.0.0.1".parse().unwrap(),
        token: token.clone(),
    };
    rig.store.lan().save(&record).unwrap();
    let context = rig.source.context.clone();
    rig.network.lan_port.set(peer.address.port());
    if recovery {
        let client = Client::new(
            did.parse().unwrap(),
            peer.address,
            token,
            crate::xiaomi::lan::TIMEOUT,
        );
        {
            let mut live = context.directory.borrow_mut();
            let mut lan = live
                .devices
                .get_mut(did)
                .unwrap()
                .work
                .as_ref()
                .unwrap()
                .lan
                .borrow_mut();
            lan.active = Some((record, client.clone()));
            lan.status = Some(LanStatus {
                protocol: super::super::super::model::LanProtocol::Miot,
                state: LanState::Ready,
                address: Some("127.0.0.1".parse().unwrap()),
                problem: None,
                recovery: RecoveryStatus::None,
            });
        }
        context
            .lan_business_failure(
                did,
                &client,
                &RequestError {
                    stage: SendStage::Sent,
                    failure: Failure::Timeout,
                },
            )
            .unwrap();
    } else {
        context.reconcile_lan(did).unwrap();
    }
    assert!(
        context.directory.borrow().devices[did]
            .work
            .as_ref()
            .unwrap()
            .lan
            .borrow()
            .status
            .as_ref()
            .is_some_and(|status| status.state == LanState::Authenticating)
    );
    if retire_work {
        context.directory.borrow().devices[did]
            .work
            .as_ref()
            .unwrap()
            .retire();
    } else {
        context.remove_source_slot(did);
        assert!(!context.directory.borrow().devices.contains_key(did));
    }
    block_on(future::or(
        async {
            context.tasks.run().await.unwrap();
            panic!("task runner stopped");
        },
        async {
            async_io::Timer::after(Duration::from_millis(100)).await;
        },
    ));
    assert_eq!(peer.packets.load(Ordering::Relaxed), 0);
}

#[test]
fn removed_device_cancels_queued_recovery_before_first_poll() {
    retired_queued_lan_task_does_not_probe(true, false);
}

#[test]
fn removed_device_cancels_queued_authentication_before_first_poll() {
    retired_queued_lan_task_does_not_probe(false, false);
}

#[test]
fn cached_scope_restarts_offline_and_executes_real_reads_and_control() {
    cached_scope_restarts_with_ha_state(false);
}

#[test]
fn cached_scope_uses_direct_control_with_stopped_ha_authorization() {
    cached_scope_restarts_with_ha_state(true);
}

fn cached_scope_restarts_with_ha_state(ha_stopped: bool) {
    use crate::device::FanCommand;
    use std::sync::atomic::Ordering;

    let did = "656878122";
    let instance = serde_json::from_str(include_str!(
        "../../../../../tests/fixtures/miot/dmaker-p5c.json"
    ))
    .unwrap();
    let mut rig = Rig::custom_lights(&[(did, "dmaker.fan.p5c", instance)]);
    let before = rig.store.devices().load_configuration().unwrap();
    let token = Token::parse("00112233445566778899aabbccddeeff").unwrap();
    let peer = FakeDevice::new(token.clone(), token.clone());
    rig.store
        .lan()
        .save(&LanRecord {
            scope: before.scope.clone().unwrap(),
            did: did.into(),
            model: "dmaker.fan.p5c".into(),
            address: "127.0.0.1".parse().unwrap(),
            token,
        })
        .unwrap();
    if ha_stopped {
        let mut record = rig.store.xiaomi().load().unwrap().unwrap();
        record.ha.stopped = true;
        rig.store.xiaomi().fixture_replace(&record).unwrap();
    } else {
        let mut tokens = rig
            .store
            .xiaomi()
            .load()
            .unwrap()
            .unwrap()
            .ha
            .authorization
            .tokens;
        tokens.refresh_at = 1;
        tokens.expires_at = 1;
        rig.store.xiaomi().fixture_update_tokens(&tokens).unwrap();
    }
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.token_status = 503;
        cloud.home_status = 503;
        cloud.cert_status = 503;
    }
    rig.clock.advance(Duration::from_secs(7200));
    let (restored, ready) = rig.reopen_adapter();
    rig.network.lan_port.set(peer.address.port());
    rig.source = restored;
    rig.ready = ready;
    rig.clock.automatic.store(true, Ordering::Relaxed);
    block_on(rig.run(|control| {
        Box::pin(async move {
            let fan = function(&control.context, did, "fan:2");
            until(|| {
                let snapshot = fan.snapshot().fan().unwrap();
                snapshot.power.value == Some(true)
                    && snapshot.speed.value == Some(40)
                    && snapshot.swing.value == Some(false)
            })
            .await;
            fan.begin_fan(FanCommand::Power(false))
                .unwrap()
                .accepted()
                .await
                .unwrap();
            assert!(peer.reads.load(Ordering::Relaxed) >= 4);
            assert_eq!(peer.writes.lock().unwrap()[0]["value"], json!(false));
            let status = control
                .context
                .diagnostics()
                .devices
                .into_iter()
                .find(|status| status.did == did)
                .unwrap();
            assert_eq!(status.selected_control, Some(PathKind::Direct));
            assert_eq!(status.selected_read, Some(PathKind::Direct));
            assert_eq!(
                control.store.devices().load_configuration().unwrap(),
                before
            );
            let work = control.context.directory.borrow().devices[did]
                .work
                .clone()
                .unwrap();
            until(|| {
                [(2, 1), (2, 3), (2, 4), (8, 1)].iter().all(|(siid, piid)| {
                    !work.refresh.borrow().contains(
                        "fan:2",
                        crate::xiaomi::capability::PropertyBinding {
                            siid: *siid,
                            piid: *piid,
                        },
                    )
                })
            })
            .await;
            control.clock.automatic.store(false, Ordering::Relaxed);
            let reads = peer.reads.load(Ordering::Relaxed);
            let business = peer.business.load(Ordering::Relaxed);
            peer.values.lock().unwrap().insert((8, 1), json!(72));
            control.clock.advance(Duration::from_secs(120));
            control.clock.automatic.store(true, Ordering::Relaxed);
            until(|| fan.snapshot().fan().unwrap().speed.value == Some(72)).await;
            until(|| {
                [(2, 1), (2, 3), (2, 4), (8, 1)].iter().all(|(siid, piid)| {
                    !work.refresh.borrow().contains(
                        "fan:2",
                        crate::xiaomi::capability::PropertyBinding {
                            siid: *siid,
                            piid: *piid,
                        },
                    )
                })
            })
            .await;
            control.clock.automatic.store(false, Ordering::Relaxed);
            let additional_reads = peer.reads.load(Ordering::Relaxed) - reads;
            assert!(additional_reads > 0);
            assert_eq!(
                peer.business.load(Ordering::Relaxed) - business,
                additional_reads
            );
        })
    }));
}

#[test]
fn sent_timeout_is_not_replayed_and_only_failed_device_recovers_with_new_baseline() {
    use crate::device::{CommandError, FanCommand};
    use std::sync::atomic::Ordering;

    let instance: Value = serde_json::from_str(include_str!(
        "../../../../../tests/fixtures/miot/dmaker-p5c.json"
    ))
    .unwrap();
    let dids = ["656878122", "656878123"];
    let rig = Rig::custom_lights(&[
        (dids[0], "dmaker.fan.p5c", instance.clone()),
        (dids[1], "dmaker.fan.p5c", instance),
    ]);
    let scope = rig
        .store
        .devices()
        .load_configuration()
        .unwrap()
        .scope
        .unwrap();
    let token = Token::parse("00112233445566778899aabbccddeeff").unwrap();
    let first = FakeDevice::new(token.clone(), token.clone());
    let second = FakeDevice::with_did(token.clone(), token.clone(), 656878123);
    for (did, address, endpoint) in [
        (dids[0], "127.0.0.2", first.address),
        (dids[1], "127.0.0.3", second.address),
    ] {
        let address = address.parse().unwrap();
        rig.store
            .lan()
            .save(&LanRecord {
                scope: scope.clone(),
                did: did.into(),
                model: "dmaker.fan.p5c".into(),
                address,
                token: token.clone(),
            })
            .unwrap();
        rig.network
            .lan_endpoints
            .borrow_mut()
            .insert(address, endpoint);
    }
    rig.network.lan_timeout.set(Duration::from_millis(100));
    rig.clock.automatic.store(true, Ordering::Relaxed);
    rig.cloud
        .lock()
        .unwrap()
        .online
        .extend(dids.iter().map(|did| (*did).to_owned()));
    block_on(rig.run(|control| {
        Box::pin(async move {
            until(|| {
                dids.iter().all(|did| {
                    control.context.direct_candidate(did).is_ok()
                        && control.context.cloud_candidate(did, true).is_ok()
                })
            })
            .await;
            until(|| {
                dids.iter().all(|did| {
                    let state = function(&control.context, did, "fan:2")
                        .snapshot()
                        .fan()
                        .unwrap();
                    state.power.value == Some(true)
                        && state.speed.value == Some(40)
                        && state.swing.value == Some(false)
                })
            })
            .await;
            control.clock.automatic.store(false, Ordering::Relaxed);
            first.drop_write.store(true, Ordering::Relaxed);
            let fan1 = function(&control.context, dids[0], "fan:2");
            let fan2 = function(&control.context, dids[1], "fan:2");
            assert_eq!(
                fan1.begin_fan(FanCommand::Power(false))
                    .unwrap()
                    .accepted()
                    .await,
                Err(CommandError::Timeout)
            );
            assert_eq!(first.writes.lock().unwrap().len(), 1);
            assert!(control.cloud.lock().unwrap().numeric_writes.is_empty());
            let statuses = control.context.diagnostics().devices;
            let failed = statuses
                .iter()
                .find(|device| device.did == dids[0])
                .unwrap();
            let healthy = statuses
                .iter()
                .find(|device| device.did == dids[1])
                .unwrap();
            assert_eq!(failed.selected_control, Some(PathKind::Cloud));
            assert_eq!(healthy.selected_control, Some(PathKind::Direct));
            let last = failed.commands[0].last.as_ref().unwrap();
            assert_eq!(last.actual_path, Some(PathKind::Direct));
            assert!(
                last.result_text().contains("uncertain"),
                "{}",
                last.result_text()
            );
            fan2.begin_fan(FanCommand::Swing(true))
                .unwrap()
                .accepted()
                .await
                .unwrap();
            assert_eq!(second.writes.lock().unwrap().len(), 1);
            first.drop_write.store(false, Ordering::Relaxed);
            first.values.lock().unwrap().insert((2, 1), json!(false));
            until(|| {
                control.context.directory.borrow().devices[dids[0]]
                    .work
                    .as_ref()
                    .unwrap()
                    .lan
                    .borrow()
                    .status
                    .as_ref()
                    .is_some_and(|status| matches!(status.recovery, RecoveryStatus::Scheduled(_)))
            })
            .await;
            control.clock.advance(Duration::from_secs(1));
            until(|| control.context.direct_candidate(dids[0]).is_ok()).await;
            control.clock.automatic.store(true, Ordering::Relaxed);
            until(|| fan1.snapshot().fan().unwrap().power.value == Some(false)).await;
            assert_eq!(first.writes.lock().unwrap().len(), 1);
            assert!(control.cloud.lock().unwrap().numeric_writes.is_empty());
        })
    }));
}

#[test]
fn credential_replacement_does_not_discard_an_already_sent_receipt() {
    credential_replacement_result(false);
}

#[test]
fn old_client_timeout_does_not_degrade_replacement_client() {
    credential_replacement_result(true);
}

fn credential_replacement_result(drop_reply: bool) {
    use crate::device::{CommandError, FanCommand};
    use std::sync::atomic::Ordering;

    let did = "656878122";
    let instance = serde_json::from_str(include_str!(
        "../../../../../tests/fixtures/miot/dmaker-p5c.json"
    ))
    .unwrap();
    let rig = Rig::custom_lights(&[(did, "dmaker.fan.p5c", instance)]);
    let scope = rig
        .store
        .devices()
        .load_configuration()
        .unwrap()
        .scope
        .unwrap();
    let old_token = Token::parse("00112233445566778899aabbccddeeff").unwrap();
    let new_token = Token::parse("11223344556677889900aabbccddeeff").unwrap();
    let old = FakeDevice::new(old_token.clone(), old_token.clone());
    let new = FakeDevice::new(new_token.clone(), new_token.clone());
    let old_address = "127.0.0.2".parse().unwrap();
    let new_address = "127.0.0.3".parse().unwrap();
    rig.store
        .lan()
        .save(&LanRecord {
            scope: scope.clone(),
            did: did.into(),
            model: "dmaker.fan.p5c".into(),
            address: old_address,
            token: old_token,
        })
        .unwrap();
    rig.network
        .lan_endpoints
        .borrow_mut()
        .insert(old_address, old.address);
    rig.network
        .lan_endpoints
        .borrow_mut()
        .insert(new_address, new.address);
    rig.network.lan_timeout.set(Duration::from_millis(800));
    rig.clock.automatic.store(true, Ordering::Relaxed);
    block_on(rig.run(|control| {
        Box::pin(async move {
            until(|| control.context.direct_candidate(did).is_ok()).await;
            until(|| {
                function(&control.context, did, "fan:2")
                    .snapshot()
                    .fan()
                    .unwrap()
                    .power
                    .value
                    == Some(true)
            })
            .await;
            old.hold_write.store(true, Ordering::Relaxed);
            let action = function(&control.context, did, "fan:2")
                .begin_fan(FanCommand::Power(false))
                .unwrap();
            until(|| old.writes.lock().unwrap().len() == 1).await;
            control
                .context
                .directory
                .borrow_mut()
                .devices
                .get_mut(did)
                .unwrap()
                .cloud_access = Some(CloudDeviceInfo {
                did: did.into(),
                model: "dmaker.fan.p5c".into(),
                instance_type: "fan".into(),
                online: false,
                lan: LanCredentials::Candidate(LanCandidate {
                    address: new_address,
                    token: new_token.clone(),
                }),
            });
            control.context.changed.notify(usize::MAX);
            until(|| {
                control
                    .store
                    .lan()
                    .load(&scope, did)
                    .unwrap()
                    .is_some_and(|record| {
                        record.address == new_address && record.token == new_token
                    })
            })
            .await;
            old.drop_write.store(drop_reply, Ordering::Relaxed);
            old.hold_write.store(false, Ordering::Relaxed);
            if drop_reply {
                assert_eq!(action.accepted().await, Err(CommandError::Timeout));
            } else {
                action.accepted().await.unwrap();
            }
            assert_eq!(old.writes.lock().unwrap().len(), 1);
            assert!(new.writes.lock().unwrap().is_empty());
            let status = control
                .context
                .diagnostics()
                .devices
                .into_iter()
                .find(|status| status.did == did)
                .unwrap();
            assert_eq!(
                status.commands[0].last.as_ref().unwrap().actual_path,
                Some(PathKind::Direct)
            );
            assert_eq!(status.selected_control, Some(PathKind::Direct));
            assert_eq!(status.lan.unwrap().state, LanState::Ready);
        })
    }));
}

#[test]
fn failed_device_has_one_backoff_recovery_and_no_healthy_probe() {
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
    let (adapter, _) = rig.reopen_adapter();
    let context = adapter.context.clone();
    rig.network.lan_port.set(peer.address.port());
    rig.network.lan_timeout.set(Duration::from_millis(40));
    rig.network
        .discover_at(peer.address, Duration::from_millis(20));
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
                        let scheduled = |delay| {
                            context.directory.borrow().devices[&record.did]
                                .work
                                .as_ref()
                                .unwrap()
                                .lan
                                .borrow()
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
                            context.directory.borrow().devices[&record.did]
                                .work
                                .as_ref()
                                .unwrap()
                                .lan
                                .borrow()
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
fn retiring_native_work_cancels_its_queued_authentication() {
    retired_queued_lan_task_does_not_probe(false, true);
}

fn cached_native_lan() -> (Rig, FakeDevice) {
    let did = "656878122";
    let instance = serde_json::from_str(include_str!(
        "../../../../../tests/fixtures/miot/dmaker-p5c.json"
    ))
    .unwrap();
    let rig = Rig::custom_lights(&[(did, "dmaker.fan.p5c", instance)]);
    let token = Token::parse("00112233445566778899aabbccddeeff").unwrap();
    let peer = FakeDevice::new(token.clone(), token.clone());
    rig.store
        .lan()
        .save(&LanRecord {
            scope: rig
                .store
                .devices()
                .load_configuration()
                .unwrap()
                .scope
                .unwrap(),
            did: did.into(),
            model: "dmaker.fan.p5c".into(),
            address: "127.0.0.1".parse().unwrap(),
            token,
        })
        .unwrap();
    rig.network.lan_port.set(peer.address.port());
    (rig, peer)
}

#[test]
fn native_work_keeps_its_authenticated_client_only_through_a_logical_replacement() {
    let did = "656878122";
    let (rig, _peer) = cached_native_lan();
    let context = rig.source.context.clone();
    let work = context.directory.borrow().devices[did]
        .work
        .clone()
        .unwrap();
    context.reconcile_lan(did).unwrap();
    block_on(future::or(
        async {
            context.tasks.run().await.unwrap();
            panic!("task runner stopped");
        },
        async {
            until(|| context.direct_candidate(did).is_ok()).await;
            let client = context.direct_candidate(did).unwrap();
            let (physical, _) =
                crate::device::PhysicalDevice::new(work.native.physical_description("Replacement"));
            let previous = work.replace_sink(Rc::new(super::super::super::work::LogicalSink::new(
                physical,
            )));
            previous.close();
            assert!(context.direct_candidate(did).unwrap().same(&client));
            work.retire();
            assert!(
                work.lan.borrow().active.is_none(),
                "a retained retired runtime must release its authenticated client"
            );
        },
    ));
}

#[test]
fn failed_lan_credential_removal_still_retires_native_authority() {
    let did = "656878122";
    let (rig, _peer) = cached_native_lan();
    let context = rig.source.context.clone();
    let work = context.directory.borrow().devices[did]
        .work
        .clone()
        .unwrap();
    context.reconcile_lan(did).unwrap();
    block_on(future::or(
        async {
            context.tasks.run().await.unwrap();
            panic!("task runner stopped");
        },
        async {
            until(|| context.direct_candidate(did).is_ok()).await;
            let database = rusqlite::Connection::open(rig.store.path()).unwrap();
            database
                .execute_batch(
                    "CREATE TRIGGER reject_lan_removal BEFORE DELETE ON xiaomi_lan
                 BEGIN SELECT RAISE(ABORT, 'injected'); END;",
                )
                .unwrap();
            assert!(context.revoke(did, "confirmed removal").is_err());
            assert!(
                work.life.is_closed(),
                "storage failure must not retain source authority"
            );
            assert!(work.current_sink().life.is_closed());
            assert!(work.lan.borrow().active.is_none());
            assert!(context.control_path(did).is_none());
            assert!(
                context.directory.borrow().devices[did]
                    .routing_history
                    .is_none()
            );
            context.update_device_routing(did, "late source event");
            assert!(
                context.directory.borrow().devices[did]
                    .routing_history
                    .is_none()
            );
        },
    ));
}
