use super::*;
use crate::device::{
    Capability, DeviceKey, LightColorTarget, LightDimension, LightMode, LightTarget, NumericRange,
    Power, Rgb,
};
use crate::storage::CachedPhysicalDevice;

pub(super) fn group_record(
    record_did: &str,
    group_did: &str,
    members: &[&str],
    physical: bool,
) -> Value {
    let member_ship = members
        .iter()
        .map(|did| ((*did).to_string(), serde_json::json!("1")))
        .collect::<serde_json::Map<_, _>>();
    serde_json::json!({
        "did": record_did,
        "name": "Native group",
        "model": "mijia.light.group3",
        "spec_type": "urn:miot-spec-v2:device:light:0000A001:mijia-group3:3:0000C802",
        "isOnline": true,
        "extra": {
            "member_list": {
                "group_did": group_did,
                "status": "1",
                "display_member_switch": 0,
                "member_ship": member_ship,
                "is_physical": physical
            }
        }
    })
}

pub(super) fn group_instance() -> Value {
    serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/group3-instance.json"
    ))
    .unwrap()
}

pub(super) fn ls2307_instance() -> Value {
    serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/devcea-light-ls2307.json"
    ))
    .unwrap()
}

const MEMBER_COLOR: crate::xiaomi::capability::PropertyBinding =
    crate::xiaomi::capability::PropertyBinding { siid: 4, piid: 1 };
const MEMBER_MODE: crate::xiaomi::capability::PropertyBinding =
    crate::xiaomi::capability::PropertyBinding { siid: 4, piid: 2 };

/// Two ls2307 members plus one native group3 relation. `member_mode` and
/// `member_power` seed the confirmed member observations.
pub(super) fn repair_rig(member_mode: u64, member_power: bool) -> Rig {
    let rig = Rig::lights(&[
        ("member-a", "devcea.light.ls2307"),
        ("member-b", "devcea.light.ls2307"),
    ]);
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.catalog_groups = vec![group_record(
            "group-record",
            "group-1",
            &["member-a", "member-b"],
            false,
        )];
        cloud.catalog_instances.insert(
            group_instance()["type"].as_str().unwrap().into(),
            group_instance(),
        );
        cloud
            .online
            .extend(["group-1", "member-a", "member-b"].map(str::to_string));
        for did in ["group-1", "member-a", "member-b"] {
            for (siid, piid, value) in [
                (2, 1, json!(member_power)),
                (2, 2, json!(50)),
                (2, 3, json!(2700)),
            ] {
                cloud.raw_values.insert((did.into(), siid, piid), value);
            }
        }
        for did in ["member-a", "member-b"] {
            cloud.raw_values.insert(
                (did.into(), MEMBER_COLOR.siid, MEMBER_COLOR.piid),
                json!(0x112233),
            );
            cloud.raw_values.insert(
                (did.into(), MEMBER_MODE.siid, MEMBER_MODE.piid),
                json!(member_mode),
            );
        }
    }
    rig
}

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

pub(super) async fn repair_ready(c: &Control) {
    c.announce();
    until(|| {
        c.context
            .live
            .borrow()
            .devices
            .get("group-1")
            .is_some_and(|slot| slot.admitted.is_some())
    })
    .await;
    until(|| {
        ["member-a", "member-b"].iter().all(|did| {
            c.context
                .live
                .borrow()
                .devices
                .get(*did)
                .is_some_and(|slot| slot.admitted.is_some() && slot.work.is_some())
        })
    })
    .await;
    until(|| {
        c.context
            .registry
            .get(&DeviceKey::new("xiaomi", "group-1"))
            .is_some_and(|physical| physical.availability() == Availability::Available)
    })
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
        let capabilities = light.description().capabilities.clone();
        assert!(
            capabilities.contains(&Capability::ColorRgb),
            "repaired group must publish RGB: {capabilities:?}"
        );
        assert_eq!(
            light.description().temperature_range(),
            Some(NumericRange {
                min: 2700,
                max: 6500,
                step: 1
            })
        );
        let action = light
            .begin_light_action(
                LightDimension::Color,
                LightTarget {
                    color: Some(LightColorTarget::Rgb(Rgb {
                        red: 0,
                        green: 255,
                        blue: 0,
                    })),
                    ..Default::default()
                },
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
                Some(json!(0x00ff00))
            );
            assert_eq!(
                member_writes(&c, did, MEMBER_MODE.siid, MEMBER_MODE.piid)
                    .last()
                    .cloned(),
                Some(json!(1)),
                "member color mode must switch to RGB"
            );
        }
        assert!(
            group_writes(&c, "group-1", MEMBER_COLOR.siid, MEMBER_COLOR.piid).is_empty(),
            "the group DID has no ambient binding"
        );
    }));
}

#[test]
fn repaired_group_ct_target_writes_native_value_and_member_white_mode() {
    let rig = repair_rig(1, true);
    block_on(rig.run(|c| async move {
        repair_ready(&c).await;
        let light = function(&c.context.registry, "group-1", "main");
        let action = light
            .begin_light_action(
                LightDimension::Color,
                LightTarget {
                    color: Some(LightColorTarget::Temperature(4000)),
                    ..Default::default()
                },
                true,
            )
            .unwrap();
        action.accepted().await.unwrap();
        wait_for(
            || {
                ["member-a", "member-b"].iter().all(|did| {
                    !member_writes(&c, did, MEMBER_MODE.siid, MEMBER_MODE.piid).is_empty()
                })
            },
            "member mode writes were not dispatched",
        )
        .await;
        assert_eq!(
            group_writes(&c, "group-1", 2, 3).last().cloned(),
            Some(json!(4000))
        );
        for did in ["member-a", "member-b"] {
            assert_eq!(
                member_writes(&c, did, MEMBER_MODE.siid, MEMBER_MODE.piid)
                    .last()
                    .cloned(),
                Some(json!(0)),
                "CT must select the member white mode"
            );
            assert!(
                member_writes(&c, did, MEMBER_COLOR.siid, MEMBER_COLOR.piid).is_empty(),
                "CT must not write member color"
            );
            assert!(
                member_writes(&c, did, 2, 3).is_empty(),
                "CT stays on the native group DID"
            );
        }
    }));
}

#[test]
fn repaired_group_native_on_restores_known_member_rgb_mode() {
    let rig = repair_rig(1, true);
    block_on(rig.run(|c| async move {
        repair_ready(&c).await;
        let light = function(&c.context.registry, "group-1", "main");
        light.execute(Command::On).await.unwrap();
        wait_for(
            || {
                ["member-a", "member-b"].iter().all(|did| {
                    !member_writes(&c, did, MEMBER_MODE.siid, MEMBER_MODE.piid).is_empty()
                })
            },
            "member mode writes were not dispatched",
        )
        .await;
        assert_eq!(
            group_writes(&c, "group-1", 2, 1).last().cloned(),
            Some(json!(true))
        );
        for did in ["member-a", "member-b"] {
            assert_eq!(
                member_writes(&c, did, MEMBER_MODE.siid, MEMBER_MODE.piid)
                    .last()
                    .cloned(),
                Some(json!(1)),
                "native On must not silently drop the known RGB mode"
            );
        }
    }));
}

#[test]
fn repaired_group_state_follows_confirmed_member_and_clears_on_loss() {
    let rig = repair_rig(0, true);
    block_on(rig.run(|c| async move {
        repair_ready(&c).await;
        let light = function(&c.context.registry, "group-1", "main");
        let work = c.context.live.borrow().devices["member-a"]
            .work
            .clone()
            .unwrap();
        work.notify_value(MEMBER_COLOR, &json!(0x00ff00), false);
        work.notify_value(MEMBER_MODE, &json!(1), false);
        // The production notification path refreshes repaired group state
        // after an actual member observation.
        c.context.refresh_repair_groups("member-a");
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
        c.context
            .block_device("member-a", "test member outage".into());
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
        c.context
            .block_device("member-b", "test member outage".into());
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
    LightTarget {
        brightness_percent: Some(value),
        ..LightTarget::default()
    }
}

fn rgb(value: u32) -> LightTarget {
    LightTarget {
        color: Some(LightColorTarget::Rgb(Rgb::from_packed(value).unwrap())),
        ..LightTarget::default()
    }
}

/// Let the outer executor honour the released DID gate and poll the queued
/// member task. This is scheduling, not a timeout.
async fn settle() {
    for _ in 0..64 {
        future::yield_now().await;
    }
}

#[test]
fn repaired_group_stop_retires_queued_member_sends() {
    let (rig, release) = gated_rig(0, true);
    block_on(rig.run(|c| async move {
        repair_ready(&c).await;
        let (released, wait_released) = flume::bounded::<()>(1);
        let command = {
            let member = function(&c.context.registry, "member-b", "main");
            async move {
                let _ = member.execute(Command::Off).await;
                let _ = released.send(());
            }
        };
        let scenario = async {
            wait_holding_member_b(&c).await;
            let light = function(&c.context.registry, "group-1", "main");
            let action = light
                .begin_light_action(LightDimension::Color, rgb(0x00ff00), false)
                .unwrap();
            action.accepted().await.unwrap();
            wait_for(
                || !member_writes(&c, "member-a", MEMBER_COLOR.siid, MEMBER_COLOR.piid).is_empty(),
                "fast member did not receive its RGB batch",
            )
            .await;
            light.stop_light(LightDimension::Color).unwrap();
            release.send(()).unwrap();
            let _ = wait_released.recv_async().await;
            settle().await;
            assert!(
                member_writes(&c, "member-b", MEMBER_COLOR.siid, MEMBER_COLOR.piid).is_empty(),
                "Stop must retire queued member sends"
            );
        };
        future::zip(command, scenario).await;
    }));
}

#[test]
fn repaired_group_final_early_success_keeps_owned_member_sends() {
    let (rig, release) = gated_rig(0, true);
    block_on(rig.run(|c| async move {
        repair_ready(&c).await;
        let light = function(&c.context.registry, "group-1", "main");
        let action = light
            .begin_light_action(LightDimension::Color, rgb(0x0000ff), true)
            .unwrap();
        action.accepted().await.unwrap();
        drop(action);
        // Stop after a completed final action is a no-op and must not cancel
        // the already-owned member receipt.
        light.stop_light(LightDimension::Color).unwrap();
        release.send(()).unwrap();
        wait_for(
            || {
                member_writes(&c, "member-b", MEMBER_COLOR.siid, MEMBER_COLOR.piid)
                    .last()
                    .cloned()
                    == Some(json!(0x0000ff))
            },
            "slow member lost its owned final send after early acceptance",
        )
        .await;
    }));
}

#[test]
fn repaired_group_new_action_retires_old_queued_member_sends() {
    let (rig, release) = gated_rig(0, true);
    block_on(rig.run(|c| async move {
        repair_ready(&c).await;
        let (released, wait_released) = flume::bounded::<()>(1);
        let command = {
            let member = function(&c.context.registry, "member-b", "main");
            async move {
                let _ = member.execute(Command::Off).await;
                let _ = released.send(());
            }
        };
        let scenario = async {
            wait_holding_member_b(&c).await;
            let light = function(&c.context.registry, "group-1", "main");
            let action = light
                .begin_light_action(LightDimension::Color, rgb(0x00ff00), false)
                .unwrap();
            action.accepted().await.unwrap();
            let next = light
                .begin_light_action(LightDimension::Color, rgb(0xff0000), true)
                .unwrap();
            next.accepted().await.unwrap();
            release.send(()).unwrap();
            let _ = wait_released.recv_async().await;
            settle().await;
            assert!(
                !member_writes(&c, "member-b", MEMBER_COLOR.siid, MEMBER_COLOR.piid)
                    .contains(&json!(0x00ff00)),
                "the obsolete frame must not reach the slow member"
            );
        };
        future::zip(command, scenario).await;
    }));
}

#[test]
fn repaired_group_rebuild_retires_unsent_member_sends() {
    let (rig, release) = gated_rig(0, true);
    block_on(rig.run(|c| async move {
        repair_ready(&c).await;
        let (released, wait_released) = flume::bounded::<()>(1);
        let command = {
            let member = function(&c.context.registry, "member-b", "main");
            async move {
                let _ = member.execute(Command::Off).await;
                let _ = released.send(());
            }
        };
        let scenario = async {
            wait_holding_member_b(&c).await;
            let light = function(&c.context.registry, "group-1", "main");
            let action = light
                .begin_light_action(LightDimension::Color, rgb(0x00ff00), false)
                .unwrap();
            action.accepted().await.unwrap();
            c.publication.stop_actions();
            release.send(()).unwrap();
            let _ = wait_released.recv_async().await;
            settle().await;
            assert!(
                member_writes(&c, "member-b", MEMBER_COLOR.siid, MEMBER_COLOR.piid).is_empty(),
                "a committed rebuild must retire unsent member frames"
            );
        };
        future::zip(command, scenario).await;
    }));
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
            false,
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
            light.description().temperature_range(),
            Some(NumericRange {
                min: 2700,
                max: 6450,
                step: 1
            })
        );
        let action = light
            .begin_light_action(
                LightDimension::Color,
                LightTarget {
                    color: Some(LightColorTarget::Temperature(6450)),
                    ..LightTarget::default()
                },
                true,
            )
            .unwrap();
        action.accepted().await.unwrap();
        wait_for(
            || group_writes(&c, "group-1", 2, 3).last().cloned() == Some(json!(6500)),
            "logical target must quantize on the genuine native grid",
        )
        .await;
        for did in ["member-a", "member-b"] {
            assert_eq!(
                member_writes(&c, did, MEMBER_MODE.siid, MEMBER_MODE.piid)
                    .last()
                    .cloned(),
                Some(json!(0)),
            );
        }
        // Actual feedback on the native grid outside the logical interval
        // becomes unknown instead of being rounded to the target.
        c.cloud
            .lock()
            .unwrap()
            .raw_values
            .insert(("group-1".into(), 2, 3), json!(6500));
        let work = c.context.live.borrow().devices["group-1"]
            .work
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

pub(super) fn confirmed_for(
    c: &Control,
    did: &str,
    model: &str,
    instance: &Value,
) -> CachedPhysicalDevice {
    let scope = c.context.live.borrow().scope.clone().unwrap();
    CachedPhysicalDevice {
        uid: scope.uid,
        home_id: scope.home_id,
        group_id: scope.group_id,
        model: model.into(),
        instance_type: instance["type"].as_str().unwrap().into(),
        device: crate::xiaomi::capability::parse_device_for_model(
            DeviceKey::new("xiaomi", did),
            model,
            instance,
        )
        .unwrap(),
    }
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
            .load_published(&DeviceKey::new("xiaomi", "group-1"))
            .unwrap()
            .unwrap();
        let identity = committed.unique_id.clone();
        let endpoint = committed.endpoint;
        let version = committed.configuration_version;
        let scope = c.context.live.borrow().scope.clone().unwrap();
        let root = c
            .store
            .devices()
            .load_publication(Some(&scope))
            .unwrap()
            .root_version;

        // The latest genuine member input drops RGB. The static independent
        // lamp contract blocks that member while the group keeps serving.
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
            .native_confirmed = Some(downgraded.clone());
        c.context.block_device(
            "member-a",
            "descriptor changed to an incompatible profile".into(),
        );

        // A failed save keeps the committed repair plan.
        let db = rusqlite::Connection::open(c.store.path()).unwrap();
        db.execute_batch(
            "CREATE TRIGGER fail_config BEFORE INSERT ON device_cache
             BEGIN SELECT RAISE(ABORT, 'injected'); END;",
        )
        .unwrap();
        c.context.stage();
        until(|| c.publication.failure().is_some()).await;
        assert!(
            light
                .description()
                .capabilities
                .contains(&Capability::ColorRgb),
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
                    .is_none_or(|function| {
                        !function
                            .description()
                            .capabilities
                            .contains(&Capability::ColorRgb)
                    })
            },
            "successful commit must fall back to the native profile",
        )
        .await;
        assert!(c.context.group_repair("group-1").is_none());
        let saved = c.store.devices().load_publication(Some(&scope)).unwrap();
        let native_a = saved
            .cached
            .iter()
            .find(|cached| cached.device.key.id == "member-a")
            .expect("member native input restored from the committed configuration");
        let stored_a = saved
            .records
            .iter()
            .find(|record| record.key.id == "member-a")
            .expect("member logical identity restored");
        assert_eq!(native_a.model, "yeelink.light.light3");
        assert!(
            native_a.device.functions[0]
                .lighting
                .as_ref()
                .unwrap()
                .color
                .is_none(),
            "the persisted confirmed input is the genuine new Native description"
        );
        assert!(
            stored_a.functions[0]
                .capabilities
                .contains(&Capability::ColorRgb),
            "the stored logical description may intentionally differ and stays static"
        );
        assert!(saved.root_version > root);

        // A restart rebuild sees the persisted Native input: the committed
        // native profile and range survive without a widening or version bump.
        let restarted = crate::publication::Publication::open(
            crate::xiaomi::adapter::SourceHandle(c.context.clone()),
            c.store.clone(),
        )
        .unwrap();
        assert!(
            restarted
                .prepare()
                .unwrap()
                .is_none_or(|prepared| !prepared.rebuild),
            "restart must not widen or rebuild from stale cached inputs"
        );
        assert!(c.context.group_repair("group-1").is_none());

        // A genuinely compatible fresh member description restores repair and
        // keeps identity/endpoint while bumping the fixed version.
        let ls = serde_json::from_str(include_str!(
            "../../../../tests/fixtures/miot/devcea-light-ls2307.json"
        ))
        .unwrap();
        let recovered = confirmed_for(&c, "member-a", "devcea.light.ls2307", &ls);
        {
            let mut live = c.context.live.borrow_mut();
            let slot = live.devices.get_mut("member-a").unwrap();
            slot.native_confirmed = Some(recovered.clone());
            slot.candidate = Some((recovered, None));
        }
        c.context.reconcile("member-a").unwrap();
        wait_for(
            || c.context.group_repair("group-1").is_some(),
            "re-confirmed members must restore repair",
        )
        .await;
        let light = function(&c.context.registry, "group-1", "main");
        assert!(
            light
                .description()
                .capabilities
                .contains(&Capability::ColorRgb)
        );
        let again = c
            .store
            .devices()
            .load_published(&DeviceKey::new("xiaomi", "group-1"))
            .unwrap()
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
        let denied = light
            .begin_light_action(LightDimension::Color, rgb(0x00ff00), true)
            .unwrap();
        assert!(denied.accepted().await.is_err());
        // The existing cloud coordination refreshes the rejected token.
        until(|| c.cloud.lock().unwrap().tokens > 0).await;
        c.cloud.lock().unwrap().property_status = 200;
        until(|| c.context.token_usable()).await;
        let next = light
            .begin_light_action(LightDimension::Color, rgb(0x0000ff), true)
            .unwrap();
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

const SHARED_BRIGHTNESS: crate::xiaomi::capability::PropertyBinding =
    crate::xiaomi::capability::PropertyBinding { siid: 2, piid: 2 };
const SHARED_TEMPERATURE: crate::xiaomi::capability::PropertyBinding =
    crate::xiaomi::capability::PropertyBinding { siid: 2, piid: 3 };

#[test]
fn shared_typed_member_echo_and_stop_are_group_local() {
    // group-1 is repaired (both ls2307 members); group-2 is native-only
    // because it also contains the ordinary OBCT member-c, yet it shares the
    // genuine typed ls2307 member-a.
    let rig = Rig::lights(&[
        ("member-a", "devcea.light.ls2307"),
        ("member-b", "devcea.light.ls2307"),
        ("member-c", "yeelink.light.light3"),
    ]);
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.catalog_groups = vec![
            group_record("group-record", "group-1", &["member-a", "member-b"], false),
            group_record(
                "group-record-2",
                "group-2",
                &["member-a", "member-c"],
                false,
            ),
        ];
        cloud.catalog_instances.insert(
            group_instance()["type"].as_str().unwrap().into(),
            group_instance(),
        );
        cloud
            .online
            .extend(["group-1", "group-2", "member-a", "member-b", "member-c"].map(str::to_string));
        for did in ["group-1", "group-2", "member-a", "member-b", "member-c"] {
            for (siid, piid, value) in [(2, 1, json!(true)), (2, 2, json!(50)), (2, 3, json!(2700))]
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
                .insert((did.into(), MEMBER_MODE.siid, MEMBER_MODE.piid), json!(0));
        }
    }
    block_on(rig.run(|c| async move {
        repair_ready(&c).await;
        until(|| {
            ["group-2", "member-c"].iter().all(|did| {
                c.context
                    .registry
                    .get(&DeviceKey::new("xiaomi", *did))
                    .is_some()
            })
        })
        .await;
        let peer = Rc::new(Peer::default());
        peer.value("member-a", SHARED_BRIGHTNESS, json!(50));
        peer.value("member-a", SHARED_TEMPERATURE, json!(2700));
        peer.value("member-a", MEMBER_MODE, json!(0));
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| control.context.active_notification("member-a") == Some(PathKind::Local))
                .await;
            let group1 = function(&control.context.registry, "group-1", "main");
            let group2 = function(&control.context.registry, "group-2", "main");
            assert!(control.context.group_repair("group-2").is_none());

            // Own converted echo must not stop the native group's fade. The
            // group keeps its own native value; only the member observation
            // moves to 80.
            let fade2 = group2
                .begin_light_action(LightDimension::Brightness, brightness(80.0), false)
                .unwrap();
            fade2.accepted().await.unwrap();
            peer.notify_value("member-a", SHARED_BRIGHTNESS, json!(80), false);
            let member_a = function(&control.context.registry, "member-a", "main");
            until(|| member_a.snapshot().lighting.unwrap().brightness_percent == Some(80.0)).await;
            assert!(
                group2.light_action_current(fade2.token()),
                "own converted member echo must not stop the native group fade"
            );
            assert_eq!(
                group2.snapshot().lighting.unwrap().brightness_percent,
                Some(50.0),
                "member values never aggregate into native group state"
            );

            // An unmatched external member value stops the dimension on every
            // group that shares the member.
            let fade1 = group1
                .begin_light_action(LightDimension::Brightness, brightness(30.0), false)
                .unwrap();
            fade1.accepted().await.unwrap();
            peer.notify_value("member-a", SHARED_BRIGHTNESS, json!(42), false);
            until(|| {
                !group1.light_action_current(fade1.token())
                    && !group2.light_action_current(fade2.token())
            })
            .await;

            // Stop applies only to the targeted group's own dimension: group-1
            // runs a brightness track while group-2 runs CT.
            let fade1 = group1
                .begin_light_action(LightDimension::Brightness, brightness(20.0), false)
                .unwrap();
            fade1.accepted().await.unwrap();
            let color2 = group2
                .begin_light_action(
                    LightDimension::Color,
                    LightTarget {
                        color: Some(LightColorTarget::Temperature(4200)),
                        ..LightTarget::default()
                    },
                    false,
                )
                .unwrap();
            color2.accepted().await.unwrap();
            group2.stop_light(LightDimension::Color).unwrap();
            assert!(!group2.light_action_current(color2.token()));
            assert!(
                group1.light_action_current(fade1.token()),
                "Stop must not stop a shared member's other group"
            );
        })
        .await;
    }));
}

fn batch_containing(c: &Control, did: &str, siid: u32, piid: u32, value: Value) -> Option<usize> {
    c.cloud
        .lock()
        .unwrap()
        .numeric_writes
        .iter()
        .position(|(items, _)| {
            items.iter().any(|item| {
                item["did"] == did
                    && item["siid"] == siid
                    && item["piid"] == piid
                    && item["value"] == value
            })
        })
}

#[test]
fn repaired_group_known_off_rgb_sends_final_off_after_color_and_mode() {
    let rig = repair_rig(0, true);
    rig.cloud
        .lock()
        .unwrap()
        .raw_values
        .insert(("group-1".into(), 2, 1), json!(false));
    block_on(rig.run(|c| async move {
        repair_ready(&c).await;
        let light = function(&c.context.registry, "group-1", "main");
        wait_for(
            || light.snapshot().power == Power::Off,
            "group high/low state did not settle to off",
        )
        .await;
        let action = light
            .begin_light_action(LightDimension::Color, rgb(0x00ff00), true)
            .unwrap();
        action.accepted().await.unwrap();
        wait_for(
            || {
                batch_containing(&c, "member-a", 4, 1, json!(0x00ff00)).is_some()
                    && batch_containing(&c, "member-a", 2, 1, json!(false)).is_some()
            },
            "member RGB/mode batch and final off did not arrive",
        )
        .await;
        let color_at =
            batch_containing(&c, "member-a", 4, 1, json!(0x00ff00)).expect("member color batch");
        let off_at =
            batch_containing(&c, "member-a", 2, 1, json!(false)).expect("member off batch");
        assert!(off_at > color_at, "final off must follow color/mode");
        {
            let writes = c.cloud.lock().unwrap();
            let (items, _) = &writes.numeric_writes[color_at];
            assert!(
                items
                    .iter()
                    .any(|item| item["siid"] == 4 && item["piid"] == 2),
                "mode travels with the member color batch"
            );
        }
        assert!(
            group_writes(&c, "group-1", 4, 1).is_empty(),
            "no fabricated group RGB binding"
        );
        // Real per-DID diagnostics for the member's own OFF batch.
        wait_for(
            || {
                let status = c
                    .context
                    .commands
                    .snapshot("member-a", ["main".to_string()].into_iter());
                status[0].last.as_ref().is_some_and(|last| {
                    last.actual_path == Some(PathKind::Cloud)
                        && last.result == Some(Ok(()))
                        && last.properties.iter().any(|(binding, value)| {
                            *binding
                                == (crate::xiaomi::capability::PropertyBinding { siid: 2, piid: 1 })
                                && value == &json!(false)
                        })
                })
            },
            "member OFF batch diagnostics were not recorded",
        )
        .await;
    }));
}

#[test]
fn repaired_group_partial_required_off_is_not_full_but_other_member_succeeds() {
    let rig = repair_rig(0, false);
    rig.cloud
        .lock()
        .unwrap()
        .raw_values
        .insert(("group-1".into(), 2, 1), json!(false));
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud
            .property_codes
            .insert(("member-a".into(), 2, 1), -704_030_013);
    }
    block_on(rig.run(|c| async move {
        repair_ready(&c).await;
        let light = function(&c.context.registry, "group-1", "main");
        wait_for(
            || light.snapshot().power == Power::Off,
            "group state did not settle off",
        )
        .await;
        let action = light
            .begin_light_action(LightDimension::Color, rgb(0x0000ff), true)
            .unwrap();
        action.accepted().await.unwrap();
        wait_for(
            || {
                !member_writes(&c, "member-a", 4, 1).is_empty()
                    && !member_writes(&c, "member-a", 2, 1).is_empty()
            },
            "rejected member's owned batch must still be attempted",
        )
        .await;
        assert!(
            !member_writes(&c, "member-b", 4, 1).is_empty(),
            "the full member satisfies logical acceptance"
        );
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
                LightDimension::Color,
                LightTarget {
                    color: Some(LightColorTarget::Temperature(4200)),
                    ..LightTarget::default()
                },
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
            member_writes(&c, "member-a", 4, 2).is_empty(),
            "member auxiliaries must not start after a native failure"
        );
    }));
}

#[test]
fn repaired_group_ct_from_rgb_selects_member_white_and_keeps_known_on() {
    let rig = repair_rig(1, true);
    block_on(rig.run(|c| async move {
        repair_ready(&c).await;
        let light = function(&c.context.registry, "group-1", "main");
        let action = light
            .begin_light_action(
                LightDimension::Color,
                LightTarget {
                    color: Some(LightColorTarget::Temperature(4000)),
                    ..LightTarget::default()
                },
                true,
            )
            .unwrap();
        action.accepted().await.unwrap();
        wait_for(
            || {
                ["member-a", "member-b"]
                    .iter()
                    .all(|did| member_writes(&c, did, 4, 2).last().cloned() == Some(json!(0)))
            },
            "CT must select the real member white mode",
        )
        .await;
        assert_eq!(
            group_writes(&c, "group-1", 2, 3).last().cloned(),
            Some(json!(4000))
        );
        for did in ["member-a", "member-b"] {
            assert!(
                member_writes(&c, did, 2, 3).is_empty(),
                "native CT stays on the group DID"
            );
            assert!(
                member_writes(&c, did, 2, 1).is_empty(),
                "known-on member needs no power restore"
            );
        }
    }));
}

#[test]
fn repaired_group_all_members_partial_is_a_logical_failure() {
    let rig = repair_rig(0, true);
    rig.cloud
        .lock()
        .unwrap()
        .raw_values
        .insert(("group-1".into(), 2, 1), json!(false));
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud
            .property_codes
            .insert(("member-a".into(), 4, 2), -704_030_013);
        cloud
            .property_codes
            .insert(("member-b".into(), 4, 1), -704_030_013);
    }
    block_on(rig.run(|c| async move {
        repair_ready(&c).await;
        let light = function(&c.context.registry, "group-1", "main");
        wait_for(
            || light.snapshot().power == Power::Off,
            "group state did not settle off",
        )
        .await;
        let action = light
            .begin_light_action(LightDimension::Color, rgb(0x00ff00), true)
            .unwrap();
        assert!(action.accepted().await.is_err());
    }));
}

#[test]
fn repaired_native_receipt_keeps_mode_and_ct_echoes_but_unmatched_mode_is_external() {
    let rig = repair_rig(1, true);
    let (release, gate) = flume::bounded::<()>(1);
    rig.cloud
        .lock()
        .unwrap()
        .write_gates
        .insert("group-1".into(), gate);
    block_on(rig.run(|c| async move {
        repair_ready(&c).await;
        let peer = Rc::new(Peer::default());
        peer.value("member-a", SHARED_BRIGHTNESS, json!(50));
        peer.value("member-a", SHARED_TEMPERATURE, json!(2700));
        peer.value("member-a", MEMBER_MODE, json!(1));
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| control.context.active_notification("member-a") == Some(PathKind::Local))
                .await;
            let light = function(&control.context.registry, "group-1", "main");
            let action = light
                .begin_light_action(
                    LightDimension::Color,
                    LightTarget {
                        color: Some(LightColorTarget::Temperature(4200)),
                        ..LightTarget::default()
                    },
                    false,
                )
                .unwrap();
            // Hold the native CT receipt; expectations are pending.
            until(|| {
                !group_writes(control, "group-1", 2, 3)
                    .iter()
                    .filter(|value| *value == &json!(4200))
                    .collect::<Vec<_>>()
                    .is_empty()
            })
            .await;
            peer.notify_value("member-a", MEMBER_MODE, json!(0), false);
            peer.notify_value("member-a", SHARED_TEMPERATURE, json!(4200), false);
            for _ in 0..64 {
                future::yield_now().await;
            }
            assert!(
                light.light_action_current(action.token()),
                "own mode/CT echoes before native ACK must not cancel"
            );
            release.send(()).unwrap();
            action.accepted().await.unwrap();
            // The accepted 3s window still owns the same mode/CT echoes.
            peer.notify_value("member-a", MEMBER_MODE, json!(0), false);
            for _ in 0..64 {
                future::yield_now().await;
            }
            assert!(light.light_action_current(action.token()));
            // An unmatched mode value remains an external operation.
            peer.notify_value("member-a", MEMBER_MODE, json!(1), false);
            until(|| !light.light_action_current(action.token())).await;
        })
        .await;
    }));
}

#[test]
fn native_cloud_partial_retains_accepted_expectation_and_retires_rejected() {
    let rig = repair_rig(0, true);
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud
            .property_codes
            .insert(("group-1".into(), 2, 1), -704_030_013);
    }
    block_on(rig.run(|c| async move {
        repair_ready(&c).await;
        let peer = Rc::new(Peer::default());
        peer.value("member-a", SHARED_BRIGHTNESS, json!(50));
        peer.value("member-a", SHARED_TEMPERATURE, json!(2700));
        peer.value("member-a", MEMBER_MODE, json!(0));
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| control.context.active_notification("member-a") == Some(PathKind::Local))
                .await;
            let light = function(&control.context.registry, "group-1", "main");
            // Power rejected, brightness accepted in one cloud native batch.
            let denied = light
                .begin_light_action(
                    LightDimension::Brightness,
                    LightTarget {
                        power: Some(false),
                        brightness_percent: Some(70.0),
                        ..LightTarget::default()
                    },
                    true,
                )
                .unwrap();
            assert!(denied.accepted().await.is_err());
            // A later real fade: the accepted native brightness expectation
            // must still own the encoded member echo.
            let fade = light
                .begin_light_action(LightDimension::Brightness, brightness(30.0), false)
                .unwrap();
            fade.accepted().await.unwrap();
            peer.notify_value("member-a", SHARED_BRIGHTNESS, json!(70), false);
            for _ in 0..96 {
                future::yield_now().await;
            }
            assert!(
                light.light_action_current(fade.token()),
                "accepted cloud native property must keep its expectation"
            );
        })
        .await;
    }));
}

#[test]
fn native_only_group_cloud_partial_retains_accepted_echo_and_retires_rejected() {
    let rig = Rig::lights(&[
        ("member-a", "devcea.light.ls2307"),
        ("member-c", "yeelink.light.light3"),
    ]);
    {
        let mut cloud = rig.cloud.lock().unwrap();
        let mut instance = group_instance();
        instance["type"] = json!("urn:miot-spec-v2:device:light:0000A001:mijia-group3:5:0000C804");
        cloud
            .catalog_instances
            .insert(instance["type"].as_str().unwrap().into(), instance);
        cloud.catalog_instances.insert(
            group_instance()["type"].as_str().unwrap().into(),
            group_instance(),
        );
        cloud.catalog_groups.push(group_record(
            "group-record-n",
            "group-n",
            &["member-a", "member-c"],
            false,
        ));
        cloud
            .online
            .extend(["group-n", "member-a", "member-c"].map(str::to_string));
        for did in ["group-n", "member-a", "member-c"] {
            for (siid, piid, value) in [(2, 1, json!(true)), (2, 2, json!(50)), (2, 3, json!(2700))]
            {
                cloud.raw_values.insert((did.into(), siid, piid), value);
            }
        }
        cloud
            .property_codes
            .insert(("group-n".into(), 2, 1), -704_030_013);
    }
    block_on(rig.run(|c| async move {
        c.announce();
        until(|| {
            c.context
                .registry
                .get(&DeviceKey::new("xiaomi", "group-n"))
                .is_some()
        })
        .await;
        let peer = Rc::new(Peer::default());
        peer.value("member-a", SHARED_BRIGHTNESS, json!(50));
        peer.value("member-a", SHARED_TEMPERATURE, json!(2700));
        peer.value("member-a", MEMBER_MODE, json!(0));
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| control.context.active_notification("member-a") == Some(PathKind::Local))
                .await;
            let light = function(&control.context.registry, "group-n", "main");
            assert!(control.context.is_group("group-n"));
            assert!(control.context.group_repair("group-n").is_none());
            // Power rejected, brightness accepted in one ordinary cloud batch.
            let denied = light
                .begin_light_action(
                    LightDimension::Brightness,
                    LightTarget {
                        power: Some(false),
                        brightness_percent: Some(70.0),
                        ..LightTarget::default()
                    },
                    true,
                )
                .unwrap();
            assert!(denied.accepted().await.is_err());
            let fade = light
                .begin_light_action(LightDimension::Brightness, brightness(30.0), false)
                .unwrap();
            fade.accepted().await.unwrap();
            peer.notify_value("member-a", SHARED_BRIGHTNESS, json!(70), false);
            for _ in 0..96 {
                future::yield_now().await;
            }
            assert!(
                light.light_action_current(fade.token()),
                "accepted ordinary native property must keep its expectation"
            );
            peer.notify_value("member-a", SHARED_BRIGHTNESS, json!(55), false);
            until(|| !light.light_action_current(fade.token())).await;
        })
        .await;
    }));
}

#[test]
fn repaired_native_transport_unknown_retains_submitted_expectation() {
    let rig = repair_rig(0, true);
    block_on(rig.run(|c| async move {
        repair_ready(&c).await;
        let peer = Rc::new(Peer::default());
        peer.value("member-a", SHARED_BRIGHTNESS, json!(50));
        peer.value("member-a", SHARED_TEMPERATURE, json!(2700));
        peer.value("member-a", MEMBER_MODE, json!(0));
        let control = &c;
        c.peer(peer, |_peer| async move {
            let light = function(&control.context.registry, "group-1", "main");
            let gate = control.context.live.borrow().devices["member-a"]
                .work
                .as_ref()
                .unwrap()
                .light_write
                .clone()
                .unwrap();
            // The transport provably submitted but the receipt is unknown.
            control.cloud.lock().unwrap().property_status = 503;
            let denied = light
                .begin_light_action(
                    LightDimension::Brightness,
                    LightTarget {
                        power: Some(true),
                        brightness_percent: Some(70.0),
                        ..LightTarget::default()
                    },
                    true,
                )
                .unwrap();
            assert!(denied.accepted().await.is_err());
            assert!(
                gate.is_feedback(SHARED_BRIGHTNESS, &json!(70)),
                "unknown-submitted property must keep its exact bounded expectation"
            );
            assert!(!gate.is_feedback(SHARED_BRIGHTNESS, &json!(55)));
            control.clock.advance(Duration::from_secs(4));
            assert!(
                !gate.is_feedback(SHARED_BRIGHTNESS, &json!(70)),
                "the bounded window must expire"
            );
        })
        .await;
    }));
}
