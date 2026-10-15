use super::{
    repair::{group_instance, group_record, repair_ready, repair_rig},
    *,
};
use crate::device::{LightColorTarget, LightDimension, LightTarget, Rgb};

fn overlapping_groups() -> Rig {
    let rig = repair_rig(0, true);
    {
        let mut cloud = rig.cloud.lock().unwrap();
        cloud.catalog_groups.push(group_record(
            "second-record",
            "group-2",
            &["member-a", "member-b"],
            false,
        ));
        let instance = group_instance();
        cloud
            .catalog_instances
            .insert(instance["type"].as_str().unwrap().into(), instance);
        cloud.online.insert("group-2".into());
        for (piid, value) in [(1, json!(true)), (2, json!(50)), (3, json!(2700))] {
            cloud.raw_values.insert(("group-2".into(), 2, piid), value);
        }
    }
    rig
}

fn rgb(value: u32) -> LightTarget {
    LightTarget {
        color: Some(LightColorTarget::Rgb(Rgb::from_packed(value).unwrap())),
        ..Default::default()
    }
}

#[test]
fn new_group_and_member_targets_take_over_shared_groups_by_dimension() {
    block_on(overlapping_groups().run(|c| async move {
        repair_ready(&c).await;
        until(|| c.context.group_repair("group-2").is_some()).await;
        let first = function(&c.context.registry, "group-1", "main");
        let second = function(&c.context.registry, "group-2", "main");
        let earlier = first
            .begin_light_action(LightDimension::Color, rgb(0x19aa33), false)
            .unwrap();
        earlier.accepted().await.unwrap();
        assert!(first.light_action_current(earlier.token()));
        let newer = second
            .begin_light_action(LightDimension::Color, rgb(0x2255cc), false)
            .unwrap();
        newer.accepted().await.unwrap();
        assert!(
            !first.light_action_current(earlier.token()),
            "a newer target on an overlapping group must stop the older color track"
        );
        assert!(second.light_action_current(newer.token()));

        first.stop_light(LightDimension::Color).unwrap();
        assert!(
            second.light_action_current(newer.token()),
            "Stop is local to the target group"
        );
        let brightness = first
            .begin_light_action(
                LightDimension::Brightness,
                LightTarget {
                    brightness_percent: Some(80.0),
                    ..Default::default()
                },
                false,
            )
            .unwrap();
        brightness.accepted().await.unwrap();
        assert!(first.light_action_current(brightness.token()));
        assert!(
            second.light_action_current(newer.token()),
            "brightness and color remain independent"
        );

        let member = function(&c.context.registry, "member-a", "main");
        member.execute(Command::Off).await.unwrap();
        assert!(!first.light_action_current(brightness.token()));
        assert!(
            !second.light_action_current(newer.token()),
            "direct member off takes over all dimensions"
        );
    }));
}

#[test]
fn member_wire_changes_use_confirmed_fields_and_actual_mode_for_group_takeover() {
    block_on(repair_rig(0, true).run(|c| async move {
        repair_ready(&c).await;
        let peer = Rc::new(Peer::default());
        for did in ["member-a", "member-b"] {
            for (siid, piid, value) in [
                (2, 1, json!(true)),
                (2, 2, json!(50)),
                (2, 3, json!(2700)),
                (4, 1, json!(0x112233)),
                (4, 2, json!(0)),
            ] {
                peer.value(
                    did,
                    crate::xiaomi::capability::PropertyBinding { siid, piid },
                    value,
                );
            }
        }
        let control = &c;
        c.peer(peer, |peer| async move {
            until(|| control.context.active_notification("member-a") == Some(PathKind::Local))
                .await;
            let work = control.context.live.borrow().devices["member-a"]
                .work
                .clone()
                .unwrap();
            until(|| {
                work.confirmed_light().is_some_and(|actual| {
                    actual.brightness_percent == Some(50.0)
                        && actual.mode == Some(crate::device::LightMode::Temperature)
                })
            })
            .await;
            let member = function(&control.context.registry, "member-a", "main");
            member
                .begin_light_action(
                    LightDimension::Brightness,
                    LightTarget {
                        brightness_percent: Some(80.0),
                        ..Default::default()
                    },
                    true,
                )
                .unwrap()
                .accepted()
                .await
                .unwrap();
            assert_eq!(
                member.snapshot().lighting.unwrap().brightness_percent,
                Some(80.0)
            );
            assert_eq!(
                work.confirmed_light().unwrap().brightness_percent,
                Some(50.0)
            );
            let group = function(&control.context.registry, "group-1", "main");
            let fade = group
                .begin_light_action(
                    LightDimension::Brightness,
                    LightTarget {
                        brightness_percent: Some(60.0),
                        ..Default::default()
                    },
                    false,
                )
                .unwrap();
            fade.accepted().await.unwrap();
            control.clock.advance(Duration::from_secs(4));
            peer.notify_value(
                "member-a",
                crate::xiaomi::capability::PropertyBinding { siid: 2, piid: 2 },
                json!(80),
                false,
            );
            until(|| {
                work.confirmed_light()
                    .is_some_and(|actual| actual.brightness_percent == Some(80.0))
            })
            .await;
            assert!(
                !group.light_action_current(fade.token()),
                "an actual change equal to an old accepted target still takes over the group"
            );

            let color = group
                .begin_light_action(LightDimension::Color, rgb(0x00ff00), false)
                .unwrap();
            color.accepted().await.unwrap();
            peer.notify_value(
                "member-a",
                crate::xiaomi::capability::PropertyBinding { siid: 4, piid: 1 },
                json!(0x0000ff),
                false,
            );
            until(|| {
                work.confirmed_light()
                    .is_some_and(|actual| actual.color == Rgb::from_packed(0x0000ff))
            })
            .await;
            assert!(
                group.light_action_current(color.token()),
                "an inactive RGB cache update in confirmed white mode is synchronization"
            );
        })
        .await;
    }));
}

#[test]
fn newer_shared_target_retires_an_older_completed_frames_unsent_member() {
    block_on(overlapping_groups().run(|c| async move {
        repair_ready(&c).await;
        until(|| c.context.group_repair("group-2").is_some()).await;
        let (release, gate) = flume::bounded::<()>(1);
        c.cloud
            .lock()
            .unwrap()
            .write_gates
            .insert("member-b".into(), gate);
        let member = function(&c.context.registry, "member-b", "main");
        let holding = async {
            member.execute(Command::Off).await.unwrap();
        };
        let replace = async {
            until(|| {
                c.cloud
                    .lock()
                    .unwrap()
                    .numeric_writes
                    .iter()
                    .flat_map(|(items, _)| items)
                    .any(|item| item["did"] == "member-b" && item["siid"] == 2 && item["piid"] == 1)
            })
            .await;
            let first = function(&c.context.registry, "group-1", "main");
            first
                .begin_light_action(LightDimension::Color, rgb(0x00ff00), true)
                .unwrap()
                .accepted()
                .await
                .unwrap();
            let second = function(&c.context.registry, "group-2", "main");
            second
                .begin_light_action(LightDimension::Color, rgb(0xff0000), true)
                .unwrap()
                .accepted()
                .await
                .unwrap();
            c.cloud.lock().unwrap().write_gates.remove("member-b");
            release.send(()).unwrap();
        };
        future::zip(holding, replace).await;
        // A real completed member batch is the barrier; the old queued frame
        // cannot hide behind an unprocessed response or an unpolled gate.
        until(|| {
            let status = c
                .context
                .commands
                .snapshot("member-b", ["main".to_owned()].into_iter());
            status[0].inflight.is_none()
                && status[0].last.as_ref().is_some_and(|last| {
                    last.result == Some(Ok(()))
                        && last.properties.iter().any(|(binding, value)| {
                            binding.siid == 4 && binding.piid == 1 && value == &json!(0xff0000)
                        })
                })
        })
        .await;
        assert!(
            !c.cloud
                .lock()
                .unwrap()
                .numeric_writes
                .iter()
                .flat_map(|(items, _)| items)
                .any(|item| item["did"] == "member-b"
                    && item["siid"] == 4
                    && item["piid"] == 1
                    && item["value"] == 0x00ff00),
            "a later shared-group target retires the older owned but unsent color frame"
        );
    }));
}
