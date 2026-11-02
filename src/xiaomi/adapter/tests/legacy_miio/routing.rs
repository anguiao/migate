use super::*;

#[test]
fn uncommitted_profile_does_not_replace_the_running_direct_source() {
    use crate::{configuration::OperationKind, xiaomi::capability::tests::source::SourceFixture};

    let did = "308540429";
    let (rig, peer) = setup(did);
    block_on(rig.run_result_without_publication(|control| async move {
        until(|| {
            control.context.direct_candidate(did).is_ok()
                && function(&control.context.registry, did, "air-conditioner:2")
                    .snapshot()
                    .air_conditioner()
                    .is_some_and(|snapshot| snapshot.mode.value.is_some())
        })
        .await;
        control.clock.automatic.store(false, Ordering::Relaxed);
        let old_work = control.context.live.borrow().devices[did]
            .admitted
            .clone()
            .unwrap();
        let old_physical = old_work.physical();
        let candidate = SourceFixture::new(did, "other.air-conditioner", instance());
        assert!(candidate.legacy_profile().is_none());
        {
            let mut live = control.context.live.borrow_mut();
            let slot = live.devices.get_mut(did).unwrap();
            // There is no newer authoritative cloud model in this scenario.
            slot.cloud_access = None;
            slot.candidate = Some(candidate.into());
        }
        assert!(control.context.legacy_mapping(did).is_some());
        let plan = control.publication.plan().unwrap().unwrap();
        assert_eq!(plan.operations[0].kind, OperationKind::ReplacedNative);
        let db = rusqlite::Connection::open(control.store.path()).unwrap();
        db.execute_batch(
            "CREATE TRIGGER reject_profile_commit BEFORE UPDATE ON bridge_configuration
             BEGIN SELECT RAISE(ABORT, 'injected profile commit failure'); END;",
        )
        .unwrap();
        let prepared = control.publication.prepare(plan);
        assert!(control.publication.apply(prepared).is_err());
        assert!(!old_work.life.is_closed());
        assert!(control.context.legacy_mapping(did).is_some());
        assert!(control.context.direct_candidate(did).is_ok());
        old_physical
            .function("air-conditioner:2")
            .unwrap()
            .begin_air_conditioner(AirConditionerCommand::Temperature(26))
            .unwrap()
            .accepted()
            .await
            .unwrap();
        assert!(peer.requests.lock().unwrap().iter().any(|request| {
            request["method"] == "set_tar_temp" && request["params"] == json!([26])
        }));

        db.execute_batch("DROP TRIGGER reject_profile_commit")
            .unwrap();
        let plan = control.publication.plan().unwrap().unwrap();
        let prepared = control.publication.prepare(plan);
        control.publication.apply(prepared).unwrap();
        assert!(old_work.life.is_closed());
        assert!(control.context.legacy_mapping(did).is_none());
        let current = control.context.live.borrow().devices[did]
            .admitted
            .clone()
            .unwrap();
        assert!(!Rc::ptr_eq(&old_work, &current));
    }))
    .unwrap();
}

#[test]
fn authoritative_cloud_model_mismatch_revokes_old_direct_access() {
    let did = "308540429";
    let (rig, _peer) = setup(did);
    block_on(rig.run(|control| async move {
        until(|| {
            control.context.direct_candidate(did).is_ok()
                && control.context.live.borrow().devices[did]
                    .cloud_access
                    .is_some()
        })
        .await;
        control.clock.automatic.store(false, Ordering::Relaxed);
        control
            .context
            .live
            .borrow_mut()
            .devices
            .get_mut(did)
            .unwrap()
            .cloud_access
            .as_mut()
            .unwrap()
            .model = "other.air-conditioner".into();
        control.context.reconcile_lan(did).unwrap();
        assert!(control.context.legacy_mapping(did).is_some());
        assert!(control.context.direct_candidate(did).is_err());
    }));
}

#[test]
fn mcn02_saved_scope_rebuilds_offline_with_the_same_identity_and_controls() {
    let did = "584519248";
    use crate::device::FunctionKey;
    let sensor: Value = serde_json::from_str(include_str!(
        "../../../../../tests/fixtures/miot/miaomiaoce-t6.json"
    ))
    .unwrap();
    let mut rig = Rig::custom_lights(&[
        (did, "lumi.acpartner.mcn02", instance()),
        ("sensor", "miaomiaoce.sensor_ht.t6", sensor),
    ]);
    let token = Token::parse("00112233445566778899aabbccddeeff").unwrap();
    let peer = LegacyDevice::new(token.clone(), did.parse().unwrap());
    let mut before = rig.store.devices().load_configuration().unwrap();
    before.temperature_bindings.insert(
        FunctionKey::new(DeviceKey::new("xiaomi", did), "air-conditioner:2"),
        FunctionKey::new(DeviceKey::new("xiaomi", "sensor"), "temperature:3:1001"),
    );
    rig.store
        .devices()
        .commit_configuration(&before, &[])
        .unwrap();
    {
        let db = rusqlite::Connection::open(rig.store.path()).unwrap();
        let stored: String = db
            .query_row(
                "SELECT json FROM bridge_configuration WHERE id=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let mut legacy: Value = serde_json::from_str(&stored).unwrap();
        let device = legacy["devices"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|device| device["native"]["model"] == "lumi.acpartner.mcn02")
            .unwrap();
        device["native"]["recognition"]["core"] = json!([]);
        db.execute(
            "UPDATE bridge_configuration SET json=?1 WHERE id=1",
            [legacy.to_string()],
        )
        .unwrap();
    }
    let legacy = rig.store.devices().load_configuration().unwrap();
    assert!(
        legacy
            .device(&DeviceKey::new("xiaomi", did))
            .unwrap()
            .native
            .as_ref()
            .unwrap()
            .device()
            .recognition()
            .unwrap()
            .core()
            .is_empty()
    );
    assert_eq!(
        before
            .device(&DeviceKey::new("xiaomi", did))
            .unwrap()
            .native
            .as_ref()
            .unwrap()
            .device()
            .recognition()
            .unwrap()
            .core(),
        ["air-conditioner:2"]
    );
    rig.store
        .matter()
        .put(rs_matter::persist::FABRIC_KEYS_START + 1, b"fabric")
        .unwrap();
    rig.store
        .matter()
        .put(
            rs_matter::persist::PERSISTENT_SUBSCRIPTIONS_START,
            b"subscription",
        )
        .unwrap();
    rig.store
        .lan()
        .save(&LanRecord {
            scope: before.scope.clone().unwrap(),
            did: did.into(),
            model: "lumi.acpartner.mcn02".into(),
            address: "127.0.0.1".parse().unwrap(),
            token,
        })
        .unwrap();
    rig.clock.automatic.store(true, Ordering::Relaxed);
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
    let (restored, ready) = XiaomiAdapter::build(
        rig.store.clone(),
        rig.source.cloud.clone(),
        rig.discovery.clone(),
        rig.clock.clone(),
    )
    .unwrap();
    restored.context.lan_port.set(peer.address.port());
    rig.source = restored;
    rig.ready = ready;
    block_on(rig.run(|control| {
        Box::pin(async move {
            let ac = function(&control.context.registry, did, "air-conditioner:2");
            until(|| {
                let state = ac.snapshot().air_conditioner().unwrap();
                state.power.value == Some(false)
                    && state.mode.value == Some(AirConditionerMode::Cool)
                    && state.temperature.value == Some(24)
            })
            .await;
            assert!(!matches!(
                ac.snapshot().ambient_temperature(),
                Some(crate::device::AmbientTemperature::Unbound) | None
            ));
            ac.begin_air_conditioner(AirConditionerCommand::Swing(true))
                .unwrap()
                .accepted()
                .await
                .unwrap();
            assert!(
                peer.requests
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|r| r["method"] == "set_ver_swing" && r["params"] == json!(["on"]))
            );
            let after = control.store.devices().load_configuration().unwrap();
            assert_eq!(after, before);
            assert_eq!(
                control
                    .store
                    .matter()
                    .get(rs_matter::persist::FABRIC_KEYS_START + 1)
                    .unwrap(),
                Some(b"fabric".to_vec())
            );
            assert_eq!(
                control
                    .store
                    .matter()
                    .get(rs_matter::persist::PERSISTENT_SUBSCRIPTIONS_START)
                    .unwrap(),
                Some(b"subscription".to_vec())
            );
        })
    }));
}

#[test]
fn mcn02_hub_and_cloud_continue_using_standard_miot_properties() {
    use crate::xiaomi::capability::PropertyBinding;
    let did = "308540429";
    let (rig, direct) = setup(did);
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.online.insert(did.into());
        for (siid, piid, value) in [
            (2, 1, json!(false)),
            (2, 2, json!(1)),
            (2, 3, json!(24.0)),
            (3, 1, json!(0)),
            (3, 2, json!(false)),
        ] {
            cloud.raw_values.insert((did.into(), siid, piid), value);
        }
    }
    block_on(rig.run(|control| {
        Box::pin(async move {
            until(|| {
                control.context.direct_candidate(did).is_ok()
                    && control.context.cloud_candidate(did, true).is_ok()
            })
            .await;
            control.announce();
            let hub = Rc::new(Peer::default());
            for (siid, piid, value) in [
                (2, 1, json!(false)),
                (2, 2, json!(1)),
                (2, 3, json!(24.0)),
                (3, 1, json!(0)),
                (3, 2, json!(false)),
            ] {
                hub.value(did, PropertyBinding { siid, piid }, value);
            }
            let c = &control;
            control
                .peer(hub, |hub| {
                    Box::pin(async move {
                        until(|| {
                            matches!(
                                c.context.control_path_for(did, "air-conditioner:2"),
                                Some(super::super::super::routing::ControlPath::Local(_))
                            )
                        })
                        .await;
                        let ac = function(&c.context.registry, did, "air-conditioner:2");
                        until(|| {
                            ac.snapshot().air_conditioner().unwrap().temperature.value == Some(24)
                        })
                        .await;
                        ac.begin_air_conditioner(AirConditionerCommand::Temperature(26))
                            .unwrap()
                            .accepted()
                            .await
                            .unwrap();
                        let hub_writes = hub
                            .state
                            .borrow()
                            .seen
                            .iter()
                            .filter(|(op, _)| op == "rpcReq")
                            .map(|(_, request)| request["rpc"]["params"][0].clone())
                            .collect::<Vec<_>>();
                        assert_eq!(hub_writes.len(), 1);
                        assert_eq!(
                            (
                                hub_writes[0]["siid"].clone(),
                                hub_writes[0]["piid"].clone(),
                                hub_writes[0]["value"].clone()
                            ),
                            (json!(2), json!(3), json!(26.0))
                        );
                        assert!(
                            !direct
                                .requests
                                .lock()
                                .unwrap()
                                .iter()
                                .any(|r| r["method"].as_str().unwrap().starts_with("set_"))
                        );
                        c.context.stop_lan(did);
                        c.context.priority.set(PathPriority::CloudFirst);
                        assert!(matches!(
                            c.context.control_path_for(did, "air-conditioner:2"),
                            Some(super::super::super::routing::ControlPath::Cloud(_))
                        ));
                        ac.begin_air_conditioner(AirConditionerCommand::Temperature(27))
                            .unwrap()
                            .accepted()
                            .await
                            .unwrap();
                        let cloud_writes = &c.cloud.lock().unwrap().numeric_writes;
                        assert_eq!(cloud_writes.len(), 1);
                        assert_eq!(
                            (
                                cloud_writes[0].0[0]["siid"].clone(),
                                cloud_writes[0].0[0]["piid"].clone(),
                                cloud_writes[0].0[0]["value"].clone()
                            ),
                            (json!(2), json!(3), json!(27.0))
                        );
                        assert!(
                            !direct
                                .requests
                                .lock()
                                .unwrap()
                                .iter()
                                .any(|r| r["method"].as_str().unwrap().starts_with("set_"))
                        );
                    })
                })
                .await;
        })
    }));
}

#[test]
fn mcn02_unknown_swing_does_not_block_offline_direct_and_recovers() {
    use crate::xiaomi::capability::PropertyBinding;

    let did = "308540429";
    let (rig, peer) = setup(did);
    rig.cloud.lock().unwrap().status = 503;
    peer.values.lock().unwrap()[4] = json!("unknown");
    block_on(rig.run(|control| async move {
        let id = "air-conditioner:2";
        let swing = PropertyBinding { siid: 3, piid: 2 };
        let ac = function(&control.context.registry, did, id);
        let work = control.context.live.borrow().devices[did]
            .admitted
            .clone()
            .unwrap();
        until(|| {
            let state = ac.snapshot().air_conditioner().unwrap();
            control.context.direct_candidate(did).is_ok()
                && state.power.value == Some(false)
                && state.mode.value == Some(AirConditionerMode::Cool)
                && state.temperature.value == Some(24)
                && state.speed.value == Some(AirConditionerFanSpeed::Auto)
                && work.refresh.borrow().property_problem(id, swing).is_some()
        })
        .await;
        control.clock.automatic.store(false, Ordering::Relaxed);

        assert_eq!(ac.snapshot().air_conditioner().unwrap().swing.value, None);
        assert_eq!(work.property_source(id, swing), None);
        for binding in [
            PropertyBinding { siid: 2, piid: 1 },
            PropertyBinding { siid: 2, piid: 2 },
            PropertyBinding { siid: 2, piid: 3 },
            PropertyBinding { siid: 3, piid: 1 },
        ] {
            assert_eq!(work.property_source(id, binding), Some(PathKind::Direct));
            assert!(
                work.refresh
                    .borrow()
                    .property_problem(id, binding)
                    .is_none()
            );
        }
        let status = control.context.diagnostics();
        let device = status
            .devices
            .iter()
            .find(|device| device.did == did)
            .unwrap();
        assert_eq!(device.selected_control, Some(PathKind::Direct));
        assert_eq!(device.selected_read, Some(PathKind::Direct));
        assert_eq!(device.notification_source, None);
        let source = crate::xiaomi::adapter::SourceHandle(control.context.clone());
        let configuration = control.store.devices().load_configuration().unwrap();
        assert!(source.initial_reads_ready(&configuration));

        peer.values.lock().unwrap()[4] = json!("on");
        control.clock.advance(Duration::from_secs(4));
        until(|| {
            ac.snapshot().air_conditioner().unwrap().swing.value == Some(true)
                && work.refresh.borrow().property_problem(id, swing).is_none()
        })
        .await;
        assert_eq!(work.property_source(id, swing), Some(PathKind::Direct));
        assert!(control.context.direct_candidate(did).is_ok());
    }));
}

#[test]
fn mcn02_invalid_identity_feedback_retries_after_backoff_and_recovers() {
    for invalid in [
        vec![
            json!("unknown"),
            json!("cool"),
            json!(24),
            json!("auto_fan"),
            json!("off"),
        ],
        vec![json!("off"), json!("cool"), json!(24), json!("auto_fan")],
    ] {
        let did = "308540429";
        let (rig, peer) = setup(did);
        rig.cloud.lock().unwrap().status = 503;
        rig.clock.automatic.store(false, Ordering::Relaxed);
        *peer.values.lock().unwrap() = invalid;
        block_on(rig.run(|control| async move {
            let ac = function(&control.context.registry, did, "air-conditioner:2");
            until(|| {
                control.context.diagnostics().devices.iter().any(|device| {
                    device.did == did
                        && device.lan.as_ref().is_some_and(|lan| {
                            lan.state == LanState::Failed
                                && matches!(lan.recovery, RecoveryStatus::Scheduled(_))
                        })
                })
            })
            .await;
            assert!(control.context.direct_candidate(did).is_err());
            assert_eq!(ac.snapshot().air_conditioner().unwrap().power.value, None);
            let status = control.context.diagnostics();
            let device = status
                .devices
                .iter()
                .find(|device| device.did == did)
                .unwrap();
            let lan = device.lan.as_ref().unwrap();
            assert_eq!(
                lan.recovery,
                RecoveryStatus::Scheduled(control.clock.wall() + 1)
            );
            assert!(lan.problem.is_some());

            let deadline = control.clock.now() + Duration::from_secs(1);
            until(|| control.clock.deadlines.lock().unwrap().contains(&deadline)).await;
            *peer.values.lock().unwrap() = vec![
                json!("off"),
                json!("cool"),
                json!(24),
                json!("auto_fan"),
                json!("off"),
            ];
            let reads = || {
                peer.requests
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|request| request["method"] == "get_prop")
                    .count()
            };
            assert_eq!(reads(), 1);
            control.clock.advance(Duration::from_millis(999));
            async_io::Timer::after(Duration::from_millis(30)).await;
            assert_eq!(reads(), 1);
            assert!(control.context.direct_candidate(did).is_err());

            control.clock.advance(Duration::from_millis(1));
            until(|| control.context.direct_candidate(did).is_ok()).await;
            assert!(reads() >= 2);
            assert_eq!(
                peer.requests
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|request| request["method"] == "miIO.info")
                    .count(),
                2
            );
            control.clock.automatic.store(true, Ordering::Relaxed);
            until(|| ac.snapshot().air_conditioner().unwrap().power.value == Some(false)).await;
            assert!(control.context.direct_candidate(did).is_ok());
            assert!(
                peer.requests
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|request| { !request["method"].as_str().unwrap().starts_with("set_") })
            );
        }));
    }
}

#[test]
fn mcn02_does_not_become_ready_when_read_only_authentication_is_rejected() {
    let did = "308540429";
    let (rig, peer) = setup(did);
    peer.rejected
        .lock()
        .unwrap()
        .insert("get_prop".into(), -4004);
    block_on(rig.run(|control| {
        Box::pin(async move {
            until(|| {
                control.context.diagnostics().devices.iter().any(|device| {
                    device.did == did
                        && device
                            .lan
                            .as_ref()
                            .is_some_and(|lan| lan.state == LanState::Failed)
                })
            })
            .await;
            assert!(control.context.direct_candidate(did).is_err());
            let requests = peer.requests.lock().unwrap();
            assert_eq!(
                requests
                    .iter()
                    .map(|request| request["method"].clone())
                    .collect::<Vec<_>>(),
                [json!("miIO.info"), json!("get_prop")]
            );
            let status = control
                .context
                .diagnostics()
                .devices
                .into_iter()
                .find(|device| device.did == did)
                .unwrap();
            let lan = status.lan.unwrap();
            assert!(lan.problem.unwrap().contains("-4004"));
            assert_eq!(lan.recovery, RecoveryStatus::None);
        })
    }));
}

#[test]
fn cloud_only_function_retries_at_deadline_while_air_conditioner_uses_direct() {
    use super::super::super::routing::ControlPath;
    use crate::xiaomi::{
        capability::PropertyBinding,
        cloud::{CloudDeviceInfo, LanCredentials},
    };

    let did = "308540429";
    let mut description = instance();
    description["services"].as_array_mut().unwrap().push(json!({
        "iid": 4, "type": "urn:miot-spec-v2:service:light:00007802:example:1",
        "properties": [{"iid": 1, "type": "urn:miot-spec-v2:property:on:00000006:example:1", "format": "bool", "access": ["read", "write", "notify"]}]
    }));
    let (rig, _peer) = setup_with_description(did, description);
    let context = rig.source.context.clone();
    let clock = rig.clock.clone();
    clock.automatic.store(false, Ordering::Relaxed);
    context.reconcile_lan(did).unwrap();
    block_on(future::or(
        async {
            context.tasks.run().await.unwrap();
            panic!("task runner stopped")
        },
        async {
            future::or(until(|| context.direct_candidate(did).is_ok()), async {
                async_io::Timer::after(Duration::from_secs(1)).await;
                panic!("direct probe stalled")
            })
            .await;
        },
    ));
    context
        .live
        .borrow_mut()
        .devices
        .get_mut(did)
        .unwrap()
        .cloud_access = Some(CloudDeviceInfo {
        did: did.into(),
        model: "lumi.acpartner.mcn02".into(),
        instance_type: "test".into(),
        online: true,
        lan: LanCredentials::Unknown,
    });
    context.adopt_credentials(rig.store.xiaomi().load().unwrap().unwrap());
    context.cloud_scope_valid.set(true);
    let binding = PropertyBinding { siid: 4, piid: 1 };
    let key = (did.to_owned(), binding.siid, binding.piid);
    rig.cloud
        .lock()
        .unwrap()
        .raw_values
        .insert(key.clone(), json!("invalid"));
    let work = context.live.borrow().devices[did].admitted.clone().unwrap();
    let read_count = || {
        rig.cloud
            .lock()
            .unwrap()
            .property_reads
            .iter()
            .filter(|read| **read == key)
            .count()
    };
    assert!(matches!(
        context.read_path(did),
        Some(ControlPath::Direct(_))
    ));
    assert!(matches!(
        context.read_path_for(did, "air-conditioner:2"),
        Some(ControlPath::Direct(_))
    ));
    assert!(matches!(
        context.read_path_for(did, "light:4"),
        Some(ControlPath::Cloud(_))
    ));
    work.refresh_property("light:4", binding);
    block_on(future::or(
        async {
            future::or(
                super::super::super::refresh::cloud(context.clone()),
                context.tasks.run(),
            )
            .await
            .unwrap();
            panic!("refresh runner stopped");
        },
        async {
            future::or(
                until(|| {
                    let state = work.refresh.borrow().snapshot();
                    state.inflight.is_empty()
                        && state
                            .pending
                            .iter()
                            .any(|read| read.id == "light:4" && read.attempts == 1)
                }),
                async {
                    async_io::Timer::after(Duration::from_secs(2)).await;
                    panic!("initial cloud read did not fail");
                },
            )
            .await;
            let pending = work
                .refresh
                .borrow()
                .snapshot()
                .pending
                .into_iter()
                .find(|read| read.id == "light:4")
                .unwrap();
            assert_eq!(pending.ready_at, Duration::from_secs(3));
            assert_eq!(read_count(), 1);
            // Let the scheduler consume the failed read's completion before advancing time.
            async_io::Timer::after(Duration::from_millis(40)).await;
            clock.advance(Duration::from_millis(2999));
            async_io::Timer::after(Duration::from_millis(20)).await;
            assert_eq!(read_count(), 1);
            rig.cloud
                .lock()
                .unwrap()
                .raw_values
                .insert(key.clone(), json!(true));
            clock.advance(Duration::from_millis(1));
            future::or(until(|| read_count() == 2), async {
                async_io::Timer::after(Duration::from_secs(1)).await;
                panic!("cloud retry was not woken at its deadline")
            })
            .await;
            future::or(
                until(|| {
                    work.physical()
                        .function("light:4")
                        .unwrap()
                        .snapshot()
                        .power()
                        == Some(Power::On)
                }),
                async {
                    async_io::Timer::after(Duration::from_secs(2)).await;
                    panic!("retried cloud reading did not settle");
                },
            )
            .await;
            assert!(
                work.refresh
                    .borrow()
                    .property_problem("light:4", binding)
                    .is_none()
            );
            assert!(matches!(
                context.read_path_for(did, "air-conditioner:2"),
                Some(ControlPath::Direct(_))
            ));
        },
    ));
}
