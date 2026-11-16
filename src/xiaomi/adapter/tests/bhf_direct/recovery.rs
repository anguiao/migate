use super::*;

fn status_text(c: &Control) -> String {
    let status = crate::terminal::AuthStatus::new(
        crate::xiaomi::auth::AuthReport::for_test(
            crate::xiaomi::auth::AuthenticationState::NotSignedIn,
            None,
            crate::xiaomi::auth::CertificateUpdate::NotNeeded,
            0,
        ),
        "/bin/migate".into(),
        "/data".into(),
    );
    let terminal = crate::terminal::bridge::Context::committed(c.publication.clone(), &status);
    block_on(terminal.handle_line(&format!("status {DID}"), 0))
        .unwrap()
        .unwrap()
}

#[test]
fn direct_business_and_field_failures_do_not_revoke_shared_or_standard_access() {
    let (rig, peer) = setup();
    block_on(rig.run(|c| async move {
        let blow = function(&c, BLOW);
        let ventilation = function(&c, VENTILATION);
        let heater = function(&c, HEATER);
        eventually(|| {
            c.context.direct_candidate(DID).is_ok()
                && blow
                    .snapshot()
                    .fan()
                    .is_some_and(|fan| fan.speed.value == Some(2))
                && ventilation
                    .snapshot()
                    .fan()
                    .is_some_and(|fan| fan.speed.value == Some(1))
        })
        .await;

        peer.values.lock().unwrap()[1] = json!(32);
        peer.values.lock().unwrap()[2] = json!("not-a-temperature");
        work(&c).refresh_shared_feedback();
        eventually(|| {
            let status = c.context.diagnostics();
            let device = status
                .devices
                .iter()
                .find(|device| device.did == DID)
                .unwrap();
            device.shared_feedback.as_ref().is_some_and(|shared| {
                shared
                    .fans
                    .iter()
                    .any(|fan| fan.function == BLOW && fan.level == Some(2) && fan.reason.is_none())
                    && shared.fans.iter().any(|fan| {
                        fan.function == VENTILATION
                            && fan.level.is_none()
                            && fan
                                .reason
                                .as_deref()
                                .is_some_and(|reason| reason.contains('2'))
                    })
                    && shared.temperature.as_ref().is_some_and(|temperature| {
                        temperature.value == json!("not-a-temperature")
                            && temperature.reason.is_some()
                    })
            })
        })
        .await;
        assert_eq!(
            heater
                .snapshot()
                .heater()
                .unwrap()
                .current_temperature
                .value,
            None
        );
        assert_eq!(
            heater.snapshot().heater().unwrap().temperature_source,
            TemperatureSourceState::AwaitingReading
        );

        peer.values.lock().unwrap()[1] = json!(31);
        peer.values.lock().unwrap()[2] = json!(26.5);
        work(&c).refresh_shared_feedback();
        eventually(|| {
            heater
                .snapshot()
                .heater()
                .is_some_and(|heater| heater.current_temperature.value == Some(26.5))
                && blow
                    .snapshot()
                    .fan()
                    .is_some_and(|fan| fan.speed.value == Some(2))
                && ventilation
                    .snapshot()
                    .fan()
                    .is_some_and(|fan| fan.speed.value == Some(1))
        })
        .await;

        peer.rejected
            .lock()
            .unwrap()
            .insert("get_prop".into(), -4004);
        work(&c).refresh_shared_feedback();
        eventually(|| {
            work(&c)
                .refresh
                .borrow()
                .input_problem(
                    crate::xiaomi::capability::SHARED_FEEDBACK_CHANNEL,
                    crate::xiaomi::capability::ReadInput::SharedFeedback,
                )
                .is_some()
        })
        .await;
        assert!(c.context.direct_candidate(DID).is_ok());
        assert_eq!(
            work(&c).property_source(BLOW, BLOW_POWER),
            Some(PathKind::Direct)
        );
        let shared = c
            .context
            .diagnostics()
            .devices
            .into_iter()
            .find(|device| device.did == DID)
            .unwrap()
            .shared_feedback
            .unwrap();
        assert_eq!(shared.selected_read, Some(PathKind::Direct));
        assert_eq!(shared.source, Some(PathKind::Direct));
        assert!(shared.read_problem.is_some());
        assert_eq!(
            blow.snapshot().fan().unwrap().speed.value,
            Some(2),
            "a rejected group read retains the last valid fan level"
        );
        assert_eq!(
            heater
                .snapshot()
                .heater()
                .unwrap()
                .current_temperature
                .value,
            Some(26.5),
            "a rejected group read retains the last valid temperature"
        );

        peer.rejected.lock().unwrap().remove("get_prop");
        peer.rejected_properties
            .lock()
            .unwrap()
            .insert((3, 3), -704);
        assert!(
            heater
                .begin_heater(crate::device::HeaterCommand::Power(true))
                .unwrap()
                .accepted()
                .await
                .is_err()
        );
        assert!(c.context.direct_candidate(DID).is_ok());
        assert_eq!(
            work(&c).property_source(BLOW, BLOW_POWER),
            Some(PathKind::Direct)
        );
        peer.rejected_properties.lock().unwrap().remove(&(3, 3));
        blow.begin_fan(FanCommand::Power(false))
            .unwrap()
            .accepted()
            .await
            .unwrap();
        eventually(|| {
            direct_methods(&peer, "set_properties")
                .iter()
                .any(|request| {
                    request["params"][0]["siid"] == json!(3)
                        && request["params"][0]["piid"] == json!(2)
                        && request["params"][0]["value"] == json!(false)
                })
        })
        .await;
        work(&c).refresh_shared_feedback();
        eventually(|| {
            work(&c)
                .refresh
                .borrow()
                .input_problem(
                    crate::xiaomi::capability::SHARED_FEEDBACK_CHANNEL,
                    crate::xiaomi::capability::ReadInput::SharedFeedback,
                )
                .is_none()
                && heater
                    .snapshot()
                    .heater()
                    .is_some_and(|heater| heater.current_temperature.value == Some(26.5))
        })
        .await;
    }));
}

#[test]
fn direct_outage_switches_shared_and_low_to_cloud_and_recovery_rebaselines_directly() {
    let (rig, direct) = setup();
    rig.network.lan_timeout.set(Duration::from_millis(100));
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud
            .rpc_results
            .insert("get_prop".into(), json!(["dry", 13, 27.0]));
        for (binding, value) in [
            (PropertyBinding { siid: 2, piid: 1 }, json!(false)),
            (PropertyBinding { siid: 2, piid: 2 }, json!(0)),
            (PropertyBinding { siid: 2, piid: 3 }, json!(50)),
            (BLOW_POWER, json!(true)),
            (VENTILATION_POWER, json!(true)),
            (HEATING, json!(false)),
            (PropertyBinding { siid: 3, piid: 8 }, json!(25)),
            (PropertyBinding { siid: 3, piid: 1 }, json!(0)),
        ] {
            cloud
                .raw_values
                .insert((DID.into(), binding.siid, binding.piid), value);
        }
    }
    let (rig, listener, cloud) = with_cloud_rig(rig);
    block_on(rig.run(|c| async move {
        future::or(cloud.run(&listener), async {
            let blow = function(&c, BLOW);
            let heater = function(&c, HEATER);
            eventually(|| {
                c.context.direct_candidate(DID).is_ok()
                    && blow
                        .snapshot()
                        .fan()
                        .is_some_and(|fan| fan.speed.value == Some(2))
                    && heater
                        .snapshot()
                        .heater()
                        .is_some_and(|heater| heater.current_temperature.value == Some(26.5))
                    && work(&c).shared_feedback().source == Some(PathKind::Direct)
            })
            .await;

            // Hold an earlier direct group read across the route change. Its
            // late response must not overwrite the cloud result.
            direct.hold_read.store(true, Ordering::Relaxed);
            let reads = direct_methods(&direct, "get_prop").len();
            work(&c).refresh_shared_feedback();
            eventually(|| direct_methods(&direct, "get_prop").len() > reads).await;
            c.context.stop_lan(DID);
            direct.hold_read.store(false, Ordering::Relaxed);
            work(&c).refresh_shared_feedback();
            eventually(|| {
                work(&c).shared_feedback().source == Some(PathKind::Cloud)
                    && heater
                        .snapshot()
                        .heater()
                        .is_some_and(|heater| heater.current_temperature.value == Some(27.0))
            })
            .await;
            assert_eq!(
                blow.snapshot().fan().unwrap().speed.value,
                Some(1),
                "the cloud group's ventilation digit supplies both fans"
            );

            let direct_low = direct_methods(&direct, "set_bh_mode").len();
            blow.begin_fan(FanCommand::Speed(1))
                .unwrap()
                .accepted()
                .await
                .unwrap();
            eventually(|| c.cloud.lock().unwrap().action_requests.len() == 1).await;
            assert_eq!(
                c.cloud.lock().unwrap().action_requests[0],
                (DID.to_owned(), 4, 4, json!([]))
            );
            assert_eq!(direct_methods(&direct, "set_bh_mode").len(), direct_low);
            let status = c
                .context
                .diagnostics()
                .devices
                .into_iter()
                .find(|device| device.did == DID)
                .unwrap();
            assert_eq!(status.selected_control, Some(PathKind::Cloud));
            assert_eq!(
                status
                    .commands
                    .iter()
                    .find(|command| command.channel == BLOW)
                    .and_then(|command| command.last.as_ref())
                    .and_then(|command| command.actual_path),
                Some(PathKind::Cloud)
            );

            direct.values.lock().unwrap()[0] = json!("quick-heat");
            direct.values.lock().unwrap()[1] = json!(31);
            direct.values.lock().unwrap()[2] = json!(28.5);
            let actions = c.cloud.lock().unwrap().action_requests.len();
            c.context.reconcile_lan(DID).unwrap();
            eventually(|| c.context.direct_candidate(DID).is_ok()).await;
            work(&c).refresh_shared_feedback();
            eventually(|| {
                work(&c).shared_feedback().source == Some(PathKind::Direct)
                    && heater
                        .snapshot()
                        .heater()
                        .is_some_and(|heater| heater.current_temperature.value == Some(28.5))
            })
            .await;
            assert_eq!(
                blow.snapshot().fan().unwrap().speed.value,
                Some(2),
                "direct recovery reads a fresh baseline"
            );
            assert_eq!(
                c.cloud.lock().unwrap().action_requests.len(),
                actions,
                "a settled cloud command is never replayed on direct recovery"
            );
        })
        .await;
    }));
}

#[test]
fn v13_credential_and_address_replacement_moves_standard_and_shared_direct_traffic() {
    use crate::xiaomi::{
        lan::Token,
        mijia::{CloudDeviceInfo, LanCandidate, LanCredentials},
    };

    let rig =
        Rig::custom_lights(&[(DID, MODEL, v13())]).with_stall_timeout(Duration::from_secs(20));
    let scope = rig
        .store
        .devices()
        .load_configuration()
        .unwrap()
        .scope
        .unwrap();
    let old_token = Token::parse("00112233445566778899aabbccddeeff").unwrap();
    let new_token = Token::parse("11223344556677889900aabbccddeeff").unwrap();
    let old = LegacyDevice::bhf_v13(old_token.clone(), DID.parse().unwrap());
    let new = LegacyDevice::bhf_v13(new_token.clone(), DID.parse().unwrap());
    let old_address: std::net::Ipv4Addr = "127.0.0.2".parse().unwrap();
    let new_address: std::net::Ipv4Addr = "127.0.0.3".parse().unwrap();
    rig.store
        .lan()
        .save(&LanRecord {
            scope: scope.clone(),
            did: DID.into(),
            model: MODEL.into(),
            address: old_address,
            token: old_token.clone(),
        })
        .unwrap();
    {
        let mut endpoints = rig.network.lan_endpoints.borrow_mut();
        endpoints.insert(old_address, old.address);
        endpoints.insert(new_address, new.address);
    }
    rig.network.lan_timeout.set(Duration::from_millis(100));
    rig.network
        .discover_at(new.address, Duration::from_millis(20));
    rig.clock.automatic.store(true, Ordering::Relaxed);
    block_on(rig.run(|c| async move {
        let blow = function(&c, BLOW);
        eventually(|| {
            c.context.direct_candidate(DID).is_ok()
                && blow
                    .snapshot()
                    .fan()
                    .is_some_and(|fan| fan.speed.value == Some(2))
        })
        .await;

        let info = CloudDeviceInfo {
            did: DID.into(),
            model: MODEL.into(),
            instance_type: v13()["type"].as_str().unwrap().into(),
            online: true,
            lan: LanCredentials::Candidate(LanCandidate {
                address: new_address,
                token: new_token.clone(),
            }),
        };
        c.context
            .directory
            .borrow_mut()
            .devices
            .get_mut(DID)
            .unwrap()
            .cloud_access = Some(info);
        c.context.changed.notify(usize::MAX);
        eventually(|| {
            c.context
                .directory
                .borrow()
                .devices
                .get(DID)
                .and_then(|slot| slot.lan.active.as_ref())
                .is_some_and(|(record, _)| {
                    record.address == new_address && record.token == new_token
                })
                && blow
                    .snapshot()
                    .fan()
                    .is_some_and(|fan| fan.power.value == Some(true))
        })
        .await;
        assert!(c.context.direct_candidate(DID).is_ok());
        assert!(c.context.legacy_mapping(DID).is_none());

        let old_set = direct_methods(&old, "set_properties").len();
        let new_set = direct_methods(&new, "set_properties").len();
        blow.begin_fan(FanCommand::Power(false))
            .unwrap()
            .accepted()
            .await
            .unwrap();
        eventually(|| direct_methods(&new, "set_properties").len() > new_set).await;
        assert_eq!(direct_methods(&old, "set_properties").len(), old_set);

        let old_shared = direct_methods(&old, "get_prop").len();
        let new_shared = direct_methods(&new, "get_prop").len();
        work(&c).refresh_shared_feedback();
        eventually(|| direct_methods(&new, "get_prop").len() > new_shared).await;
        assert_eq!(direct_methods(&old, "get_prop").len(), old_shared);
        assert_eq!(
            c.context
                .directory
                .borrow()
                .devices
                .get(DID)
                .and_then(|slot| slot.lan.active.as_ref())
                .map(|(record, _)| record.address),
            Some(new_address),
            "the old client is replaced by the current credential/address"
        );
    }));
}

#[test]
fn v13_offline_cold_start_keeps_identity_functions_and_reads_a_new_baseline() {
    let (mut rig, peer) = setup();
    let before = rig.store.devices().load_configuration().unwrap();
    {
        let mut record = rig.store.xiaomi().load().unwrap().unwrap();
        record.ha.authorization.tokens.refresh_at = 1;
        record.ha.authorization.tokens.expires_at = 1;
        rig.store.xiaomi().fixture_replace(&record).unwrap();
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
    rig.network.lan_timeout.set(Duration::from_millis(100));
    rig.source = restored;
    rig.ready = ready;
    rig.clock.automatic.store(true, Ordering::Relaxed);
    block_on(rig.run(|c| async move {
        eventually(|| {
            c.context.direct_candidate(DID).is_ok()
                && function(&c, BLOW)
                    .snapshot()
                    .fan()
                    .is_some_and(|fan| fan.speed.value == Some(2))
                && function(&c, HEATER)
                    .snapshot()
                    .heater()
                    .is_some_and(|heater| heater.current_temperature.value == Some(26.5))
        })
        .await;
        let ids = c
            .context
            .directory
            .borrow()
            .devices
            .get(DID)
            .and_then(|slot| slot.admitted.as_ref())
            .unwrap()
            .native
            .device()
            .functions()
            .iter()
            .map(|function| function.id().to_owned())
            .collect::<Vec<_>>();
        for id in ["light:2", HEATER, BLOW, VENTILATION] {
            assert!(ids.iter().any(|candidate| candidate == id), "{ids:?}");
        }
        assert_eq!(
            c.store.devices().load_configuration().unwrap(),
            before,
            "offline restart preserves the committed physical identity and functions"
        );
        let status = c
            .context
            .diagnostics()
            .devices
            .into_iter()
            .find(|device| device.did == DID)
            .unwrap();
        assert_eq!(status.lan.unwrap().state, LanState::Ready);
        assert_eq!(status.selected_control, Some(PathKind::Direct));
        assert_eq!(status.selected_read, Some(PathKind::Direct));
    }));
}

#[test]
fn diagnostics_show_shared_current_path_apart_from_the_last_observed_source() {
    let (rig, _peer) = setup();
    block_on(rig.run(|c| async move {
        let heater = function(&c, HEATER);
        eventually(|| {
            c.context.direct_candidate(DID).is_ok()
                && heater
                    .snapshot()
                    .heater()
                    .is_some_and(|heater| heater.current_temperature.value == Some(26.5))
        })
        .await;
        let text = status_text(&c);
        assert!(
            text.contains(
                "Legacy feedback group: candidates direct; selected read direct; last observed direct; settled; pending false."
            ),
            "{text}"
        );

        // Remove both eligible group paths without yielding to a background
        // re-authentication: the current availability must still be visible.
        let direct = c.context
            .directory
            .borrow_mut()
            .devices
            .get_mut(DID)
            .unwrap()
            .lan
            .active
            .take();
        c.context.credentials.borrow_mut().pause_http();
        work(&c).reconcile_state_reads(&c.context);
        assert_eq!(
            heater.snapshot().heater().unwrap().temperature_source,
            TemperatureSourceState::Unavailable
        );
        let text = status_text(&c);
        assert!(
            text.contains(
                "Legacy feedback group: candidates none; selected read none; last observed direct; settled; pending false."
            ),
            "{text}"
        );
        assert!(
            text.contains("Current temperature: unknown (built-in source unavailable)"),
            "{text}"
        );
        assert!(text.contains("Last reading (diagnostic only): 26.50 °C"), "{text}");
        assert!(text.contains("temperature: raw 26.5."), "{text}");

        c.context
            .directory
            .borrow_mut()
            .devices
            .get_mut(DID)
            .unwrap()
            .lan
            .active = direct;
        c.context.credentials.borrow_mut().resume_http();
        work(&c).reconcile_state_reads(&c.context);
        let text = status_text(&c);
        assert!(
            text.contains(
                "Legacy feedback group: candidates direct; selected read direct; last observed direct; settled; pending false."
            ),
            "{text}"
        );
        assert!(text.contains("temperature: raw 26.5."), "{text}");
    }));
}

#[test]
fn v13_scope_revocation_stops_direct_without_deleting_saved_identity_or_functions() {
    let (rig, _peer) = setup();
    let before = rig.store.devices().load_configuration().unwrap();
    block_on(rig.run(|c| async move {
        eventually(|| c.context.direct_candidate(DID).is_ok()).await;
        c.context.invalidate_lan_scope().unwrap();
        eventually(|| {
            c.context.direct_candidate(DID).is_err()
                && c.store
                    .lan()
                    .load(
                        &c.store
                            .devices()
                            .load_configuration()
                            .unwrap()
                            .scope
                            .unwrap(),
                        DID,
                    )
                    .unwrap()
                    .is_none()
        })
        .await;
        assert_eq!(c.store.devices().load_configuration().unwrap(), before);
        let ids = c
            .context
            .directory
            .borrow()
            .devices
            .get(DID)
            .and_then(|slot| slot.admitted.as_ref())
            .unwrap()
            .native
            .device()
            .functions()
            .iter()
            .map(|function| function.id().to_owned())
            .collect::<Vec<_>>();
        for id in ["light:2", HEATER, BLOW, VENTILATION] {
            assert!(ids.iter().any(|candidate| candidate == id), "{ids:?}");
        }
        let status = c
            .context
            .diagnostics()
            .devices
            .into_iter()
            .find(|device| device.did == DID)
            .unwrap();
        assert!(status.direct_unavailable_reason.is_some());
        assert_eq!(status.selected_control, None);
    }));
}
