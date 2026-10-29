//! Synthetic members exercise group composition across distinct light policies.
use super::*;
use crate::device::{Capability, LightMode, LightTarget, Rgb};

fn generic_rgb() -> Value {
    let mut instance: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/yeelink-light-light3.json"
    ))
    .unwrap();
    instance["services"][1]["properties"]
        .as_array_mut()
        .unwrap()
        .push(json!({
            "iid": 4, "type": "urn:miot-spec-v2:property:color:0000000E:example:1",
            "format": "uint32", "access": ["read", "write", "notify"], "unit": "rgb",
            "value-range": [0, 16777215, 1]
        }));
    instance
}

fn fan_service() -> Value {
    json!({
        "iid": 8, "type": "urn:miot-spec-v2:service:fan:00007808:example:1",
        "properties": [
            {"iid": 1, "type": "urn:miot-spec-v2:property:on:00000006:example:1", "format": "bool", "access": ["read", "write", "notify"]},
            {"iid": 2, "type": "urn:miot-spec-v2:property:fan-level:00000016:example:1", "format": "uint8", "access": ["read", "write", "notify"], "value-range": [1, 3, 1]}
        ]
    })
}

fn group_rig(members: &[(&str, &str, Value)], physical: bool) -> Rig {
    let rig = Rig::custom_lights(members);
    let mut cloud = rig.cloud.lock().unwrap();
    cloud.catalog_groups = vec![group_record(
        "group-record",
        "group-1",
        &members.iter().map(|(did, _, _)| *did).collect::<Vec<_>>(),
        physical,
    )];
    let group = group_instance();
    cloud
        .catalog_instances
        .insert(group["type"].as_str().unwrap().into(), group);
    cloud.online.insert("group-1".into());
    for did in members.iter().map(|(did, _, _)| *did).chain(["group-1"]) {
        cloud.online.insert(did.into());
        for (siid, piid, value) in [(2, 1, json!(true)), (2, 2, json!(50)), (2, 3, json!(3000))] {
            cloud.raw_values.insert((did.into(), siid, piid), value);
        }
    }
    drop(cloud);
    rig
}

#[test]
fn mixed_group_dispatches_each_adapted_policy_and_keeps_composite_fan_independent() {
    let mut patched = ls2307_instance();
    patched["services"]
        .as_array_mut()
        .unwrap()
        .push(fan_service());
    let mut generic = generic_rgb();
    generic["services"][1]["iid"] = json!(9);
    let rig = group_rig(
        &[
            ("member-a", "devcea.light.ls2307", patched),
            ("member-b", "example.rgb", generic),
        ],
        true,
    );
    {
        let mut cloud = rig.cloud.lock().unwrap();
        for (did, siid, piid, value) in [
            ("member-a", 4, 1, json!(0x112233)),
            ("member-a", 4, 2, json!(0)),
            ("member-a", 8, 1, json!(true)),
            ("member-a", 8, 2, json!(2)),
            ("member-b", 9, 1, json!(true)),
            ("member-b", 9, 2, json!(50)),
            ("member-b", 9, 3, json!(3000)),
            ("member-b", 9, 4, json!(0x334455)),
        ] {
            cloud.raw_values.insert((did.into(), siid, piid), value);
        }
    }
    block_on(rig.run(|c| async move {
        c.announce();
        until(|| c.context.group_repair("group-1").is_some()).await;
        let group = function(&c.context.registry, "group-1", "light:2");
        let member = function(&c.context.registry, "member-a", "light:2");
        let fan = function(&c.context.registry, "member-a", "fan:8");
        let generic = function(&c.context.registry, "member-b", "light:9");
        until(|| {
            group.snapshot().power() == Some(Power::On)
                && member.snapshot().power() == Some(Power::On)
                && generic.snapshot().power() == Some(Power::On)
                && member.snapshot().lighting().unwrap().mode == Some(LightMode::Temperature)
                && fan.snapshot().power() == Some(Power::On)
        })
        .await;
        let current = c.publication.current();
        let member_key = DeviceKey::new("xiaomi", "member-a");
        let saved = current.configuration.device(&member_key).unwrap();
        assert!(current.derived.published.contains(&member_key));
        assert_eq!(
            current
                .derived
                .visible_functions(&member_key)
                .iter()
                .map(|f| f.id.as_str())
                .collect::<Vec<_>>(),
            ["fan:8"]
        );
        assert!(
            saved
                .functions
                .iter()
                .all(|function| function.endpoint != saved.endpoint)
        );
        assert!(
            group
                .description()
                .capabilities()
                .contains(&Capability::ColorRgb)
        );
        let plan = current.derived.served[&DeviceKey::new("xiaomi", "group-1")]
            .repair
            .as_ref()
            .unwrap();
        assert_eq!(
            plan.members
                .iter()
                .map(|member| (member.device.id.as_str(), member.id.as_str()))
                .collect::<Vec<_>>(),
            [("member-a", "light:2"), ("member-b", "light:9")]
        );
        let action = group
            .begin_light_action(
                LightTarget::rgb(None, Rgb::from_packed(0xaabbcc).unwrap()),
                false,
            )
            .unwrap();
        action.accepted().await.unwrap();
        until(|| {
            let cloud = c.cloud.lock().unwrap();
            let writes = cloud.numeric_writes.iter().flat_map(|(items, _)| items);
            let writes = writes.collect::<Vec<_>>();
            [
                ("member-a", 4, 1, 0xaabbcc),
                ("member-a", 4, 2, 1),
                ("member-b", 9, 4, 0xaabbcc),
            ]
            .iter()
            .all(|(did, siid, piid, value)| {
                writes.iter().any(|item| {
                    item["did"] == *did
                        && item["siid"] == *siid
                        && item["piid"] == *piid
                        && item["value"] == *value
                })
            })
        })
        .await;
        fan.execute(Command::Off).await.unwrap();
        assert!(
            group.light_action_current(action.token()),
            "the fan must not take over the group color action"
        );
        let work = c.context.live.borrow().devices["member-a"]
            .admitted
            .clone()
            .unwrap();
        work.notify_value(MEMBER_COLOR, &json!(0x00ff00), false, false);
        work.notify_value(MEMBER_MODE, &json!(1), false, false);
        c.context.refresh_group_states("member-a");
        let snapshot = group.snapshot();
        assert_eq!(snapshot.power(), Some(Power::On));
        let lighting = snapshot.lighting().unwrap();
        assert_eq!(lighting.brightness_percent, Some(50.0));
        assert_eq!(lighting.color, Rgb::from_packed(0x00ff00));
        assert_eq!(lighting.mode, Some(LightMode::Rgb));
        let writes = c.cloud.lock().unwrap();
        assert!(
            !writes
                .numeric_writes
                .iter()
                .flat_map(|(items, _)| items)
                .any(|item| item["did"] == "group-1" && item["siid"] == 4 && item["piid"] == 1)
        );
    }));
}

#[test]
fn ambiguous_member_stays_visible_and_cannot_supply_group_feedback() {
    let mut ambiguous = generic_rgb();
    let mut second = ambiguous["services"][1].clone();
    second["iid"] = json!(9);
    ambiguous["services"].as_array_mut().unwrap().push(second);
    let rig = group_rig(&[("member-a", "example.rgb", ambiguous)], false);
    block_on(rig.run(|c| async move {
        c.announce();
        until(|| c.context.graph.borrow().rules.contains_key("group-1")).await;
        let key = DeviceKey::new("xiaomi", "member-a");
        let current = c.publication.current();
        assert!(current.derived.published.contains(&key));
        assert_eq!(current.derived.visible_functions(&key).len(), 2);
        assert!(current.derived.rules["group-1"].members.is_empty());
        assert!(current.derived.member_issues["group-1"][0].contains("ambiguous"));
        let group = function(&c.context.registry, "group-1", "light:2");
        assert!(!group.description().supports_rgb());
        assert_eq!(group.snapshot().power(), Some(Power::Unknown));
        let status = crate::terminal::AuthStatus::new(
            c.context.status.borrow().authentication.clone(),
            "/bin/migate".into(),
            "/data".into(),
        );
        let text = crate::terminal::bridge::Context::committed(c.publication.clone(), &status)
            .handle_line("status group-1", 0)
            .await
            .unwrap()
            .unwrap();
        assert!(
            text.contains("member member-a light function is ambiguous"),
            "{text}"
        );
    }));
}

#[test]
fn native_rgb_uses_its_own_interface_even_with_an_unresolved_member() {
    let rig = Rig::new(false, &["one"]);
    let missing_urn = rig
        .data
        .iter()
        .find(|native| native.device().key().id == "one")
        .unwrap()
        .instance_type()
        .to_owned();
    let native = ls2307_instance();
    {
        let mut cloud = rig.cloud.lock().unwrap();
        let mut group = group_record("record", "group-1", &["one"], true);
        group["model"] = json!("devcea.light.ls2307");
        group["spec_type"] = native["type"].clone();
        cloud.catalog_groups = vec![group];
        cloud
            .catalog_instances
            .insert(native["type"].as_str().unwrap().into(), native);
        cloud.instance_status.insert(missing_urn, 500);
        cloud.online.insert("group-1".into());
        for (siid, piid, value) in [
            (2, 1, json!(true)),
            (2, 2, json!(50)),
            (2, 3, json!(3000)),
            (4, 1, json!(0x112233)),
            (4, 2, json!(0)),
        ] {
            cloud
                .raw_values
                .insert(("group-1".into(), siid, piid), value);
        }
    }
    block_on(rig.run(|c| async move {
        c.announce();
        until(|| c.context.graph.borrow().rules.contains_key("group-1")).await;
        let group = function(&c.context.registry, "group-1", "light:2");
        until(|| group.snapshot().power() == Some(Power::On)).await;
        let current = c.publication.current();
        let served = &current.derived.served[&DeviceKey::new("xiaomi", "group-1")];
        assert!(group.description().supports_rgb());
        assert!(served.repair.is_none());
        assert!(served.repair_reason.is_none());
        assert!(current.derived.member_issues["group-1"][0].contains("description is missing"));
        group
            .begin_light_action(
                LightTarget::rgb(None, Rgb::from_packed(0xaabbcc).unwrap()),
                true,
            )
            .unwrap()
            .accepted()
            .await
            .unwrap();
        let cloud = c.cloud.lock().unwrap();
        let writes = cloud
            .numeric_writes
            .iter()
            .flat_map(|(items, _)| items)
            .collect::<Vec<_>>();
        assert!(writes.iter().any(|item| item["did"] == "group-1"
            && item["siid"] == 4
            && item["piid"] == 1
            && item["value"] == 0xaabbcc));
        assert!(writes.iter().all(|item| item["did"] == "group-1"));
    }));
}
