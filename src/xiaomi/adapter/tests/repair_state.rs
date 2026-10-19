use super::{
    support::{group_instance, group_record, repair_ready, repair_rig},
    *,
};
use crate::device::{Capability, LightMode, LightPatch, LightSnapshot, Rgb};

#[test]
fn representative_cannot_mix_a_new_source_mode_with_an_old_rgb_cache() {
    let rig = repair_rig(1, false);
    block_on(rig.run(|c| async move {
        repair_ready(&c).await;
        until(|| {
            ["member-a", "member-b"].iter().all(|did| {
                function(&c.context.registry, did, "main")
                    .snapshot()
                    .lighting
                    .is_some_and(|state| state.mode.is_some() && state.color.is_some())
            })
        })
        .await;
        let light = function(&c.context.registry, "group-1", "main");
        let work = c.context.live.borrow().devices["member-a"]
            .work
            .clone()
            .unwrap();
        let color = crate::xiaomi::capability::PropertyBinding { siid: 4, piid: 1 };
        let mode = crate::xiaomi::capability::PropertyBinding { siid: 4, piid: 2 };
        work.notify_value(color, &json!(0x00ff00), false, false);
        c.context.refresh_group_states("member-a");
        assert_eq!(
            light.snapshot().lighting.unwrap().color,
            Rgb::from_packed(0x00ff00)
        );

        // Only a mode has arrived from the new source authority. Its missing
        // RGB cannot be filled with the previous authority's cached value.
        work.source_changed();
        work.notify_value(mode, &json!(0), false, false);
        c.context.refresh_group_states("member-a");
        let state = light.snapshot().lighting.unwrap();
        assert_eq!(state.mode, Some(LightMode::Temperature));
        assert_eq!(state.color, None);

        // A readable unknown-mode A and a valid but OFF B are in the same
        // fallback tier, so stable member order selects A and keeps it unknown.
        work.source_changed();
        work.notify_value(
            crate::xiaomi::capability::PropertyBinding { siid: 2, piid: 1 },
            &json!(true),
            false,
            false,
        );
        c.context.refresh_group_states("member-a");
        let state = light.snapshot().lighting.unwrap();
        assert_eq!(state.mode, None);
        assert_eq!(state.color, None);
    }));
}

#[test]
fn virtual_power_and_brightness_use_current_members_without_treating_unknown_as_off() {
    let rig = repair_rig(0, true);
    block_on(rig.run(|c| async move {
        repair_ready(&c).await;
        let group = function(&c.context.registry, "group-1", "main");
        let members =
            ["member-a", "member-b"].map(|did| function(&c.context.registry, did, "main"));
        use Power::{Off, On, Unknown};
        for (powers, levels, expected_power, expected_level) in [
            ([On, On], [Some(20.0), Some(80.0)], On, Some(50.0)),
            ([On, Off], [Some(20.0), Some(80.0)], On, Some(20.0)),
            ([Off, Off], [Some(20.0), Some(80.0)], Off, Some(50.0)),
            ([Off, Unknown], [Some(20.0), Some(80.0)], Unknown, None),
            ([On, Unknown], [Some(20.0), Some(80.0)], On, Some(20.0)),
            ([On, On], [None, Some(80.0)], On, Some(80.0)),
            ([Off, Off], [None, Some(80.0)], Off, Some(80.0)),
            ([On, Off], [None, Some(80.0)], On, None),
            ([On, On], [None, None], On, None),
        ] {
            for (index, member) in members.iter().enumerate() {
                let mut state = member.snapshot().lighting.unwrap();
                state.brightness_percent = levels[index];
                member.replace_light_state(powers[index], state);
            }
            c.context.refresh_group_states("member-a");
            let state = group.snapshot();
            assert_eq!(state.power, expected_power, "{powers:?}");
            assert_eq!(
                state.lighting.unwrap().brightness_percent,
                expected_level,
                "{powers:?}"
            );
        }

        members[0].observe(false);
        let member_b = c
            .context
            .registry
            .get(&DeviceKey::new("xiaomi", "member-b"))
            .unwrap();
        member_b.set_availability(Availability::Unavailable("test".into()));
        c.context.refresh_group_states("member-b");
        assert_eq!(group.snapshot().power, Power::Unknown);
        assert_eq!(group.snapshot().lighting.unwrap().brightness_percent, None);
        members[0].observe(true);
        c.context.refresh_group_states("member-a");
        assert_eq!(group.snapshot().power, Power::On);

        member_b.set_availability(Availability::Available);
        members[0].observe(false);
        members[1].observe(false);
        c.context
            .live
            .borrow_mut()
            .devices
            .get_mut("group-1")
            .unwrap()
            .effective_group
            .as_mut()
            .unwrap()
            .members
            .push("missing-description".into());
        c.context.refresh_group_state("group-1");
        assert_eq!(
            group.snapshot().power,
            Power::Unknown,
            "a missing member remains in the all-off decision"
        );
    }));
}

#[test]
fn virtual_color_uses_one_stable_snapshot_and_clears_missing_fields() {
    let rig = repair_rig(0, true);
    block_on(rig.run(|c| async move {
        repair_ready(&c).await;
        let group = function(&c.context.registry, "group-1", "main");
        let a = function(&c.context.registry, "member-a", "main");
        let b = function(&c.context.registry, "member-b", "main");
        a.observe_light(LightPatch {
            temperature_kelvin: Some(3000),
            color: Rgb::from_packed(0xff0000),
            mode: Some(LightMode::Rgb),
            ..LightPatch::default()
        })
        .unwrap();
        b.observe_light(LightPatch {
            temperature_kelvin: Some(6000),
            color: Rgb::from_packed(0x0000ff),
            mode: Some(LightMode::Temperature),
            ..LightPatch::default()
        })
        .unwrap();
        c.context.refresh_group_states("member-a");
        let before = group.snapshot();
        let color = before.lighting.unwrap();
        assert_eq!(color.temperature_kelvin, Some(3000));
        assert_eq!(color.color, Rgb::from_packed(0xff0000));
        assert_eq!(color.mode, Some(LightMode::Rgb));
        c.context
            .live
            .borrow_mut()
            .devices
            .get_mut("group-1")
            .unwrap()
            .effective_group
            .as_mut()
            .unwrap()
            .members
            .reverse();
        c.context.refresh_group_states("member-b");
        assert_eq!(
            group.snapshot(),
            before,
            "notification or relation order must not change the representative"
        );

        a.observe(false);
        let mut b_state = b.snapshot().lighting.unwrap();
        b_state.color = None;
        b.replace_light_state(Power::On, b_state);
        c.context.refresh_group_states("member-a");
        let color = group.snapshot().lighting.unwrap();
        assert_eq!(color.temperature_kelvin, Some(6000));
        assert_eq!(
            color.color, None,
            "the new representative must clear the old RGB"
        );
        assert_eq!(color.mode, Some(LightMode::Temperature));

        c.context
            .registry
            .get(&DeviceKey::new("xiaomi", "group-1"))
            .unwrap()
            .set_availability(Availability::Unavailable("test".into()));
        c.context.refresh_group_state("group-1");
        assert_eq!(group.snapshot().power, Power::Unknown);
        assert_eq!(
            group.snapshot().lighting,
            Some(LightSnapshot {
                brightness_percent: None,
                temperature_kelvin: None,
                color: None,
                mode: None,
            })
        );
    }));
}

#[test]
fn unrepaired_virtual_group_does_not_infer_white_mode_or_clamp_member_feedback() {
    let rig = Rig::lights(&[
        ("member-a", "devcea.light.ls2307"),
        ("member-b", "yeelink.light.ml9"),
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
    }
    block_on(rig.run(|c| async move {
        c.announce();
        until(|| {
            ["group-1", "member-a", "member-b"].iter().all(|did| {
                c.context
                    .live
                    .borrow()
                    .devices
                    .get(*did)
                    .is_some_and(|slot| {
                        slot.work.is_some()
                            && slot.admitted.as_ref().is_some_and(|record| {
                                record.physical.availability() == Availability::Available
                            })
                    })
            })
        })
        .await;
        let group = function(&c.context.registry, "group-1", "main");
        assert!(c.context.group_repair("group-1").is_none());
        assert!(!group.description().has_capability(Capability::ColorRgb));
        let a = function(&c.context.registry, "member-a", "main");
        let b = function(&c.context.registry, "member-b", "main");
        a.observe_light(LightPatch {
            power: Some(true),
            brightness_percent: Some(20.0),
            temperature_kelvin: Some(6500),
            color: Rgb::from_packed(0xff0000),
            mode: Some(LightMode::Rgb),
        })
        .unwrap();
        b.observe_light(LightPatch {
            power: Some(false),
            brightness_percent: Some(80.0),
            temperature_kelvin: Some(4000),
            ..LightPatch::default()
        })
        .unwrap();
        c.context.refresh_group_states("member-a");
        let snapshot = group.snapshot();
        let color = snapshot.lighting.unwrap();
        assert_eq!(snapshot.power, Power::On);
        assert_eq!(color.brightness_percent, Some(20.0));
        assert_eq!(
            color.mode, None,
            "a CT-only group cannot represent the member's RGB mode"
        );
        assert_eq!(color.color, None);
        assert_eq!(
            color.temperature_kelvin, None,
            "6500 K exceeds the published 6400 K bound"
        );
        b.observe(true);
        c.context.refresh_group_states("member-b");
        let color = group.snapshot().lighting.unwrap();
        assert_eq!(color.brightness_percent, Some(50.0));
        assert_eq!(color.mode, Some(LightMode::Temperature));
        assert_eq!(
            color.temperature_kelvin,
            Some(4000),
            "the valid ON color member takes priority"
        );
    }));
}

#[test]
fn physical_repair_preserves_native_power_brightness_and_temperature() {
    let rig = repair_rig(0, true);
    rig.cloud.lock().unwrap().catalog_groups[0]["extra"]["member_list"]["is_physical"] =
        json!(true);
    block_on(rig.run(|c| async move {
        repair_ready(&c).await;
        let group = function(&c.context.registry, "group-1", "main");
        group
            .observe_light(LightPatch {
                power: Some(false),
                brightness_percent: Some(13.0),
                temperature_kelvin: Some(4000),
                ..LightPatch::default()
            })
            .unwrap();
        let member = function(&c.context.registry, "member-a", "main");
        member
            .observe_light(LightPatch {
                power: Some(true),
                brightness_percent: Some(80.0),
                temperature_kelvin: Some(3000),
                color: Rgb::from_packed(0x00ff00),
                mode: Some(LightMode::Rgb),
            })
            .unwrap();
        c.context.refresh_group_states("member-a");
        let snapshot = group.snapshot();
        let color = snapshot.lighting.unwrap();
        assert_eq!(snapshot.power, Power::Off);
        assert_eq!(color.brightness_percent, Some(13.0));
        assert_eq!(color.temperature_kelvin, Some(4000));
        assert_eq!(color.color, Rgb::from_packed(0x00ff00));
        assert_eq!(color.mode, Some(LightMode::Rgb));
    }));
}
