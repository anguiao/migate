use super::*;
use crate::device::{
    Capability, DeviceKey, LightDimension, LightMode, LightTarget, Power, Rgb, TemperatureInterval,
};

async fn wait_for(predicate: impl FnMut() -> bool, what: &str) {
    let stall = async_io::Timer::after(Duration::from_secs(2));
    futures_lite::future::or(
        async move {
            until(predicate).await;
        },
        async {
            stall.await;
            panic!("{what}");
        },
    )
    .await;
}

fn member_writes(c: &Control, did: &str, siid: u32, piid: u32) -> Vec<Value> {
    c.cloud
        .lock()
        .unwrap()
        .numeric_writes
        .iter()
        .flat_map(|(items, _)| items.iter())
        .filter(|item| item["did"] == did && item["siid"] == siid && item["piid"] == piid)
        .map(|item| item["value"].clone())
        .collect()
}

fn group_writes(c: &Control, did: &str, siid: u32, piid: u32) -> Vec<Value> {
    member_writes(c, did, siid, piid)
}

#[test]
fn repaired_group_publishes_extended_color_and_dispatches_rgb_to_members() {
    let rig = repair_rig(0, true);
    block_on(rig.run(|c| async move {
        repair_ready(&c).await;
        let light = function(&c.context.registry, "group-1", "main");
        let capabilities = light.description().capabilities().clone();
        assert!(
            capabilities.contains(&Capability::ColorRgb),
            "repaired group must publish RGB: {capabilities:?}"
        );
        assert_eq!(
            light.description().temperature_interval(),
            Some(TemperatureInterval::new(2700, 6500).unwrap())
        );
        let before = light.snapshot();
        c.cloud.lock().unwrap().property_reads.clear();
        // The same cached RGB must still be sent to switch from white mode.
        let action = light
            .begin_light_action(
                LightTarget::rgb(
                    None,
                    Rgb {
                        red: 0x11,
                        green: 0x22,
                        blue: 0x33,
                    },
                ),
                true,
            )
            .unwrap();
        action.accepted().await.unwrap();
        wait_for(
            || {
                ["member-a", "member-b"].iter().all(|did| {
                    !member_writes(&c, did, MEMBER_COLOR.siid, MEMBER_COLOR.piid).is_empty()
                })
            },
            "member RGB writes were not dispatched",
        )
        .await;
        for did in ["member-a", "member-b"] {
            assert_eq!(
                member_writes(&c, did, MEMBER_COLOR.siid, MEMBER_COLOR.piid)
                    .last()
                    .cloned(),
                Some(json!(0x112233))
            );
            assert!(member_writes(&c, did, 4, 2).is_empty());
            assert!(member_writes(&c, did, 2, 1).is_empty());
        }
        wait_for(
            || {
                ["member-a", "member-b"].iter().all(|did| {
                    c.cloud
                        .lock()
                        .unwrap()
                        .property_reads
                        .contains(&(did.to_string(), 4, 2))
                })
            },
            "RGB acceptance must refresh the real mode property",
        )
        .await;
        assert_eq!(light.snapshot(), before, "acceptance is not feedback");
        assert!(
            group_writes(&c, "group-1", MEMBER_COLOR.siid, MEMBER_COLOR.piid).is_empty(),
            "the group DID has no ambient binding"
        );
    }));
}

#[test]
fn repaired_group_native_controls_do_not_depend_on_member_writes() {
    let rig = repair_rig(1, true);
    block_on(rig.run(|c| async move {
        repair_ready(&c).await;
        let light = function(&c.context.registry, "group-1", "main");
        for did in ["member-a", "member-b"] {
            force_unavailable(&c.context, did, "member unavailable");
        }
        let before = light.snapshot();
        light.execute(Command::On).await.unwrap();
        light.execute(Command::Off).await.unwrap();
        for target in [
            brightness(80.0),
            // The native value already equals this target, but writing it
            // still selects white mode on the members.
            LightTarget::temperature(None, 2700.0).expect("valid test target"),
        ] {
            light
                .begin_light_action(target, true)
                .unwrap()
                .accepted()
                .await
                .unwrap();
        }
        assert_eq!(
            group_writes(&c, "group-1", 2, 1),
            [json!(true), json!(false)]
        );
        assert_eq!(group_writes(&c, "group-1", 2, 2), [json!(80)]);
        assert_eq!(group_writes(&c, "group-1", 2, 3), [json!(2700)]);
        assert!(
            c.cloud
                .lock()
                .unwrap()
                .numeric_writes
                .iter()
                .flat_map(|(items, _)| items)
                .all(|item| item["did"] == "group-1")
        );
        assert_eq!(
            light.snapshot(),
            before,
            "native acceptance is not feedback"
        );
    }));
}

#[test]
fn repaired_group_state_follows_confirmed_member_and_clears_on_loss() {
    let rig = repair_rig(0, true);
    block_on(rig.run(|c| async move {
        repair_ready(&c).await;
        let light = function(&c.context.registry, "group-1", "main");
        let work = c.context.live.borrow().devices["member-a"]
            .admitted
            .clone()
            .unwrap();
        work.notify_value(MEMBER_COLOR, &json!(0x00ff00), false, false);
        work.notify_value(MEMBER_MODE, &json!(1), false, false);
        // The production notification path refreshes repaired group state
        // after an actual member observation.
        c.context.refresh_group_states("member-a");
        wait_for(
            || {
                let snapshot = light.snapshot();
                let lighting = snapshot.lighting.unwrap();
                lighting.mode == Some(LightMode::Rgb)
                    && lighting.color
                        == Some(Rgb {
                            red: 0,
                            green: 255,
                            blue: 0,
                        })
            },
            "group state did not follow the confirmed member",
        )
        .await;
        // Losing the representative selects the other readable member and
        // carries that member's own confirmed mode and cached RGB together.
        force_unavailable(&c.context, "member-a", "test member outage");
        wait_for(
            || {
                let lighting = light.snapshot().lighting.unwrap();
                lighting.mode == Some(LightMode::Temperature)
                    && lighting.color
                        == Some(Rgb {
                            red: 0x11,
                            green: 0x22,
                            blue: 0x33,
                        })
            },
            "representative fallback did not carry its confirmed mode/cache",
        )
        .await;
        // Losing every readable member clears the repaired mode too.
        force_unavailable(&c.context, "member-b", "test member outage");
        wait_for(
            || light.snapshot().lighting.unwrap().mode.is_none(),
            "group repaired mode was not cleared on all-member loss",
        )
        .await;
    }));
}

fn gated_rig(member_mode: u64, member_power: bool) -> (Rig, flume::Sender<()>) {
    let rig = repair_rig(member_mode, member_power);
    let (release, gate) = flume::bounded::<()>(1);
    rig.cloud
        .lock()
        .unwrap()
        .write_gates
        .insert("member-b".into(), gate);
    (rig, release)
}

/// Hold member-b's shared DID write gate through a real ordinary command until
/// the cloud response gate is released.
async fn wait_holding_member_b(c: &Control) {
    wait_for(
        || !member_writes(c, "member-b", 2, 1).is_empty(),
        "ordinary member command did not take its DID gate",
    )
    .await;
}

fn brightness(value: f64) -> LightTarget {
    LightTarget::brightness(None, value).expect("valid test target")
}

fn rgb(value: u32) -> LightTarget {
    LightTarget::rgb(None, Rgb::from_packed(value).unwrap())
}

/// Let the outer executor honour the released DID gate and poll the queued
/// member task. This is scheduling, not a timeout.
async fn settle() {
    for _ in 0..64 {
        future::yield_now().await;
    }
}

#[test]
fn queued_rgb_rechecks_action_and_member_power_before_sending() {
    #[derive(Debug)]
    enum Change {
        Stop,
        NewTarget,
        Rebuild,
        MemberOff,
        NativeTemperature,
        NativeOff,
    }
    for change in [
        Change::Stop,
        Change::NewTarget,
        Change::Rebuild,
        Change::MemberOff,
        Change::NativeTemperature,
        Change::NativeOff,
    ] {
        let (rig, release) = gated_rig(0, true);
        block_on(rig.run(|c| async move {
            repair_ready(&c).await;
            let member = function(&c.context.registry, "member-b", "main");
            let holding = async {
                member.execute(Command::Off).await.unwrap();
            };
            let scenario = async {
                wait_holding_member_b(&c).await;
                let light = function(&c.context.registry, "group-1", "main");
                let action = light.begin_light_action(rgb(0x00ff00), true).unwrap();
                action.accepted().await.unwrap();
                match change {
                    Change::Stop => {
                        light.stop_light(LightDimension::Color).unwrap();
                    }
                    Change::NewTarget => {
                        light
                            .begin_light_action(rgb(0xff0000), true)
                            .unwrap()
                            .accepted()
                            .await
                            .unwrap();
                    }
                    Change::NativeTemperature => {
                        light
                            .begin_light_action(
                                LightTarget::temperature(None, 4000.0).expect("valid test target"),
                                true,
                            )
                            .unwrap()
                            .accepted()
                            .await
                            .unwrap();
                    }
                    Change::NativeOff => {
                        light.execute(Command::Off).await.unwrap();
                    }
                    Change::Rebuild => c.publication.stop_actions(),
                    Change::MemberOff => {
                        let work = c.context.live.borrow().devices["member-b"]
                            .admitted
                            .clone()
                            .unwrap();
                        work.notify_value(
                            crate::xiaomi::capability::PropertyBinding { siid: 2, piid: 1 },
                            &json!(false),
                            false,
                            false,
                        );
                    }
                }
                c.cloud.lock().unwrap().write_gates.remove("member-b");
                release.send(()).unwrap();
            };
            future::zip(holding, scenario).await;
            if matches!(change, Change::NewTarget) {
                wait_for(
                    || member_writes(&c, "member-b", 4, 1).contains(&json!(0xff0000)),
                    "replacement frame did not reach the released member",
                )
                .await;
            } else {
                settle().await;
            }
            assert!(
                !member_writes(&c, "member-b", 4, 1).contains(&json!(0x00ff00)),
                "queued RGB must be discarded after {change:?}"
            );
        }));
    }
}

#[test]
fn submitted_rgb_receipt_survives_stop_before_or_after_early_success() {
    for early_success in [false, true] {
        let rig = repair_rig(0, true);
        let held = if early_success {
            "member-b"
        } else {
            "member-a"
        };
        let (release, gate) = flume::bounded(1);
        {
            let mut cloud = rig.cloud.lock().unwrap();
            cloud.write_gates.insert(held.into(), gate);
            if !early_success {
                cloud
                    .raw_values
                    .insert(("member-b".into(), 2, 1), json!(false));
            }
        }
        block_on(rig.run(|c| async move {
            repair_ready(&c).await;
            let light = function(&c.context.registry, "group-1", "main");
            let action = light.begin_light_action(rgb(0x0000ff), true).unwrap();
            if early_success {
                action.accepted().await.unwrap();
            }
            wait_for(
                || member_writes(&c, held, 4, 1).contains(&json!(0x0000ff)),
                "held member write was not submitted",
            )
            .await;
            // Stop cannot retract a submitted RGB write or change its real receipt.
            light.stop_light(LightDimension::Color).unwrap();
            release.send(()).unwrap();
            action.accepted().await.unwrap();
            if !early_success {
                assert_eq!(
                    action.ended().await,
                    Err(crate::device::CommandError::Stopped)
                );
                assert!(
                    !light.light_action_current(action.token()),
                    "the late receipt must not revive the action"
                );
            }
            wait_for(
                || {
                    let status = c
                        .context
                        .commands
                        .snapshot(held, ["main".to_owned()].into_iter());
                    status[0].inflight.is_none()
                        && status[0]
                            .last
                            .as_ref()
                            .is_some_and(|last| last.result == Some(Ok(())))
                },
                "submitted member receipt did not finish after Stop",
            )
            .await;
        }));
    }
}

#[test]
fn repaired_unaligned_native_grid_quantizes_and_out_of_interval_is_unknown() {
    let mut group = group_instance();
    group["services"][1]["properties"][2]["value-range"] = json!([1700, 7000, 100]);
    let mut member = ls2307_instance();
    member["services"][1]["properties"][2]["value-range"] = json!([2700, 6450, 1]);
    let rig = Rig::custom_lights(&[
        ("member-a", "devcea.light.ls2307", member.clone()),
        ("member-b", "devcea.light.ls2307", member),
    ]);
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.catalog_groups = vec![group_record(
            "group-record",
            "group-1",
            &["member-a", "member-b"],
            true,
        )];
        cloud
            .catalog_instances
            .insert(group["type"].as_str().unwrap().into(), group);
        cloud
            .online
            .extend(["group-1", "member-a", "member-b"].map(str::to_string));
        for did in ["group-1", "member-a", "member-b"] {
            for (siid, piid, value) in
                [(2, 1, json!(false)), (2, 2, json!(50)), (2, 3, json!(2700))]
            {
                cloud.raw_values.insert((did.into(), siid, piid), value);
            }
        }
        for did in ["member-a", "member-b"] {
            cloud.raw_values.insert(
                (did.into(), MEMBER_COLOR.siid, MEMBER_COLOR.piid),
                json!(0x112233),
            );
            cloud
                .raw_values
                .insert((did.into(), MEMBER_MODE.siid, MEMBER_MODE.piid), json!(1));
        }
    }
    block_on(rig.run(|c| async move {
        repair_ready(&c).await;
        let light = function(&c.context.registry, "group-1", "main");
        assert_eq!(
            light.description().temperature_interval(),
            Some(TemperatureInterval::new(2700, 6450).unwrap())
        );
        let action = light
            .begin_light_action(
                LightTarget::temperature(None, 6450.0).expect("valid test target"),
                true,
            )
            .unwrap();
        action.accepted().await.unwrap();
        wait_for(
            || group_writes(&c, "group-1", 2, 3).last().cloned() == Some(json!(6500)),
            "logical target must quantize on the genuine native grid",
        )
        .await;
        // Actual feedback on the native grid outside the logical interval
        // becomes unknown instead of being rounded to the target.
        c.cloud
            .lock()
            .unwrap()
            .raw_values
            .insert(("group-1".into(), 2, 3), json!(6500));
        let work = c.context.live.borrow().devices["group-1"]
            .admitted
            .clone()
            .unwrap();
        work.request_dirty_read(
            "main",
            crate::xiaomi::capability::PropertyBinding { siid: 2, piid: 3 },
        );
        wait_for(
            || {
                light
                    .snapshot()
                    .lighting
                    .unwrap()
                    .temperature_kelvin
                    .is_none()
            },
            "out-of-interval feedback must clear to unknown",
        )
        .await;
    }));
}

#[test]
fn repaired_group_downgrade_after_commit_and_failed_save_keeps_old_plan() {
    let rig = repair_rig(0, true);
    block_on(rig.run(|c| async move {
        repair_ready(&c).await;
        let light = function(&c.context.registry, "group-1", "main");
        let committed = c
            .store
            .devices()
            .load_configuration()
            .unwrap()
            .device(&DeviceKey::new("xiaomi", "group-1"))
            .cloned()
            .unwrap();
        let identity = committed.unique_id.clone();
        let endpoint = committed.endpoint;
        let version = committed.configuration_version;
        let _scope = c.context.live.borrow().scope.clone().unwrap();
        let root = c.store.devices().load_configuration().unwrap().root_version;

        // A new genuine member input drops RGB. The existing group keeps
        // serving until the whole candidate configuration commits.
        let downgraded = confirmed_for(
            &c,
            "member-a",
            "yeelink.light.light3",
            &serde_json::from_str(include_str!(
                "../../../../tests/fixtures/miot/yeelink-light-light3.json"
            ))
            .unwrap(),
        );
        c.context
            .live
            .borrow_mut()
            .devices
            .get_mut("member-a")
            .unwrap()
            .latest = Some(downgraded.clone());

        // A failed save keeps the committed repair plan.
        let db = rusqlite::Connection::open(c.store.path()).unwrap();
        db.execute_batch(
            "CREATE TRIGGER fail_config BEFORE UPDATE ON bridge_configuration
             BEGIN SELECT RAISE(ABORT, 'injected'); END;",
        )
        .unwrap();
        c.context.stage();
        until(|| c.publication.failure().is_some()).await;
        assert!(
            light.description().supports_rgb(),
            "a failed commit must retain the old repair plan"
        );
        assert!(c.context.group_repair("group-1").is_some());
        db.execute_batch("DROP TRIGGER fail_config").unwrap();

        // The successful complete commit persists the genuine new Native input
        // and installs the native fallback profile.
        c.context.stage();
        let group_key = DeviceKey::new("xiaomi", "group-1");
        wait_for(
            || {
                c.context
                    .registry
                    .get(&group_key)
                    .and_then(|physical| physical.function("main"))
                    .is_none_or(|function| !function.description().supports_rgb())
            },
            "successful commit must fall back to the native profile",
        )
        .await;
        assert!(c.context.group_repair("group-1").is_none());
        let saved = c.store.devices().load_configuration().unwrap();
        let native_a = saved
            .devices
            .iter()
            .filter_map(|device| device.native.as_ref())
            .find(|native| native.device.key().id == "member-a")
            .expect("member native input restored from the committed configuration");
        let stored_a = saved
            .devices
            .iter()
            .find(|record| record.key.id == "member-a")
            .expect("member logical identity restored");
        assert_eq!(native_a.model, "yeelink.light.light3");
        assert!(
            native_a.device.functions()[0]
                .lighting()
                .unwrap()
                .color()
                .is_none(),
            "the persisted confirmed input is the genuine new Native description"
        );
        assert_eq!(stored_a.functions[0].id, "main");
        assert!(saved.root_version > root);

        // A restart rebuild sees the persisted Native input: the committed
        // native profile and range survive without a widening or version bump.
        let restarted = crate::app::App::open(
            crate::xiaomi::adapter::SourceHandle(c.context.clone()),
            c.store.clone(),
        )
        .unwrap();
        assert!(
            restarted.plan().unwrap().is_none_or(|plan| !plan.rebuild),
            "restart must not widen or rebuild from stale cached inputs"
        );
        assert!(c.context.group_repair("group-1").is_none());

        // A fresh supported member description restores repair and
        // keeps identity/endpoint while bumping the fixed version.
        let ls = serde_json::from_str(include_str!(
            "../../../../tests/fixtures/miot/devcea-light-ls2307.json"
        ))
        .unwrap();
        let recovered = confirmed_for(&c, "member-a", "devcea.light.ls2307", &ls);
        {
            let mut live = c.context.live.borrow_mut();
            let slot = live.devices.get_mut("member-a").unwrap();
            slot.latest = Some(recovered.clone());
            slot.latest = Some(recovered);
        }
        c.context.reconcile("member-a").unwrap();
        wait_for(
            || c.context.group_repair("group-1").is_some(),
            "re-confirmed members must restore repair",
        )
        .await;
        let light = function(&c.context.registry, "group-1", "main");
        assert!(light.description().supports_rgb());
        let again = c
            .store
            .devices()
            .load_configuration()
            .unwrap()
            .device(&DeviceKey::new("xiaomi", "group-1"))
            .cloned()
            .unwrap();
        assert_eq!(again.unique_id, identity);
        assert_eq!(again.endpoint, endpoint);
        assert!(again.configuration_version > version);
    }));
}

#[test]
fn repaired_group_unauthorized_member_path_recovers_after_token_refresh() {
    let rig = repair_rig(0, true);
    block_on(rig.run(|c| async move {
        repair_ready(&c).await;
        let light = function(&c.context.registry, "group-1", "main");
        c.cloud.lock().unwrap().property_status = 401;
        let denied = light.begin_light_action(rgb(0x00ff00), true).unwrap();
        assert!(denied.accepted().await.is_err());
        // The existing cloud coordination refreshes the rejected token.
        until(|| c.cloud.lock().unwrap().tokens > 0).await;
        c.cloud.lock().unwrap().property_status = 200;
        until(|| c.context.token_usable()).await;
        until(|| {
            ["member-a", "member-b"]
                .iter()
                .all(|did| function(&c.context.registry, did, "main").snapshot().power == Power::On)
        })
        .await;
        let next = light.begin_light_action(rgb(0x0000ff), true).unwrap();
        next.accepted().await.unwrap();
        wait_for(
            || {
                member_writes(&c, "member-a", MEMBER_COLOR.siid, MEMBER_COLOR.piid)
                    .last()
                    .cloned()
                    == Some(json!(0x0000ff))
            },
            "member writes did not recover after token refresh",
        )
        .await;
    }));
}

#[test]
fn repaired_group_native_partial_failure_fails_without_optimistic_state() {
    let rig = repair_rig(0, true);
    rig.cloud
        .lock()
        .unwrap()
        .raw_values
        .insert(("group-1".into(), 2, 3), json!(2700));
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud
            .property_codes
            .insert(("group-1".into(), 2, 3), -704_030_013);
    }
    block_on(rig.run(|c| async move {
        repair_ready(&c).await;
        let light = function(&c.context.registry, "group-1", "main");
        wait_for(
            || light.snapshot().lighting.unwrap().temperature_kelvin == Some(2700),
            "native CT did not settle",
        )
        .await;
        let before = light.snapshot();
        let action = light
            .begin_light_action(
                LightTarget::temperature(None, 4200.0).expect("valid test target"),
                true,
            )
            .unwrap();
        assert!(action.accepted().await.is_err());
        assert_eq!(
            light.snapshot(),
            before,
            "no optimistic native/member state"
        );
        assert!(
            member_writes(&c, "member-a", 4, 1).is_empty(),
            "native failure must not dispatch member RGB"
        );
    }));
}

#[test]
fn repaired_rgb_eligibility_and_acceptance_use_actual_member_power() {
    for (powers, failures, accepted) in [
        ([Some(true), Some(false)], [false, false], true),
        ([None, Some(true)], [false, false], true),
        ([Some(false), None], [false, false], false),
        ([Some(true), Some(true)], [true, true], false),
        ([Some(true), Some(true)], [true, false], true),
        ([Some(true), Some(false)], [true, false], false),
    ] {
        let rig = repair_rig(0, true);
        {
            let mut cloud = rig.cloud.lock().unwrap();
            for (index, did) in ["member-a", "member-b"].into_iter().enumerate() {
                if failures[index] {
                    cloud
                        .property_codes
                        .insert((did.into(), 4, 1), -704_030_013);
                }
            }
        }
        block_on(rig.run(|c| async move {
            repair_ready(&c).await;
            let light = function(&c.context.registry, "group-1", "main");
            for (index, did) in ["member-a", "member-b"].into_iter().enumerate() {
                c.cloud
                    .lock()
                    .unwrap()
                    .raw_values
                    .insert((did.into(), 2, 1), json!(powers[index]));
                let work = c.context.live.borrow().devices[did]
                    .admitted
                    .clone()
                    .unwrap();
                work.source_changed();
                c.context.refresh_group_states(did);
            }
            until(|| {
                ["member-a", "member-b"]
                    .iter()
                    .enumerate()
                    .all(|(index, did)| {
                        let snapshot = function(&c.context.registry, did, "main").snapshot();
                        snapshot.power.value() == powers[index]
                            && snapshot.lighting.unwrap().mode.is_some()
                    })
            })
            .await;
            assert_eq!(
                light.snapshot().power,
                if powers.contains(&Some(true)) {
                    Power::On
                } else {
                    Power::Unknown
                }
            );
            let action = light.begin_light_action(rgb(0x00ff00), true).unwrap();
            assert_eq!(
                action.accepted().await.is_ok(),
                accepted,
                "{powers:?} {failures:?}"
            );
            settle().await;
            for (index, did) in ["member-a", "member-b"].into_iter().enumerate() {
                assert_eq!(
                    !member_writes(&c, did, 4, 1).is_empty(),
                    powers[index] == Some(true)
                );
                assert!(member_writes(&c, did, 4, 2).is_empty());
                assert!(member_writes(&c, did, 2, 1).is_empty());
                if powers[index] != Some(true) {
                    let work = c.context.live.borrow().devices[did]
                        .admitted
                        .clone()
                        .unwrap();
                    work.notify_value(
                        crate::xiaomi::capability::PropertyBinding { siid: 2, piid: 1 },
                        &json!(true),
                        false,
                        false,
                    );
                }
            }
            settle().await;
            for (index, did) in ["member-a", "member-b"].into_iter().enumerate() {
                if powers[index] != Some(true) {
                    assert!(
                        member_writes(&c, did, 4, 1).is_empty(),
                        "power recovery must not replay skipped RGB"
                    );
                }
            }
        }));
    }
}
